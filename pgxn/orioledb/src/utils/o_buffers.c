/*-------------------------------------------------------------------------
 *
 * o_buffers.c
 * 		Buffering layer for file access.
 *
 * Copyright (c) 2021-2026, Oriole DB Inc.
 * Copyright (c) 2025-2026, Supabase Inc.
 *
 * IDENTIFICATION
 *	  contrib/orioledb/src/utils/o_buffers.c
 *
 *-------------------------------------------------------------------------
 */
#include "postgres.h"

#include <unistd.h>

#include "orioledb.h"

#include "access/xlog.h"
#include "access/xloginsert.h"
#include "btree/btree.h"
#include "btree/io.h"
#include "catalog/pg_tablespace_d.h"
#include "checkpoint/control.h"		/* ORIOLEDB_CONTROL_FILE_OID for StaticAssert */
#include "miscadmin.h"
#include "storage/procnumber.h"
#include "storage/smgr.h"
#include "utils/o_buffers.h"

#include "pgstat.h"

/*
 * Plan B: OrioleDB log-like persistence for undo / xidmap buffers.
 *
 * OBuffers backs append-only logs (undo segments, xidmap) that OrioleDB
 * normally writes to `orioledb_undo/` and similar local directories. For a
 * stateless compute those directories vanish on restart, so we shadow the
 * local write with a full-page image (REGBUF_FORCE_IMAGE) WAL record aimed at
 * a synthetic relation per OBuffersDesc. PageServer stores the image and
 * serves it back through smgr when the local file is missing.
 *
 * These logs are genuinely cluster-global (single static OBuffersDesc in
 * shmem, one set of undo/xidmap for the whole PG cluster), so keying them
 * with dbOid = 0 alongside SLRU-like cluster state is architecturally
 * correct. The concern that Phase 6.6.1 closes is not a dbOid collision
 * but a relNumber collision: the OIDs below must not overlap with any
 * relfilenode that PG's user-relfilenode allocator could produce.
 *
 * Mapping: each OBuffersDesc gets a distinct relNumber in the
 * ORIOLEDB_OBUF_RELNODE_BASE reserved range so different log streams
 * (undo row / undo page / undo system / xidmap) never collide.
 *
 * ORIOLEDB_OBUF_RELNODE_BASE is placed in the top 256 values of the
 * 32-bit RelFileNumber space, together with ORIOLEDB_CONTROL_FILE_OID
 * (see include/checkpoint/control.h). This range is unreachable by PG's
 * user-relfilenode allocator in any practical workload (would require
 * ~4.29 billion allocations in a single cluster). The StaticAssert below
 * enforces non-overlap with ORIOLEDB_CONTROL_FILE_OID.
 *
 * ORIOLEDB_OBUF_MAX_LOGS caps the number of Plan B log streams that can
 * coexist; each stream consumes ORIOLEDB_OBUF_TAGS_PER_LOG relfilenodes.
 * Bump only after re-checking the StaticAssert.
 */
#define ORIOLEDB_OBUF_RELNODE_BASE  0xFFFFFF00u
#define ORIOLEDB_OBUF_TAGS_PER_LOG  16
#define ORIOLEDB_OBUF_MAX_LOGS      14	/* 14 * 16 = 224; base + 224 = 0xFFFFFFE0, below control oid */

/*
 * Top relnode actually emitted by obuffers_relnode() when planBLogId
 * is at its runtime ceiling (ORIOLEDB_OBUF_MAX_LOGS) and the tag hash
 * lands on the last slot (ORIOLEDB_OBUF_TAGS_PER_LOG - 1). This is the
 * concrete upper bound the StaticAssert must clear.
 */
#define ORIOLEDB_OBUF_MAX_RELNODE \
	(ORIOLEDB_OBUF_RELNODE_BASE + \
	 (uint32) ORIOLEDB_OBUF_MAX_LOGS * ORIOLEDB_OBUF_TAGS_PER_LOG + \
	 (ORIOLEDB_OBUF_TAGS_PER_LOG - 1))

