/*-------------------------------------------------------------------------
 *
 * control.c
 *		Routines to work with control file.
 *
 * Copyright (c) 2024-2026, Oriole DB Inc.
 * Copyright (c) 2025-2026, Supabase Inc.
 *
 * IDENTIFICATION
 *	  contrib/orioledb/src/checkpoint/control.c
 *
 *-------------------------------------------------------------------------
 */

#include "postgres.h"

#include <unistd.h>

#include "orioledb.h"
#include "access/xlog.h"
#include "miscadmin.h"
#include "access/xloginsert.h"
#include "catalog/pg_tablespace_d.h"
#include "catalog/storage_xlog.h"
#include "storage/ipc.h"
#include "storage/procnumber.h"
#include "storage/smgr.h"

#include "btree/io.h"
#include "checkpoint/control.h"

#include "utils/wait_event.h"

#include "transam/oxid.h"	/* xid_meta */

/*
 * Read checkpoint control file data from the disk.
 *
 * Returns false if the control file doesn't exist.
 */
bool
get_checkpoint_control_data(CheckpointControl *control)
{
	int			controlFile;
	Size		readBytes;

	controlFile = BasicOpenFile(CONTROL_FILENAME, O_RDONLY | PG_BINARY);
	if (controlFile < 0)
	{
		if (errno == ENOENT)
		{
			/*
			 * Neon Plan E fallback: the control file is missing locally.
			 * Try to fetch the FPI that write_checkpoint_control() emitted
			 * via XLogRegisterBlock into PageServer, using the synthetic
			 * relation (dbOid=0, relNumber=ORIOLEDB_CONTROL_FILE_OID).
			 *
			 * We use smgrexists + smgrnblocks as the preflight instead of
			 * PG_TRY/PG_CATCH: callers (OrioleDB startup) may hold LWLocks
			 * and FlushErrorState would corrupt the holdoff/lock bookkeeping.
			 * If PageServer hasn't recorded a block under this relation yet
			 * (no prior checkpoint on this branch), smgrexists returns
			 * false and we treat the control file as absent.
			 */
			if (smgr_hook != NULL && IsUnderPostmaster)
			{
				RelFileLocator rlocator;
				SMgrRelation reln;
				char		page[BLCKSZ];

				rlocator.spcOid = DEFAULTTABLESPACE_OID;
				rlocator.dbOid = 0;
				rlocator.relNumber = ORIOLEDB_CONTROL_FILE_OID;
				reln = smgropen(rlocator, INVALID_PROC_NUMBER,
								RELPERSISTENCE_PERMANENT);

				if (smgrexists(reln, MAIN_FORKNUM) &&
					smgrnblocks(reln, MAIN_FORKNUM) > 0)
				{
					smgrread(reln, MAIN_FORKNUM, 0, page);
					memcpy(control, page, sizeof(CheckpointControl));
					check_checkpoint_control(control);
					elog(LOG, "OrioleDB: control file loaded from PageServer "
						 "(chkp=%u)", control->lastCheckpointNumber);
					return true;
				}
			}
			return false;
		}

		ereport(ERROR,
				(errcode_for_file_access(),
				 errmsg("could not open file \"%s\": %m",
						CONTROL_FILENAME)));
	}

	readBytes = read(controlFile, (Pointer) control, sizeof(CheckpointControl));

	if (readBytes == 0)
	{
		close(controlFile);
		return false;
	}
	if (readBytes != sizeof(CheckpointControl))
	{
		int			save_errno = errno;

		close(controlFile);
		errno = save_errno;
		ereport(ERROR,
				(errcode_for_file_access(),
				 errmsg("could not read data from control file \"%s\": %m",
						CONTROL_FILENAME)));
	}

	close(controlFile);
	check_checkpoint_control(control);
	return true;
}

/*
 * Check checkpoint control data
 *   - Check CRC
 *   - Check control parameters
 */
