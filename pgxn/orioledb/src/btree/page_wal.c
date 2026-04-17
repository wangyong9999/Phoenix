/*-------------------------------------------------------------------------
 *
 * page_wal.c
 *		Page-level WAL emission helpers for OrioleDB on Neon.
 *
 * These functions support emitting page-level delta WAL records that
 * enable Neon PageServer's wal-redo to materialize OrioleDB B-tree
 * pages without full tree context.
 *
 * Copyright (c) 2024-2026, Oriole DB Inc.
 *
 * IDENTIFICATION
 *	  contrib/orioledb/src/btree/page_wal.c
 *
 *-------------------------------------------------------------------------
 */

#include "postgres.h"

#include "orioledb.h"

#include "access/xlog.h"
#include "access/xloginsert.h"
#include "catalog/pg_tablespace_d.h"
#include "storage/procnumber.h"
#include "storage/smgr.h"

#include "btree/btree.h"
#include "btree/io.h"
#include "btree/page_contents.h"
#include "btree/page_wal.h"
#include "btree/page_walrecord.h"
#include "catalog/storage_xlog.h"
#include "storage/ipc.h"
#include "utils/hsearch.h"

/*
 * Check if page-level WAL emission is enabled.
 *
 * Only enabled in Neon mode (smgr_hook present) and when WAL insertion
 * is allowed (not during recovery, not during startup).
 */
bool
orioledb_page_wal_enabled(void)
{
	return smgr_hook != NULL && XLogInsertAllowed();
}

/*
 * Track which synthetic relations have been registered with PageServer
 * via XLOG_SMGR_CREATE. We use a simple static hash to avoid emitting
 * duplicate registration WAL records within the same backend lifetime.
 */
typedef struct
{
	Oid			dbOid;
	Oid			relNumber;
	ForkNumber	forkNum;
} SyntheticRelKey;

static HTAB *registered_rels = NULL;

/*
 * Emit XLOG_SMGR_CREATE for a synthetic relation if not already registered.
 *
 * OrioleDB's synthetic relations (B-tree data, map files, control file,
 * undo/xidmap) are never created via DDL, so PageServer has no directory
 * entry for them. Without registration, smgrexists returns false and
 * all GetPage requests fail.
 *
 * This function emits a standard XLOG_SMGR_CREATE WAL record, which
 * PageServer processes via put_rel_creation() to create the directory
 * entry. Idempotent — duplicate creates are harmless.
 */
static void
orioledb_ensure_rel_registered(Oid dbOid, Oid relNumber, ForkNumber forkNum)
{
	SyntheticRelKey key;
	bool		found;

	if (!orioledb_page_wal_enabled())
		return;

	/* Don't emit WAL during shutdown — causes PANIC */
	if (proc_exit_inprogress)
		return;

	/* Initialize hash table on first call */
	if (registered_rels == NULL)
	{
		HASHCTL		ctl;

		memset(&ctl, 0, sizeof(ctl));
		ctl.keysize = sizeof(SyntheticRelKey);
		ctl.entrysize = sizeof(SyntheticRelKey);
		ctl.hcxt = TopMemoryContext;
		registered_rels = hash_create("OrioleDB registered rels",
									  64, &ctl,
									  HASH_ELEM | HASH_BLOBS | HASH_CONTEXT);
	}

	key.dbOid = dbOid;
	key.relNumber = relNumber;
	key.forkNum = forkNum;

	hash_search(registered_rels, &key, HASH_ENTER, &found);
	if (found)
		return;					/* already registered this session */

	/* Emit XLOG_SMGR_CREATE — tells PageServer this relation exists */
	{
		xl_smgr_create xlrec;

		xlrec.rlocator.spcOid = DEFAULTTABLESPACE_OID;
		xlrec.rlocator.dbOid = dbOid;
		xlrec.rlocator.relNumber = relNumber;
		xlrec.forkNum = forkNum;

		XLogBeginInsert();
		XLogRegisterData((char *) &xlrec, sizeof(xlrec));
		XLogInsert(RM_SMGR_ID, XLOG_SMGR_CREATE);
	}

	elog(LOG, "OrioleDB: registered synthetic relation (%u, %u) fork=%d "
		 "with PageServer via XLOG_SMGR_CREATE",
		 dbOid, relNumber, forkNum);
}

