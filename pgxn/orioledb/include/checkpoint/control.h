/*-------------------------------------------------------------------------
 *
 * control.h
 *		Declarations for control file.
 *
 * Copyright (c) 2024-2026, Oriole DB Inc.
 * Copyright (c) 2025-2026, Supabase Inc.
 *
 * IDENTIFICATION
 *	  contrib/orioledb/include/checkpoint/control.h
 *
 *-------------------------------------------------------------------------
 */
#ifndef __CHECKPOINT_CONTROL_H__
#define __CHECKPOINT_CONTROL_H__

#include "postgres.h"

#include "orioledb.h"

typedef struct
{
	UndoLocation lastUndoLocation;
	UndoLocation checkpointRetainStartLocation;
	UndoLocation checkpointRetainEndLocation;
} CheckpointUndoInfo;

#define NUM_CHECKPOINTABLE_UNDO_LOGS	2

/*
 * Bump every time CheckpointControl structure changes its format.
 * Also on-the-flight conversion routine should be added to
 * check_checkpoint_control()
 */
#define ORIOLEDB_CHECKPOINT_CONTROL_VERSION	1

/*
 * To ensure correct reading of controlFileVersion, changes in struct layout
 * are permitted only between .controlFileVersion and .crc, that
 * should be the last.
 */
typedef struct
{
	uint64		controlIdentifier;
	uint32		lastCheckpointNumber;
	uint32		controlFileVersion;
	CommitSeqNo lastCSN;
	OXid		lastXid;
	UndoLocation lastUndoLocation;
	XLogRecPtr	toastConsistentPtr;
	XLogRecPtr	replayStartPtr;
	XLogRecPtr	sysTreesStartPtr;
	uint64		mmapDataLength;
	CheckpointUndoInfo undoInfo[NUM_CHECKPOINTABLE_UNDO_LOGS];
	UndoLocation checkpointRetainStartLocation;
	UndoLocation checkpointRetainEndLocation;
	OXid		checkpointRetainXmin;
	OXid		checkpointRetainXmax;
	uint32		binaryVersion;
	bool		s3Mode;
	/* CRC of all fields above. It should be last */
	pg_crc32c	crc;
} CheckpointControl;

#define CONTROL_FILENAME    ORIOLEDB_DATA_DIR"/control"

/*
 * Synthetic RelFileNumber used to publish the OrioleDB checkpoint control
 * file to Neon PageServer as a single-block fake relation.
 *
 * Placed in the top 256 values of the 32-bit RelFileNumber space, which
 * PG's user-relfilenode allocator will not reach before OID wraparound —
 * hitting this range requires ~4.29 billion relfilenode allocations in a
 * single cluster, which is unreachable by any practical workload.
 *
 * Any overlap with user relfilenodes would cause PageServer to conflate
 * OrioleDB control-file pages with a user table's block 0 at the same
 * tablespace, which is silent corruption. Keep this reservation disjoint
 * from the OrioleDB o_buffers reservation range (ORIOLEDB_OBUF_* in
 * o_buffers.c); the StaticAssert in o_buffers.c enforces non-overlap.
 */
#define ORIOLEDB_CONTROL_FILE_OID	0xFFFFFFFEu

StaticAssertDecl(ORIOLEDB_CONTROL_FILE_OID >= 0xFFFFFF00u &&
				 ORIOLEDB_CONTROL_FILE_OID < UINT32_MAX,
				 "ORIOLEDB_CONTROL_FILE_OID must live in the reserved "
				 "top-256 synthetic OID range (0xFFFFFF00 .. 0xFFFFFFFE) "
				 "to stay unreachable by PG's user-relfilenode allocator");

#define GetCheckpointableUndoLog(i) \
	(AssertMacro((i) >= 0 && (i) < 2), \
		(i) == 0 ? UndoLogRegular : UndoLogSystem)

/*
 * Physical size of the orioledb_data/control file.  Note that this is considerably
 * bigger than the actually used size (ie, sizeof(CheckpointControl)).
 * The idea is to keep the physical size constant independent of format
 * changes, so that get_checkpoint_control_data will deliver a suitable wrong-version
 * message instead of a read error if it's looking at an incompatible file.
 */
#define CHECKPOINT_CONTROL_FILE_SIZE	8192

extern bool get_checkpoint_control_data(CheckpointControl *control);
extern void check_checkpoint_control(CheckpointControl *control);
extern void write_checkpoint_control(CheckpointControl *control);

#endif							/* __CHECKPOINT_CONTROL_H__ */