StaticAssertDecl(ORIOLEDB_OBUF_RELNODE_BASE >= 0xFFFFFF00u &&
				 ORIOLEDB_OBUF_MAX_RELNODE < ORIOLEDB_CONTROL_FILE_OID,
				 "ORIOLEDB_OBUF reservation window must live in the "
				 "reserved top-256 synthetic OID range and must not "
				 "overlap ORIOLEDB_CONTROL_FILE_OID at its highest slot");

static uint32
obuffers_relnode(OBuffersDesc *desc, uint32 tag)
{
	/*
	 * Stable across restarts: driven by the user-supplied planBLogId and the
	 * tag (segment family within the log). A log must declare planBLogId != 0
	 * to participate in Plan B mirroring.
	 *
	 * planBLogId must stay within [1, ORIOLEDB_OBUF_MAX_LOGS]; a value >=
	 * ORIOLEDB_OBUF_MAX_LOGS would push the computed relnode past the
	 * control-file OID and silently collide with it. Enforced at runtime
	 * (planBLogId is a struct field, so the StaticAssert at file scope can
	 * only bound the *window*, not individual values).
	 */
	Assert(desc->planBLogId != 0);
	Assert(desc->planBLogId <= ORIOLEDB_OBUF_MAX_LOGS);
	return ORIOLEDB_OBUF_RELNODE_BASE +
		(uint32) desc->planBLogId * ORIOLEDB_OBUF_TAGS_PER_LOG +
		(tag & (ORIOLEDB_OBUF_TAGS_PER_LOG - 1));
}

static bool
obuffers_planb_enabled(OBuffersDesc *desc)
{
	return smgr_hook != NULL
		&& desc->planBLogId != 0
		&& !RecoveryInProgress()
		&& XLogInsertAllowed()
		&& !AmStartupProcess();
}

#define O_BUFFERS_PER_GROUP 4

struct OBuffersMeta
{
	int			groupCtlTrancheId;
	int			bufferCtlTrancheId;
};

typedef struct
{
	LWLock		bufferCtlLock;
	int64		blockNum;
	int64		shadowBlockNum;
	uint32		tag;
	uint32		shadowTag;
	uint32		usageCount;
	bool		dirty;
	char		data[ORIOLEDB_BLCKSZ];
} OBuffer;

struct OBuffersGroup
{
	LWLock		groupCtlLock;
	OBuffer		buffers[O_BUFFERS_PER_GROUP];
};

Size
o_buffers_shmem_needs(OBuffersDesc *desc)
{
	desc->groupsCount = (desc->buffersCount + O_BUFFERS_PER_GROUP - 1) / O_BUFFERS_PER_GROUP;

	return add_size(CACHELINEALIGN(sizeof(OBuffersMeta)),
					CACHELINEALIGN(mul_size(sizeof(OBuffersGroup), desc->groupsCount)));
}

void
o_buffers_shmem_init(OBuffersDesc *desc, void *buf, bool found)
{
	Pointer		ptr = buf;

	desc->metaPageBlkno = (OBuffersMeta *) ptr;
	ptr += CACHELINEALIGN(sizeof(OBuffersMeta));

	desc->groups = (OBuffersGroup *) ptr;
	desc->groupsCount = (desc->buffersCount + O_BUFFERS_PER_GROUP - 1) / O_BUFFERS_PER_GROUP;
	desc->curFile = -1;

	Assert((desc->singleFileSize % ORIOLEDB_BLCKSZ) == 0);

	if (!found)
	{
		uint32		i,
					j;

		desc->metaPageBlkno->groupCtlTrancheId = LWLockNewTrancheId();
		desc->metaPageBlkno->bufferCtlTrancheId = LWLockNewTrancheId();

		for (i = 0; i < desc->groupsCount; i++)
		{
			OBuffersGroup *group = &desc->groups[i];

			LWLockInitialize(&group->groupCtlLock,
							 desc->metaPageBlkno->groupCtlTrancheId);
			for (j = 0; j < O_BUFFERS_PER_GROUP; j++)
			{
				OBuffer    *buffer = &group->buffers[j];

				LWLockInitialize(&buffer->bufferCtlLock,
								 desc->metaPageBlkno->bufferCtlTrancheId);
				buffer->blockNum = -1;
				buffer->usageCount = 0;
				buffer->dirty = false;
				buffer->tag = 0;
			}
		}
	}
	LWLockRegisterTranche(desc->metaPageBlkno->groupCtlTrancheId,
						  desc->groupCtlTrancheName);
	LWLockRegisterTranche(desc->metaPageBlkno->bufferCtlTrancheId,
						  desc->bufferCtlTrancheName);
}