/*
 * Get the stable block number for a page.
 *
 * Uses the page's current extent.off as the BlockNumber. This is stable
 * within a checkpoint epoch — COW checkpoint may relocate the page,
 * at which point a new FPI is emitted as the base image.
 *
 * Returns InvalidBlockNumber if the page has no disk extent yet
 * (newly created, not yet checkpointed).
 */
BlockNumber
orioledb_page_get_blkno(OInMemoryBlkno blkno)
{
	OrioleDBPageDesc *page_desc = O_GET_IN_MEMORY_PAGEDESC(blkno);

	if (!FileExtentIsValid(page_desc->fileExtent))
		return InvalidBlockNumber;

	/*
	 * extent.off is 48-bit; BlockNumber is 32-bit. For non-compressed pages,
	 * 2^32 * 8KB = 32TB which is well above Neon's practical limit.
	 * Assert to catch overflow if OrioleDB data ever exceeds this.
	 */
	Assert(page_desc->fileExtent.off <= (uint64) MaxBlockNumber);

	return (BlockNumber) page_desc->fileExtent.off;
}

/*
 * Register a B-tree's synthetic relations with PageServer.
 * Emits SMGR_CREATE for both MAIN (data pages) and INIT (map files) forks.
 *
 * MUST be called outside CRIT_SECTION (hash_create allocates memory).
 * Call from checkpointable_tree_init or btree_open_smgr, NOT from
 * page-level WAL emission functions (which run inside CRIT_SECTION).
 */
void
orioledb_page_wal_register_tree(BTreeDescr *desc)
{
	orioledb_ensure_rel_registered(desc->oids.datoid,
								   desc->oids.relnode,
								   MAIN_FORKNUM);
	orioledb_ensure_rel_registered(desc->oids.datoid,
								   desc->oids.relnode,
								   INIT_FORKNUM);
}

/*
 * Fill a RelFileLocator for page-level WAL block references.
 *
 * Maps OrioleDB's (datoid, relnode) to PG's RelFileLocator:
 *   spcOid    = DEFAULTTABLESPACE_OID
 *   dbOid     = desc->oids.datoid
 *   relNumber = desc->oids.relnode
 */
void
orioledb_page_wal_rlocator(BTreeDescr *desc, RelFileLocator *rlocator)
{
	rlocator->spcOid = DEFAULTTABLESPACE_OID;
	rlocator->dbOid = desc->oids.datoid;
	rlocator->relNumber = desc->oids.relnode;
}

/*
 * Emit a page-level FPI for a modified page.
 *
 * Used for structural operations (SPLIT, MERGE, COMPACT) where the
 * entire page is reorganized and a delta would be as large as the FPI.
 *
 * Also used as a fallback when a page has no stable extent yet.
 */
void
orioledb_page_wal_emit_fpi(BTreeDescr *desc, OInMemoryBlkno blkno,
						   uint8 info)
{
	Page		page;
	RelFileLocator rlocator;
	BlockNumber disk_blkno;

	if (!orioledb_page_wal_enabled())
		return;

	/* NOTE: registration is done outside CRIT_SECTION, not here.
	 * See orioledb_page_wal_register_tree(). */

	disk_blkno = orioledb_page_get_blkno(blkno);
	if (disk_blkno == InvalidBlockNumber)
	{
		/* Pre-allocate extent so we can emit WAL with a stable blkno */
		orioledb_page_ensure_extent(desc, blkno);
		disk_blkno = orioledb_page_get_blkno(blkno);
		if (disk_blkno == InvalidBlockNumber)
			return;				/* still no extent — skip */
	}

	page = O_GET_IN_MEMORY_PAGE(blkno);
	orioledb_page_wal_rlocator(desc, &rlocator);

	XLogBeginInsert();
	XLogRegisterBlock(0, &rlocator, MAIN_FORKNUM, disk_blkno,
					  page, REGBUF_FORCE_IMAGE | REGBUF_WILL_INIT);
	XLogInsert(ORIOLEDB_RMGR_ID, info);
}

