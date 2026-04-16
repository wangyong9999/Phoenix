/*-------------------------------------------------------------------------
 *
 * page_walrecord.h
 *		Page-level WAL record definitions for OrioleDB on Neon.
 *
 * These WAL records encode page-level deltas (like PG nbtxlog.h),
 * enabling PageServer wal-redo to materialize OrioleDB B-tree pages
 * without full tree context.
 *
 * Design notes:
 *   - All records use XLogRegisterBlock() with extent.off as blkno
 *   - extent.off is stable within a checkpoint epoch (COW relocates
 *     pages at checkpoint boundaries, emitting FPIs as new base images)
 *   - Single-page ops use delta; structural ops (SPLIT/MERGE) use FPI
 *   - Coexists with row-level ORIOLEDB_XLOG_CONTAINER (0x00)
 *
 * Copyright (c) 2024-2026, Oriole DB Inc.
 *
 * IDENTIFICATION
 *	  contrib/orioledb/include/btree/page_walrecord.h
 *
 *-------------------------------------------------------------------------
 */
#ifndef PAGE_WALRECORD_H
#define PAGE_WALRECORD_H

#include "orioledb.h"

/*
 * Page-level WAL info bytes for ORIOLEDB_RMGR_ID (129).
 *
 * These share the RMGR with existing record types:
 *   0x00 = ORIOLEDB_XLOG_CONTAINER  (row-level, compute crash recovery)
 *   0x81 = ORIOLEDB_XLOG_PAGE_IMAGE (checkpoint FPI)
 *
 * New page-level delta types use 0x10-0xA0 range.
 */
#define ORIOLEDB_XLOG_PAGE_INIT		0x10	/* new empty page (WILL_INIT) */
#define ORIOLEDB_XLOG_LEAF_INSERT	0x20	/* insert tuple into leaf */
#define ORIOLEDB_XLOG_LEAF_DELETE	0x30	/* mark tuple deleted on leaf */
#define ORIOLEDB_XLOG_LEAF_UPDATE	0x40	/* replace tuple on leaf */
#define ORIOLEDB_XLOG_LEAF_LOCK		0x50	/* update lock flags on tuple */
#define ORIOLEDB_XLOG_COMPACT		0x60	/* page compaction (FPI) */
#define ORIOLEDB_XLOG_SPLIT			0x70	/* page split (2-3 FPIs) */
#define ORIOLEDB_XLOG_MERGE			0x80	/* page merge (FPI+delta) */
/*      ORIOLEDB_XLOG_PAGE_IMAGE	0x81	   (existing — checkpoint FPI) */
#define ORIOLEDB_XLOG_ROOT_SPLIT	0x90	/* root split (3 FPIs) */
#define ORIOLEDB_XLOG_UNDO_APPLY	0xA0	/* undo rollback (FPI) */

/*
 * xl_orioledb_leaf_insert — insert a tuple into a leaf page.
 *
 * Block ref 0: the target leaf page (delta, not FPI).
 * Followed by: BTreeLeafTuphdr (8 bytes) + tuple data (variable).
 *
 * The item_index is the global OffsetNumber within the page (as returned
 * by BTREE_PAGE_LOCATOR_GET_OFFSET). The redo function recovers the
 * full locator via BTREE_PAGE_OFFSET_GET_LOCATOR (page_item_fill_locator)
 * and calls page_locator_insert_item().
 */
typedef struct xl_orioledb_leaf_insert
{
	uint16		item_index;		/* position in page (0-based) */
	uint16		tuple_size;		/* total size of header + tuple data */
	/* BTreeLeafTuphdr + tuple data follows */
} xl_orioledb_leaf_insert;

#define SizeOfOrioleDBLeafInsert	(offsetof(xl_orioledb_leaf_insert, tuple_size) + sizeof(uint16))

/*
 * xl_orioledb_leaf_delete — mark a tuple as deleted on a leaf page.
 *
 * Block ref 0: the target leaf page (delta).
 * Only updates the BTreeLeafTuphdr at the given position.
 */
typedef struct xl_orioledb_leaf_delete
{
	uint16		item_index;		/* position in page */
	uint8		deleted;		/* BTreeLeafTupleDeleted value */
	uint8		pad;
	CommitSeqNo csn;			/* visibility CSN */
	UndoLocation undo_loc;		/* new undo location */
} xl_orioledb_leaf_delete;

#define SizeOfOrioleDBLeafDelete	sizeof(xl_orioledb_leaf_delete)