/*
 * Open a buffer file.  When create is true, the file is created if it
 * doesn't exist (O_CREAT) and failure is always PANIC.  When create is
 * false, a missing file (ENOENT) returns false instead of panicking.
 */
static bool
open_file(OBuffersDesc *desc, uint32 tag, uint64 fileNum, bool create)
{
	int			flags;

	Assert(OBuffersMaxTagIsValid(tag));

	if (desc->curFile >= 0 &&
		desc->curFileNum == fileNum &&
		desc->curFileTag == tag)
		return true;

	if (desc->curFile >= 0)
		FileClose(desc->curFile);

	pg_snprintf(desc->curFileName, MAXPGPATH,
				desc->filenameTemplate[tag],
				(uint32) (fileNum >> 32),
				(uint32) fileNum);
	flags = O_RDWR | PG_BINARY | (create ? O_CREAT : 0);
	desc->curFile = PathNameOpenFile(desc->curFileName, flags);
	desc->curFileNum = fileNum;
	desc->curFileTag = tag;
	if (desc->curFile < 0)
	{
		if (!create && errno == ENOENT)
			return false;
		ereport(PANIC, (errcode_for_file_access(),
						errmsg("could not open file %s: %m", desc->curFileName)));
	}
	return true;
}

static void
unlink_file(OBuffersDesc *desc, uint32 tag, uint64 fileNum)
{
	static char fileNameToUnlink[MAXPGPATH];

	Assert(OBuffersMaxTagIsValid(tag));

	pg_snprintf(fileNameToUnlink, MAXPGPATH,
				desc->filenameTemplate[tag],
				(uint32) (fileNum >> 32),
				(uint32) fileNum);

	(void) unlink(fileNameToUnlink);
}