void
check_checkpoint_control(CheckpointControl *control)
{
	pg_crc32c	crc;

	INIT_CRC32C(crc);
	COMP_CRC32C(crc, control, offsetof(CheckpointControl, crc));
	FIN_CRC32C(crc);

	if (crc != control->crc)
		elog(ERROR, "Wrong CRC in control file");

	if (control->controlFileVersion != ORIOLEDB_CHECKPOINT_CONTROL_VERSION)
	{
		/*
		 * Now we have only one control version. When we bump
		 * ORIOLEDB_CHECKPOINT_CONTROL_VERSION this is the place to write
		 * routine for on-the-flight convesion of data read from control file
		 * to CheckpointControl contents.
		 */
		ereport(FATAL,
				(errmsg("checkpoint files are incompatible with server"),
				 errdetail("OrioleDB checkpount control file was initialized with version %d,"
						   " but the currently required version is %d.",
						   control->controlFileVersion, ORIOLEDB_CHECKPOINT_CONTROL_VERSION)));
	}

	if (control->binaryVersion != ORIOLEDB_BINARY_VERSION)
		ereport(FATAL,
				(errmsg("database files are incompatible with server"),
				 errdetail("OrioleDB was initialized with binary version %d,"
						   " but the extension is compiled with binary version %d.",
						   control->binaryVersion, ORIOLEDB_BINARY_VERSION),
				 errhint("It looks like you need to initdb.")));

	if (control->s3Mode != orioledb_s3_mode)
		ereport(FATAL,
				(errmsg("database files are incompatible with server"),
				 errdetail("OrioleDB was initialized with S3 mode %s,"
						   " but the extension is configured with S3 mode %s.",
						   control->s3Mode ? "on" : "off",
						   orioledb_s3_mode ? "on" : "off")));
}

/*
 * Write checkpoint control file to the disk (and sync).
 */
void
write_checkpoint_control(CheckpointControl *control)
{
	File		controlFile;
	char		buffer[CHECKPOINT_CONTROL_FILE_SIZE];

	INIT_CRC32C(control->crc);
	COMP_CRC32C(control->crc, control, offsetof(CheckpointControl, crc));
	FIN_CRC32C(control->crc);

	memset(buffer, 0, CHECKPOINT_CONTROL_FILE_SIZE);
	memcpy(buffer, control, sizeof(CheckpointControl));

	controlFile = PathNameOpenFile(CONTROL_FILENAME, O_RDWR | O_CREAT | PG_BINARY);
	if (controlFile < 0)
		ereport(FATAL, (errcode_for_file_access(),
						errmsg("could not open checkpoint control file %s: %m", CONTROL_FILENAME)));

	if (OFileWrite(controlFile, buffer, CHECKPOINT_CONTROL_FILE_SIZE, 0,
				   WAIT_EVENT_SLRU_WRITE) != CHECKPOINT_CONTROL_FILE_SIZE ||
		FileSync(controlFile, WAIT_EVENT_SLRU_SYNC) != 0)
		ereport(FATAL, (errcode_for_file_access(),
						errmsg("could not write checkpoint control to file %s: %m", CONTROL_FILENAME)));

	FileClose(controlFile);

	/*
	 * Neon Plan E: emit control file as FPI to PageServer.
	 * Uses fake relation (dbOid=0, relNumber=0) so GetPage can
	 * serve it on restart without local files.
	 *
	 * Skip during shutdown checkpoint: XLogInsertAllowed() is still
	 * true at the moment the shutdown checkpoint starts, but by the
	 * time we finish inserting here PG has transitioned to "database
	 * system is shutting down" and triggers a PANIC with "concurrent
	 * write-ahead log activity". The previous checkpoint's FPI plus
	 * SafeKeeper WAL since then is enough to recover on restart, so
	 * losing this shutdown FPI is safe.
	 */
	{
		extern bool checkpoint_is_shutdown;

		if (smgr_hook != NULL && !RecoveryInProgress()
			&& !checkpoint_is_shutdown && XLogInsertAllowed())
		{
			RelFileLocator rlocator;
			char		page[BLCKSZ];

			rlocator.spcOid = DEFAULTTABLESPACE_OID;
			rlocator.dbOid = 0;
			rlocator.relNumber = ORIOLEDB_CONTROL_FILE_OID;

			/* Pack control data into a standard 8KB page */
			memset(page, 0, BLCKSZ);
			memcpy(page, buffer, Min(CHECKPOINT_CONTROL_FILE_SIZE, BLCKSZ));

			XLogBeginInsert();
			XLogRegisterBlock(0, &rlocator, MAIN_FORKNUM, 0,
							  page, REGBUF_FORCE_IMAGE | REGBUF_WILL_INIT);
			XLogInsert(ORIOLEDB_RMGR_ID, ORIOLEDB_XLOG_PAGE_IMAGE);

			elog(LOG, "OrioleDB: control file FPI emitted (chkp=%u, replayStartPtr=%X/%X)",
				 control->lastCheckpointNumber,
				 LSN_FORMAT_ARGS(control->replayStartPtr));
		}
	}
}