/*
 * xl_orioledb_leaf_update — replace a tuple on a leaf page.
 *
 * Block ref 0: the target leaf page (delta).
 * If the new tuple is the same size, in-place update.
 * If different size, delete old + insert new at same position.
 *
 * Followed by: BTreeLeafTuphdr + new tuple data.
 */
typedef struct xl_orioledb_leaf_update
{
	uint16		item_index;		/* position in page */
	uint16		old_tuple_size;	/* size of old tuple (for size-change detection) */
	uint16		new_tuple_size;	/* total size of new header + tuple data */
	uint16		pad;
	/* BTreeLeafTuphdr + new tuple data follows */
} xl_orioledb_leaf_update;

#define SizeOfOrioleDBLeafUpdate	sizeof(xl_orioledb_leaf_update)

/*
 * xl_orioledb_leaf_lock — update lock flags on a leaf tuple.
 *
 * Block ref 0: the target leaf page (delta).
 * Only modifies the chainHasLocks bit in BTreeLeafTuphdr.
 */
typedef struct xl_orioledb_leaf_lock
{
	uint16		item_index;		/* position in page */
	uint16		pad;
	CommitSeqNo csn;
	UndoLocation undo_loc;
} xl_orioledb_leaf_lock;

#define SizeOfOrioleDBLeafLock	sizeof(xl_orioledb_leaf_lock)

/*
 * For SPLIT, MERGE, ROOT_SPLIT, COMPACT, UNDO_APPLY:
 *   No delta struct needed — these use REGBUF_FORCE_IMAGE (FPI).
 *   The entire page content is stored in the WAL record as an image.
 *   wal-redo receives the image directly via Value::Image path,
 *   bypassing the redo function entirely.
 *
 * SPLIT uses 2-3 block refs (left FPI + right WILL_INIT FPI + optional parent FPI).
 * MERGE uses 2-3 block refs (left FPI + parent delta/FPI).
 * ROOT_SPLIT uses 3 block refs (all FPIs).
 */

/*
 * xl_orioledb_split — metadata for split operation.
 *
 * All pages are stored as FPIs, so this struct is for logging/debugging only.
 * Block ref 0: left page (FPI)
 * Block ref 1: right page (FPI, WILL_INIT)
 * Block ref 2: parent page (FPI, optional — only if parent downlink updated)
 */
typedef struct xl_orioledb_split
{
	uint16		level;			/* tree level of split pages */
	bool		is_leaf;		/* true if splitting leaf pages */
	bool		is_rootsplit;	/* true if this also triggers root split */
} xl_orioledb_split;

#define SizeOfOrioleDBSplit	sizeof(xl_orioledb_split)

/*
 * Helper: check if a WAL info byte is a page-level record (vs container/FPI).
 * Uses explicit enumeration to avoid matching undefined codes in gaps.
 */
static inline bool
IsOrioleDBPageLevelWal(uint8 info)
{
	uint8 masked = info & ~XLR_INFO_MASK;
	switch (masked)
	{
		case ORIOLEDB_XLOG_PAGE_INIT:
		case ORIOLEDB_XLOG_LEAF_INSERT:
		case ORIOLEDB_XLOG_LEAF_DELETE:
		case ORIOLEDB_XLOG_LEAF_UPDATE:
		case ORIOLEDB_XLOG_LEAF_LOCK:
		case ORIOLEDB_XLOG_COMPACT:
		case ORIOLEDB_XLOG_SPLIT:
		case ORIOLEDB_XLOG_MERGE:
		case ORIOLEDB_XLOG_ROOT_SPLIT:
		case ORIOLEDB_XLOG_UNDO_APPLY:
			return true;
		default:
			return false;
	}
}

/*
 * Helper: check if a page-level record uses FPI (no redo needed).
 */
static inline bool
IsOrioleDBPageFPI(uint8 info)
{
	uint8 masked = info & ~XLR_INFO_MASK;
	return masked == ORIOLEDB_XLOG_PAGE_INIT ||
		   masked == ORIOLEDB_XLOG_COMPACT ||
		   masked == ORIOLEDB_XLOG_SPLIT ||
		   masked == ORIOLEDB_XLOG_MERGE ||
		   masked == ORIOLEDB_XLOG_PAGE_IMAGE ||
		   masked == ORIOLEDB_XLOG_ROOT_SPLIT ||
		   masked == ORIOLEDB_XLOG_UNDO_APPLY;
}

#endif							/* PAGE_WALRECORD_H */