static void
write_buffer_data(OBuffersDesc *desc, char *data, uint32 tag, uint64 blockNum)
{
	int			result;
	bool		wal_emitted = false;

	Assert(OBuffersMaxTagIsValid(tag));

	/*
	 * Phase 6.6.2 — WAL-then-FS ordering discipline.
	 *
	 * Emit the Plan B WAL record BEFORE touching the local file. Under
	 * Log-is-Data the local file is a compute-local cache, and a crash
	 * that happens between these two steps must not leave the local file
	 * in a state that wasn't yet described in WAL — on stateless restart
	 * the local file is gone, WAL replay is what the next compute sees.
	 *
	 * Persistence layering for undo/xidmap data (audited 2026-04-18, see
	 * docs/BASEBACKUP_AUDIT.md for the basebackup side):
	 *
	 *   1. Row-level WAL via ORIOLEDB_XLOG_CONTAINER (recovery/wal.c:980)
	 *      is the *source of truth* for every undo / xidmap byte. Backends
	 *      emit CONTAINER records at transaction time, before commit.
	 *      Recovery can reconstruct every live undo record by replaying
	 *      CONTAINER records from the redo start LSN forward.
	 *
	 *   2. Plan B FPI (emitted here) is a *recovery accelerator*: it
	 *      publishes a point-in-time image of the buffer so that
	 *      stateless restart does not have to replay CONTAINER records
	 *      all the way back to the dawn of the log. Without a fresh FPI,
	 *      recovery still succeeds as long as CONTAINER records covering
	 *      the range are retained; with a fresh FPI, recovery starts
	 *      later and is cheaper.
	 *
	 * checkpoint_is_shutdown guard (preserved): a late XLogInsert while
	 * PG is tearing down WAL insertion triggers the "concurrent
	 * write-ahead log activity while database system is shutting down"
	 * PANIC. The prior regular checkpoint's FPIs plus CONTAINER records
	 * emitted before shutdown collectively cover the data; the
	 * end-of-recovery checkpoint on the next stateless restart will emit
	 * fresh FPIs. Skipping is safe because persistence does not rely on
	 * Plan B alone — this guard was rediscovered in Phase 6.6.2 review
	 * and must not be removed without also severing the CONTAINER-record
	 * dependency.
	 */
	extern bool checkpoint_is_shutdown;
	if (obuffers_planb_enabled(desc) && !checkpoint_is_shutdown)
	{
		RelFileLocator rlocator;
		char		page[BLCKSZ];

		rlocator.spcOid = DEFAULTTABLESPACE_OID;
		rlocator.dbOid = 0;
		rlocator.relNumber = obuffers_relnode(desc, tag);

		/*
		 * OrioleDB block size may differ from PG BLCKSZ; pad the PG block
		 * to make the decoder happy. For undo the two are equal today.
		 */
		memset(page, 0, BLCKSZ);
		memcpy(page, data, Min(ORIOLEDB_BLCKSZ, BLCKSZ));

		XLogBeginInsert();
		XLogRegisterBlock(0, &rlocator, MAIN_FORKNUM,
						  (BlockNumber) blockNum,
						  page, REGBUF_FORCE_IMAGE | REGBUF_WILL_INIT);
		(void) XLogInsert(ORIOLEDB_RMGR_ID, ORIOLEDB_XLOG_PAGE_IMAGE);
		wal_emitted = true;
	}

	/*
	 * Ordering invariant: once Plan B is active and we are NOT in the
	 * shutdown carve-out, the WAL record must have been emitted before
	 * the local cache refresh below. Written as an assert (not a runtime
	 * check) because all gating conditions collapse deterministically
	 * from the branch taken above; this is a tripwire against future
	 * edits that reshuffle the branch without updating the invariant.
	 */
	Assert(desc->planBLogId == 0 || smgr_hook == NULL ||
		   RecoveryInProgress() || !XLogInsertAllowed() ||
		   AmStartupProcess() || checkpoint_is_shutdown || wal_emitted);

	open_file(desc, tag, blockNum / (desc->singleFileSize / ORIOLEDB_BLCKSZ), true);
	result = OFileWrite(desc->curFile, data, ORIOLEDB_BLCKSZ,
						(blockNum * ORIOLEDB_BLCKSZ) % desc->singleFileSize,
						WAIT_EVENT_SLRU_WRITE);
	if (result != ORIOLEDB_BLCKSZ)
		ereport(PANIC, (errcode_for_file_access(),
						errmsg("could not write buffer to file %s: %m", desc->curFileName)));
}

static void
write_buffer(OBuffersDesc *desc, OBuffer *buffer)
{
	write_buffer_data(desc, buffer->data, buffer->tag, buffer->blockNum);
}

/*
 * Read a buffer from disk.  When missing_ok is true, a missing file
 * returns false (buffer zeroed) instead of panicking.
 *
 * Plan B: if the local file is absent / too short and the compute is running
 * under Neon, fall back to the PageServer-hosted FPI mirror for this log.
 */
static bool
read_buffer_planb_fallback(OBuffersDesc *desc, OBuffer *buffer)
{
	RelFileLocator rlocator;
	SMgrRelation reln;
	char		page[BLCKSZ];

	if (smgr_hook == NULL
		|| desc->planBLogId == 0
		|| RecoveryInProgress()
		|| !IsNormalProcessingMode()
		|| AmStartupProcess())
		return false;

	rlocator.spcOid = DEFAULTTABLESPACE_OID;
	rlocator.dbOid = 0;
	rlocator.relNumber = obuffers_relnode(desc, buffer->tag);
	reln = smgropen(rlocator, INVALID_PROC_NUMBER, RELPERSISTENCE_PERMANENT);

	if (!smgrexists(reln, MAIN_FORKNUM))
		return false;
	if ((BlockNumber) buffer->blockNum >= smgrnblocks(reln, MAIN_FORKNUM))
		return false;

	smgrread(reln, MAIN_FORKNUM, (BlockNumber) buffer->blockNum, page);
	memcpy(buffer->data, page, Min(ORIOLEDB_BLCKSZ, BLCKSZ));
	if (ORIOLEDB_BLCKSZ > BLCKSZ)
		memset(&buffer->data[BLCKSZ], 0, ORIOLEDB_BLCKSZ - BLCKSZ);
	return true;
}

