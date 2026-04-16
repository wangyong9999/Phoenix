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
 * Mapping: each OBuffersDesc gets a distinct relNumber in the
 * ORIOLEDB_OBUF_RELNODE_BASE reserved range so different log streams
 * (undo row / undo page / undo system / xidmap) never collide.
 */
#define ORIOLEDB_OBUF_RELNODE_BASE  65600u
#define ORIOLEDB_OBUF_TAGS_PER_LOG  16

static uint32
obuffers_relnode(OBuffersDesc *desc, uint32 tag)
{
	/*
	 * Stable across restarts: driven by the user-supplied planBLogId and the
	 * tag (segment family within the log). A log must declare planBLogId != 0
	 * to participate in Plan B mirroring.
	 */
	Assert(desc->planBLogId != 0);
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

	Assert(OBuffersMaxTagIsValid(tag));

	open_file(desc, tag, blockNum / (desc->singleFileSize / ORIOLEDB_BLCKSZ), true);
	result = OFileWrite(desc->curFile, data, ORIOLEDB_BLCKSZ,
						(blockNum * ORIOLEDB_BLCKSZ) % desc->singleFileSize,
						WAIT_EVENT_SLRU_WRITE);
	if (result != ORIOLEDB_BLCKSZ)
		ereport(PANIC, (errcode_for_file_access(),
						errmsg("could not write buffer to file %s: %m", desc->curFileName)));

	/*
	 * Plan B: mirror the write into the WAL as a full page image. PageServer
	 * treats the record as a Value::Image so later smgrread() on the synthetic
	 * relation returns exactly this content, even if the compute is restarted
	 * on a different node with no local orioledb_undo / xidmap files.
	 *
	 * Uses the same REGBUF_FORCE_IMAGE | REGBUF_WILL_INIT flags as Plan E for
	 * B-tree pages so wal_decoder recognises the record as an image.
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
		XLogInsert(ORIOLEDB_RMGR_ID, ORIOLEDB_XLOG_PAGE_IMAGE);
	}
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
