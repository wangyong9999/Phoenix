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
	 */
	if (smgr_hook != NULL && !RecoveryInProgress() && XLogInsertAllowed())
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