static bool
read_buffer(OBuffersDesc *desc, OBuffer *buffer, bool missing_ok)
{
	int			result;

	if (!open_file(desc, buffer->tag,
				   buffer->blockNum / (desc->singleFileSize / ORIOLEDB_BLCKSZ),
				   !missing_ok))
	{
		/* Local file missing — try PageServer mirror before giving up. */
		if (read_buffer_planb_fallback(desc, buffer))
			return true;
		memset(buffer->data, 0, ORIOLEDB_BLCKSZ);
		return false;
	}

	result = OFileRead(desc->curFile, buffer->data, ORIOLEDB_BLCKSZ,
					   (buffer->blockNum * ORIOLEDB_BLCKSZ) % desc->singleFileSize,
					   WAIT_EVENT_SLRU_READ);

	if (result < 0)
	{
		if (missing_ok)
		{
			if (read_buffer_planb_fallback(desc, buffer))
				return true;
			memset(buffer->data, 0, ORIOLEDB_BLCKSZ);
			return false;
		}
		ereport(PANIC, (errcode_for_file_access(),
						errmsg("could not read buffer from file %s: %m", desc->curFileName)));
	}

	if (result < ORIOLEDB_BLCKSZ)
	{
		/*
		 * Short read: local file was truncated or the tail wasn't flushed.
		 * If we have a PageServer mirror, prefer its content; otherwise
		 * zero-fill the remainder like the original behaviour.
		 */
		if (!read_buffer_planb_fallback(desc, buffer))
			memset(&buffer->data[result], 0, ORIOLEDB_BLCKSZ - result);
	}

	return true;
}

/*
 * Get (or load) a buffer for the given block.  When missing_ok is true and
 * the underlying file does not exist, the buffer is invalidated and NULL is
 * returned (no lock held).  The write flag controls the lock mode for
 * already-cached buffers.
 */
static OBuffer *
get_buffer(OBuffersDesc *desc, uint32 tag, int64 blockNum, bool write,
		   bool missing_ok)
{
	OBuffersGroup *group = &desc->groups[blockNum % desc->groupsCount];
	OBuffer    *buffer = NULL;
	int			i,
				victim = 0;
	uint32		victimUsageCount = 0;
	bool		prevDirty;
	int64		prevBlockNum;
	uint32		prevTag;
	LWLockMode	lockMode = write ? LW_EXCLUSIVE : LW_SHARED;

	/* First check if required buffer is already loaded */
	LWLockAcquire(&group->groupCtlLock, LW_SHARED);
	for (i = 0; i < O_BUFFERS_PER_GROUP; i++)
	{
		buffer = &group->buffers[i];
		if (buffer->blockNum == blockNum &&
			buffer->tag == tag)
		{
			LWLockAcquire(&buffer->bufferCtlLock, lockMode);
			buffer->usageCount++;
			LWLockRelease(&group->groupCtlLock);

			return buffer;
		}
	}
	LWLockRelease(&group->groupCtlLock);

	/* No luck: have to evict some buffer */
	LWLockAcquire(&group->groupCtlLock, LW_EXCLUSIVE);

	/* Search for victim buffer */
	for (i = 0; i < O_BUFFERS_PER_GROUP; i++)
	{
		buffer = &group->buffers[i];

		/* Need to recheck after relock */
		if (buffer->blockNum == blockNum &&
			buffer->tag == tag)
		{
			LWLockAcquire(&buffer->bufferCtlLock, lockMode);
			buffer->usageCount++;
			LWLockRelease(&group->groupCtlLock);

			return buffer;
		}

		if (buffer->shadowBlockNum == blockNum &&
			buffer->shadowTag == tag)
		{
			/*
			 * There is an in-progress operation with required tag.  We must
			 * wait till it's completed.
			 */
			if (LWLockAcquireOrWait(&buffer->bufferCtlLock, LW_SHARED))
				LWLockRelease(&buffer->bufferCtlLock);
		}

		if (i == 0 || buffer->usageCount < victimUsageCount)
		{
			victim = i;
			victimUsageCount = buffer->usageCount;
		}
		buffer->usageCount /= 2;
	}
	buffer = &group->buffers[victim];
	LWLockAcquire(&buffer->bufferCtlLock, LW_EXCLUSIVE);

	prevDirty = buffer->dirty;
	prevBlockNum = buffer->blockNum;
	prevTag = buffer->tag;

	buffer->usageCount = 1;
	buffer->dirty = false;
	buffer->blockNum = blockNum;
	buffer->tag = tag;
	buffer->shadowBlockNum = prevBlockNum;
	buffer->shadowTag = prevTag;

	LWLockRelease(&group->groupCtlLock);

	if (prevDirty)
		write_buffer_data(desc, buffer->data, prevTag, prevBlockNum);

	if (!read_buffer(desc, buffer, missing_ok))
	{
		/* File doesn't exist — invalidate buffer */
		Assert(missing_ok);
		buffer->blockNum = -1;
		buffer->shadowBlockNum = -1;
		LWLockRelease(&buffer->bufferCtlLock);
		return NULL;
	}

	buffer->shadowBlockNum = -1;

	return buffer;
}