/*
 * Cold-start summary — basebackup-delivered complement to the
 * checkpoint control file.
 *
 * The control file is written at checkpoint time, so on cold-start
 * (stateless restart), xid_meta->nextXid loaded from it can be stale
 * relative to `sync_lsn` — OXids allocated after the last checkpoint
 * but before the crash will have advanced beyond control->lastXid.
 *
 * The walingest-maintained summary (`ORIOLEDB_STATE_KEY` in
 * PageServer keyspace, shipped as `PGDATA/global/orioledb.state` in
 * basebackup) covers that tail. It is derived from the rmid=129
 * stream as walingest processes it, so its `next_oxid` field is
 * the largest OXid ever referenced in a record that reached
 * SafeKeeper — exactly what compute needs to seed `xid_meta->nextXid`.
 *
 * See `libs/wal_decoder/src/orioledb_state.rs`: `encode_packed`
 * emits the wire format this reader consumes.
 *
 * Design choices: the summary only *advances* fields — never
 * regresses. Absence of the file is a valid state (tenant has never
 * written rmid=129 traffic). Bad magic / version is logged at LOG
 * level and ignored, matching the "forward-compatible" posture of
 * the checkpoint control file itself.
 */
#define ORIOLEDB_STATE_FILENAME		"global/orioledb.state"
#define ORIOLEDB_STATE_MAGIC		0x534F524Fu		/* "OROS" */
#define ORIOLEDB_STATE_VERSION		2u
#define ORIOLEDB_STATE_ENCODED_SIZE	48

typedef struct
{
	uint32		magic;
	uint32		version;
	uint64		next_oxid;
	uint32		last_pg_xid_seen;
	uint32		reserved;
	uint64		last_ingested_lsn_raw;
	uint64		ingested_count;
	uint64		next_csn;		/* v2 */
} OrioleDBStatePacked;

StaticAssertDecl(sizeof(OrioleDBStatePacked) == ORIOLEDB_STATE_ENCODED_SIZE,
				 "OrioleDBStatePacked size must match the Rust encoder");