/*
 * Emit a LEAF_INSERT page-level WAL record.
 *
 * Called after page_locator_insert_item() and tuple header setup.
 *
 * Parameters:
 *   desc       - B-tree descriptor (for relation OIDs)
 *   blkno      - in-memory page number
 *   item_index - 0-based item position in the page
 *   tuphdr     - leaf tuple header (BTreeLeafTuphdr)
 *   tuple_data - tuple payload
 *   tuple_len  - length of tuple payload (excluding header)
 */
void
orioledb_page_wal_leaf_insert(BTreeDescr *desc, OInMemoryBlkno blkno,
							  uint16 item_index,
							  BTreeLeafTuphdr *tuphdr,
							  Pointer tuple_data, uint16 tuple_len)
{
	Page		page;
	RelFileLocator rlocator;
	BlockNumber disk_blkno;
	xl_orioledb_leaf_insert xlrec;

	if (!orioledb_page_wal_enabled())
		return;

	/* NOTE: registration done outside CRIT_SECTION via
	 * orioledb_page_wal_register_tree(). */

	disk_blkno = orioledb_page_get_blkno(blkno);
	if (disk_blkno == InvalidBlockNumber)
	{
		/* No extent yet — pre-allocate one so we have a stable blkno */
		orioledb_page_ensure_extent(desc, blkno);
		disk_blkno = orioledb_page_get_blkno(blkno);
		if (disk_blkno == InvalidBlockNumber)
			return;				/* still no extent — skip */
	}

	page = O_GET_IN_MEMORY_PAGE(blkno);
	orioledb_page_wal_rlocator(desc, &rlocator);

	xlrec.item_index = item_index;
	xlrec.tuple_size = sizeof(BTreeLeafTuphdr) + tuple_len;

	XLogBeginInsert();
	XLogRegisterBlock(0, &rlocator, MAIN_FORKNUM, disk_blkno, page, 0);
	XLogRegisterBufData(0, (char *) &xlrec, SizeOfOrioleDBLeafInsert);
	XLogRegisterBufData(0, (char *) tuphdr, sizeof(BTreeLeafTuphdr));
	XLogRegisterBufData(0, tuple_data, tuple_len);
	XLogInsert(ORIOLEDB_RMGR_ID, ORIOLEDB_XLOG_LEAF_INSERT);
}

/*
 * Emit a LEAF_DELETE page-level WAL record.
 *
 * Delete only modifies the BTreeLeafTuphdr (deleted flag, undoLocation,
 * xactInfo, chainHasLocks). No tuple data changes.
 */
void
orioledb_page_wal_leaf_delete(BTreeDescr *desc, OInMemoryBlkno blkno,
							  uint16 item_index,
							  BTreeLeafTuphdr *tuphdr)
{
	Page		page;
	RelFileLocator rlocator;
	BlockNumber disk_blkno;
	xl_orioledb_leaf_delete xlrec;

	if (!orioledb_page_wal_enabled())
		return;

	disk_blkno = orioledb_page_get_blkno(blkno);
	if (disk_blkno == InvalidBlockNumber)
		return;

	page = O_GET_IN_MEMORY_PAGE(blkno);
	orioledb_page_wal_rlocator(desc, &rlocator);

	xlrec.item_index = item_index;
	xlrec.deleted = tuphdr->deleted;
	xlrec.pad = 0;
	xlrec.csn = 0;			/* CSN is in page header, not tuple header */
	xlrec.undo_loc = tuphdr->undoLocation;

	XLogBeginInsert();
	XLogRegisterBlock(0, &rlocator, MAIN_FORKNUM, disk_blkno, page, 0);
	XLogRegisterBufData(0, (char *) &xlrec, SizeOfOrioleDBLeafDelete);
	/* Store the full tuphdr for precise state restoration in redo */
	XLogRegisterBufData(0, (char *) tuphdr, sizeof(BTreeLeafTuphdr));
	XLogInsert(ORIOLEDB_RMGR_ID, ORIOLEDB_XLOG_LEAF_DELETE);
}