/*
 * Read or write a range of data from/to buffers.  When missing_ok is true
 * and the underlying file does not exist (read path only), returns false
 * with buf zeroed.
 */
static bool
o_buffers_rw(OBuffersDesc *desc, Pointer buf,
			 uint32 tag, int64 offset, int64 size,
			 bool write, bool missing_ok)
{
	int64		firstBlockNum = offset / ORIOLEDB_BLCKSZ,
				lastBlockNum = (offset + size - 1) / ORIOLEDB_BLCKSZ,
				blockNum;
	Pointer		ptr = buf;

	for (blockNum = firstBlockNum; blockNum <= lastBlockNum; blockNum++)
	{
		OBuffer    *buffer = get_buffer(desc, tag, blockNum, write, missing_ok);
		uint32		copySize,
					copyOffset;

		if (!buffer)
		{
			Assert(missing_ok);
			memset(buf, 0, size);
			return false;
		}

		if (firstBlockNum == lastBlockNum)
		{
			copySize = size;
			copyOffset = offset % ORIOLEDB_BLCKSZ;
		}
		else if (blockNum == firstBlockNum)
		{
			copySize = ORIOLEDB_BLCKSZ - offset % ORIOLEDB_BLCKSZ;
			copyOffset = offset % ORIOLEDB_BLCKSZ;
		}
		else if (blockNum == lastBlockNum)
		{
			copySize = (offset + size - 1) % ORIOLEDB_BLCKSZ + 1;
			copyOffset = 0;
		}
		else
		{
			copySize = ORIOLEDB_BLCKSZ;
			copyOffset = 0;
		}

		if (write)
		{
			memcpy(&buffer->data[copyOffset], ptr, copySize);
			buffer->dirty = true;
		}
		else
		{
			memcpy(ptr, &buffer->data[copyOffset], copySize);
		}
		ptr += copySize;
		LWLockRelease(&buffer->bufferCtlLock);
	}
	return true;
}

void
o_buffers_read(OBuffersDesc *desc, Pointer buf, uint32 tag, int64 offset, int64 size)
{
	Assert(OBuffersMaxTagIsValid(tag) && offset >= 0 && size > 0);
	o_buffers_rw(desc, buf, tag, offset, size, false, false);
}

bool
o_buffers_read_if_exists(OBuffersDesc *desc, Pointer buf, uint32 tag,
						 int64 offset, int64 size)
{
	Assert(OBuffersMaxTagIsValid(tag) && offset >= 0 && size > 0);
	return o_buffers_rw(desc, buf, tag, offset, size, false, true);
}