void
apply_orioledb_cold_start_summary(void)
{
	int			fd;
	OrioleDBStatePacked packed;
	int			nread;
	OXid		current;

	/*
	 * BasicOpenFile — not PathNameOpenFile — because this function is
	 * called from checkpoint_shmem_init, which runs in postmaster
	 * before InitFileAccess sets up the VFD cache. PathNameOpenFile
	 * asserts `SizeVfdCache > 0` and traps if called too early.
	 */
	fd = BasicOpenFile(ORIOLEDB_STATE_FILENAME, O_RDONLY | PG_BINARY);
	if (fd < 0)
	{
		/*
		 * Not an error: most paths won't have a summary yet (fresh
		 * tenant, non-OrioleDB tenant, or basebackup from a
		 * pageserver that predates Phase 2.1 C.2).
		 */
		return;
	}

	nread = read(fd, (char *) &packed, sizeof(packed));
	close(fd);

	if (nread != sizeof(packed))
	{
		elog(LOG,
			 "OrioleDB cold-start summary %s: truncated (%d / %zu bytes)",
			 ORIOLEDB_STATE_FILENAME, nread, sizeof(packed));
		return;
	}

	if (packed.magic != ORIOLEDB_STATE_MAGIC)
	{
		elog(LOG,
			 "OrioleDB cold-start summary %s: bad magic 0x%08x (expected 0x%08x)",
			 ORIOLEDB_STATE_FILENAME, packed.magic, ORIOLEDB_STATE_MAGIC);
		return;
	}

	if (packed.version != ORIOLEDB_STATE_VERSION)
	{
		elog(LOG,
			 "OrioleDB cold-start summary %s: unsupported version %u (expected %u)",
			 ORIOLEDB_STATE_FILENAME, packed.version,
			 ORIOLEDB_STATE_VERSION);
		return;
	}

	/*
	 * Bump xid_meta->nextXid if the summary advances past the
	 * checkpoint-control-file value. Summary never regresses — it
	 * is at least as new as the control file (both are derived from
	 * records ≤ sync_lsn).
	 */
	current = pg_atomic_read_u64(&xid_meta->nextXid);
	if (packed.next_oxid > current)
	{
		pg_atomic_write_u64(&xid_meta->nextXid, packed.next_oxid);
		if (pg_atomic_read_u64(&xid_meta->runXmin) < packed.next_oxid)
			pg_atomic_write_u64(&xid_meta->runXmin, packed.next_oxid);
		if (pg_atomic_read_u64(&xid_meta->globalXmin) < packed.next_oxid)
			pg_atomic_write_u64(&xid_meta->globalXmin, packed.next_oxid);
		if (pg_atomic_read_u64(&xid_meta->lastXidWhenUpdatedGlobalXmin) < packed.next_oxid)
			pg_atomic_write_u64(&xid_meta->lastXidWhenUpdatedGlobalXmin,
								packed.next_oxid);
		if (pg_atomic_read_u64(&xid_meta->writtenXmin) < packed.next_oxid)
			pg_atomic_write_u64(&xid_meta->writtenXmin, packed.next_oxid);
		if (pg_atomic_read_u64(&xid_meta->writeInProgressXmin) < packed.next_oxid)
			pg_atomic_write_u64(&xid_meta->writeInProgressXmin, packed.next_oxid);
		if (pg_atomic_read_u64(&xid_meta->cleanedXmin) < packed.next_oxid)
			pg_atomic_write_u64(&xid_meta->cleanedXmin, packed.next_oxid);

		elog(LOG,
			 "OrioleDB cold-start: nextXid bumped " UINT64_FORMAT " -> " UINT64_FORMAT
			 " from %s (ingested_count=" UINT64_FORMAT
			 ", last_lsn=%X/%X)",
			 current, packed.next_oxid, ORIOLEDB_STATE_FILENAME,
			 packed.ingested_count,
			 (uint32) (packed.last_ingested_lsn_raw >> 32),
			 (uint32) packed.last_ingested_lsn_raw);
	}
	else
	{
		elog(DEBUG1,
			 "OrioleDB cold-start: nextXid already " UINT64_FORMAT
			 " >= summary.next_oxid " UINT64_FORMAT " (%s)",
			 current, packed.next_oxid, ORIOLEDB_STATE_FILENAME);
	}

	/*
	 * Bump `startupCommitSeqNo` when the summary carries a newer
	 * next_csn. PG's StartupXLOG
	 * (vendor/postgres-v17/src/backend/access/transam/xlog.c:5669)
	 * writes `startupCommitSeqNo` into
	 * `TransamVariables->nextCommitSeqNo` after our hook runs, so
	 * modifying the global here is the correct hand-off.
	 *
	 * The summary's next_csn is derived from WAL_REC_COMMIT /
	 * WAL_REC_JOINT_COMMIT sub-records ingested up to sync_lsn; it
	 * will only be populated after the first user commit has
	 * shipped through walingest, so on a brand-new tenant this block
	 * is a safe no-op.
	 */
	if (packed.next_csn > startupCommitSeqNo)
	{
		elog(LOG,
			 "OrioleDB cold-start: startupCommitSeqNo bumped "
			 UINT64_FORMAT " -> " UINT64_FORMAT " from %s",
			 (uint64) startupCommitSeqNo, packed.next_csn,
			 ORIOLEDB_STATE_FILENAME);
		startupCommitSeqNo = packed.next_csn;
	}
}