/*
 * Emit a LEAF_UPDATE page-level WAL record.
 *
 * Update replaces the tuple header and data at the given position.
 * If the new tuple is a different size, the page structure changes
 * (handled by page_locator_resize_item in the original code).
 */
void
orioledb_page_wal_leaf_update(BTreeDescr *desc, OInMemoryBlkno blkno,
							  uint16 item_index,
							  BTreeLeafTuphdr *tuphdr,
							  Pointer tuple_data, uint16 tuple_len,
							  uint16 old_tuple_size)
{
	Page		page;
	RelFileLocator rlocator;
	BlockNumber disk_blkno;
	xl_orioledb_leaf_update xlrec;

	if (!orioledb_page_wal_enabled())
		return;

	disk_blkno = orioledb_page_get_blkno(blkno);
	if (disk_blkno == InvalidBlockNumber)
		return;

	page = O_GET_IN_MEMORY_PAGE(blkno);
	orioledb_page_wal_rlocator(desc, &rlocator);

	xlrec.item_index = item_index;
	xlrec.old_tuple_size = old_tuple_size;
	xlrec.new_tuple_size = sizeof(BTreeLeafTuphdr) + tuple_len;
	xlrec.pad = 0;

	XLogBeginInsert();
	XLogRegisterBlock(0, &rlocator, MAIN_FORKNUM, disk_blkno, page, 0);
	XLogRegisterBufData(0, (char *) &xlrec, SizeOfOrioleDBLeafUpdate);
	XLogRegisterBufData(0, (char *) tuphdr, sizeof(BTreeLeafTuphdr));
	XLogRegisterBufData(0, tuple_data, tuple_len);
	XLogInsert(ORIOLEDB_RMGR_ID, ORIOLEDB_XLOG_LEAF_UPDATE);
}

/*
 * Ensure a page has a disk extent allocated.
 *
 * For Neon page-level WAL, we need a stable blkno (extent.off) to identify
 * pages in WAL records. Newly created pages (e.g., split right page) have
 * fileExtent = Invalid. This function pre-allocates an extent by atomically
 * incrementing the tree's datafileLength counter.
 *
 * The allocated offset is unique (monotonically increasing) and won't
 * conflict with checkpoint COW allocation:
 *   - checkpointNum is set to current → checkpoint sees less_num=false
 *   - page is rewritten in-place (no COW relocation) at next checkpoint
 *   - subsequent checkpoints (N+1) will COW-relocate normally
 */
void
orioledb_page_ensure_extent(BTreeDescr *desc, OInMemoryBlkno blkno)
{
	OrioleDBPageDesc *page_desc = O_GET_IN_MEMORY_PAGEDESC(blkno);
	BTreeMetaPage *metaPage;
	BTreePageHeader *header;
	uint32		chkpNum;

	if (FileExtentIsValid(page_desc->fileExtent))
		return;					/* already has extent */

	if (!orioledb_page_wal_enabled())
		return;					/* not in Neon mode */

	metaPage = BTREE_GET_META(desc);
	header = (BTreePageHeader *) O_GET_IN_MEMORY_PAGE(blkno);

	/* Use the current checkpoint's data file length slot */
	chkpNum = header->o_header.checkpointNum;
	if (chkpNum == 0)
	{
		/*
		 * New page with checkpointNum=0. Use slot 0 (even checkpoints).
		 * The next checkpoint will see less_num=true and COW-relocate.
		 */
		chkpNum = 0;
	}

	page_desc->fileExtent.off =
		pg_atomic_fetch_add_u64(&metaPage->datafileLength[chkpNum % 2], 1);
	page_desc->fileExtent.len = 1;

	elog(DEBUG1, "OrioleDB: pre-allocated extent off=%lu for page %u (Neon WAL)",
		 (unsigned long) page_desc->fileExtent.off, blkno);
}