void
o_buffers_write(OBuffersDesc *desc, Pointer buf, uint32 tag, int64 offset, int64 size)
{
	Assert(OBuffersMaxTagIsValid(tag) && offset >= 0 && size > 0);
	o_buffers_rw(desc, buf, tag, offset, size, true, false);
}

bool
o_buffers_write_if_exists(OBuffersDesc *desc, Pointer buf, uint32 tag,
						  int64 offset, int64 size)
{
	Assert(OBuffersMaxTagIsValid(tag) && offset >= 0 && size > 0);
	return o_buffers_rw(desc, buf, tag, offset, size, true, true);
}

static void
o_buffers_flush(OBuffersDesc *desc,
				uint32 tag,
				int64 firstBufferNumber,
				int64 lastBufferNumber)
{
	int			i,
				j;

	for (i = 0; i < desc->groupsCount; i++)
	{
		OBuffersGroup *group = &desc->groups[i];

		for (j = 0; j < O_BUFFERS_PER_GROUP; j++)
		{
			OBuffer    *buffer = &group->buffers[j];

			LWLockAcquire(&buffer->bufferCtlLock, LW_SHARED);
			if (buffer->dirty &&
				buffer->tag == tag &&
				buffer->blockNum >= firstBufferNumber &&
				buffer->blockNum <= lastBufferNumber)
			{
				write_buffer(desc, buffer);
				buffer->dirty = false;
			}
			LWLockRelease(&buffer->bufferCtlLock);
		}
	}
}

static void
o_buffers_wipe(OBuffersDesc *desc,
			   uint32 tag,
			   int64 firstBufferNumber,
			   int64 lastBufferNumber)
{
	int			i,
				j;

	for (i = 0; i < desc->groupsCount; i++)
	{
		OBuffersGroup *group = &desc->groups[i];

		for (j = 0; j < O_BUFFERS_PER_GROUP; j++)
		{
			OBuffer    *buffer = &group->buffers[j];

			LWLockAcquire(&buffer->bufferCtlLock, LW_EXCLUSIVE);
			if (buffer->dirty &&
				buffer->tag == tag &&
				buffer->blockNum >= firstBufferNumber &&
				buffer->blockNum <= lastBufferNumber)
			{
				buffer->blockNum = -1;
				buffer->dirty = false;
				buffer->tag = 0;
			}
			LWLockRelease(&buffer->bufferCtlLock);
		}
	}
}

void
o_buffers_sync(OBuffersDesc *desc, uint32 tag,
			   int64 fromOffset, int64 toOffset,
			   uint32 wait_event_info)
{
	int64		firstPageNumber,
				lastPageNumber;
	int64		firstFileNumber,
				lastFileNumber,
				fileNumber;

	Assert(OBuffersMaxTagIsValid(tag));

	firstPageNumber = fromOffset / ORIOLEDB_BLCKSZ;
	lastPageNumber = toOffset / ORIOLEDB_BLCKSZ;
	if (toOffset % ORIOLEDB_BLCKSZ == 0)
		lastPageNumber--;

	o_buffers_flush(desc, tag, firstPageNumber, lastPageNumber);

	firstFileNumber = fromOffset / desc->singleFileSize;
	lastFileNumber = toOffset / desc->singleFileSize;
	if (toOffset % desc->singleFileSize == 0)
		lastFileNumber--;

	for (fileNumber = firstFileNumber; fileNumber <= lastFileNumber; fileNumber++)
	{
		open_file(desc, tag, fileNumber, true);
		FileSync(desc->curFile, wait_event_info);
	}
}

void
o_buffers_unlink_files_range(OBuffersDesc *desc, uint32 tag,
							 int64 firstFileNumber, int64 lastFileNumber)
{
	int64		fileNumber;

	Assert(OBuffersMaxTagIsValid(tag));

	o_buffers_wipe(desc, tag,
				   firstFileNumber * (desc->singleFileSize / ORIOLEDB_BLCKSZ),
				   (lastFileNumber + 1) * (desc->singleFileSize / ORIOLEDB_BLCKSZ) - 1);

	for (fileNumber = firstFileNumber;
		 fileNumber <= lastFileNumber;
		 fileNumber++)
		unlink_file(desc, tag, fileNumber);
}