/*
 * Emit SPLIT FPIs — two pages in a single WAL record.
 *
 * Block ref 0: left page (FPI, existing page — has extent)
 * Block ref 1: right page (FPI, new page — extent pre-allocated)
 *
 * Both pages are fully reorganized by perform_page_split, so FPI
 * is the natural strategy (delta would be same size).
 */
void
orioledb_page_wal_split(BTreeDescr *desc,
						OInMemoryBlkno left_blkno,
						OInMemoryBlkno right_blkno)
{
	Page		left_page, right_page;
	RelFileLocator rlocator;
	BlockNumber left_disk, right_disk;

	if (!orioledb_page_wal_enabled())
		return;

	/* Ensure right page has an extent */
	orioledb_page_ensure_extent(desc, right_blkno);

	left_disk = orioledb_page_get_blkno(left_blkno);
	right_disk = orioledb_page_get_blkno(right_blkno);

	if (left_disk == InvalidBlockNumber || right_disk == InvalidBlockNumber)
		return;

	left_page = O_GET_IN_MEMORY_PAGE(left_blkno);
	right_page = O_GET_IN_MEMORY_PAGE(right_blkno);
	orioledb_page_wal_rlocator(desc, &rlocator);

	XLogBeginInsert();
	XLogRegisterBlock(0, &rlocator, MAIN_FORKNUM, left_disk,
					  left_page, REGBUF_FORCE_IMAGE | REGBUF_WILL_INIT);
	XLogRegisterBlock(1, &rlocator, MAIN_FORKNUM, right_disk,
					  right_page, REGBUF_FORCE_IMAGE | REGBUF_WILL_INIT);
	XLogInsert(ORIOLEDB_RMGR_ID, ORIOLEDB_XLOG_SPLIT);
}

/*
 * Emit MERGE FPIs — left page (absorbed) + parent page (downlink removed).
 *
 * Block ref 0: left page (FPI — absorbed right page's items)
 * Block ref 1: parent page (FPI — removed right page's downlink)
 */
void
orioledb_page_wal_merge(BTreeDescr *desc,
						OInMemoryBlkno left_blkno,
						OInMemoryBlkno parent_blkno)
{
	Page		left_page, parent_page;
	RelFileLocator rlocator;
	BlockNumber left_disk, parent_disk;

	if (!orioledb_page_wal_enabled())
		return;

	left_disk = orioledb_page_get_blkno(left_blkno);
	parent_disk = orioledb_page_get_blkno(parent_blkno);

	if (left_disk == InvalidBlockNumber || parent_disk == InvalidBlockNumber)
		return;

	left_page = O_GET_IN_MEMORY_PAGE(left_blkno);
	parent_page = O_GET_IN_MEMORY_PAGE(parent_blkno);
	orioledb_page_wal_rlocator(desc, &rlocator);

	XLogBeginInsert();
	XLogRegisterBlock(0, &rlocator, MAIN_FORKNUM, left_disk,
					  left_page, REGBUF_FORCE_IMAGE | REGBUF_WILL_INIT);
	XLogRegisterBlock(1, &rlocator, MAIN_FORKNUM, parent_disk,
					  parent_page, REGBUF_FORCE_IMAGE | REGBUF_WILL_INIT);
	XLogInsert(ORIOLEDB_RMGR_ID, ORIOLEDB_XLOG_MERGE);
}
