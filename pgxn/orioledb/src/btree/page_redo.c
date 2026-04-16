/*-------------------------------------------------------------------------
 *
 * page_redo.c
 *		Page-level WAL redo functions for OrioleDB on Neon.
 *
 * These redo functions operate on individual pages without B-tree context,
 * enabling Neon PageServer's wal-redo to materialize OrioleDB pages.
 *
 * Design: each redo function receives a page buffer via XLogReadBufferForRedo
 * and applies the delta described in the WAL record. No page pool, no undo,
 * no tree navigation required.
 *
 * These functions are called from orioledb_redo() when the info byte matches
 * a page-level WAL type. FPI-based records (SPLIT, MERGE, COMPACT) are
 * handled by wal_decoder as Value::Image and never reach redo.
 *
 * Copyright (c) 2024-2026, Oriole DB Inc.
 *
 * IDENTIFICATION
 *	  contrib/orioledb/src/btree/page_redo.c
 *
 *-------------------------------------------------------------------------
 */

#include "postgres.h"

#include "orioledb.h"

#include "access/xlog.h"
#include "access/xlogutils.h"
#include "access/xlogreader.h"

#include "btree/page_contents.h"
#include "btree/page_chunks.h"
#include "btree/page_wal.h"
#include "btree/page_walrecord.h"

/* Forward declarations for per-operation redo functions */
static void orioledb_redo_leaf_insert(XLogReaderState *record);
static void orioledb_redo_leaf_delete(XLogReaderState *record);
static void orioledb_redo_leaf_update(XLogReaderState *record);

/*
 * orioledb_page_redo — dispatcher for page-level WAL records.
 *
 * Called from the main orioledb_redo() for non-CONTAINER, non-PAGE_IMAGE
 * record types. FPI-based records are handled directly by PageServer
 * as Value::Image and should never reach this function.
 */
void
orioledb_page_redo(XLogReaderState *record)
{
	uint8		info = XLogRecGetInfo(record) & ~XLR_INFO_MASK;

	switch (info)
	{
		case ORIOLEDB_XLOG_LEAF_INSERT:
			orioledb_redo_leaf_insert(record);
			break;

		case ORIOLEDB_XLOG_LEAF_DELETE:
			orioledb_redo_leaf_delete(record);
			break;

		case ORIOLEDB_XLOG_LEAF_UPDATE:
			orioledb_redo_leaf_update(record);
			break;

		case ORIOLEDB_XLOG_LEAF_LOCK:
			/* LEAF_LOCK modifies same fields as DELETE — reuse redo */
			orioledb_redo_leaf_delete(record);
			break;

		case ORIOLEDB_XLOG_PAGE_INIT:
		case ORIOLEDB_XLOG_COMPACT:
		case ORIOLEDB_XLOG_SPLIT:
		case ORIOLEDB_XLOG_MERGE:
		case ORIOLEDB_XLOG_ROOT_SPLIT:
		case ORIOLEDB_XLOG_UNDO_APPLY:
			/*
			 * These are FPI-based records. In normal Neon operation, the
			 * wal_decoder extracts the image and stores it as Value::Image,
			 * so redo is never called. If we DO reach here (e.g., standalone
			 * PG recovery), the FPI is applied by the standard XLog machinery
			 * before we're called, so nothing to do.
			 */
			break;

		default:
			elog(PANIC, "orioledb_page_redo: unknown info byte 0x%02x", info);
			break;
	}
}

/*
 * Redo a LEAF_INSERT operation.
 *
 * WAL format:
 *   Block ref 0: target leaf page (delta, not FPI)
 *   BufData:     xl_orioledb_leaf_insert
 *                + BTreeLeafTuphdr (sizeof = 16 bytes, packed bitfields)
 *                + tuple payload (variable length)
 *
 * Redo steps:
 *   1. Navigate to item_index position in the page
 *   2. Insert space via page_locator_insert_item()
 *   3. Copy tuple header + data into the new slot
 */
static void
orioledb_redo_leaf_insert(XLogReaderState *record)
{
	Buffer		buf;
	XLogRedoAction action;

	action = XLogReadBufferForRedo(record, 0, &buf);

	if (action == BLK_NEEDS_REDO)
	{
		Page		page = BufferGetPage(buf);
		Size		datalen;
		char	   *data = XLogRecGetBlockData(record, 0, &datalen);
		xl_orioledb_leaf_insert *xlrec = (xl_orioledb_leaf_insert *) data;
		char	   *tuphdr_and_tuple = data + SizeOfOrioleDBLeafInsert;
		LocationIndex item_size = xlrec->tuple_size;
		BTreePageItemLocator loc;

		/*
		 * Recover the full page locator from the global OffsetNumber.
		 * This maps item_index → (chunkOffset, itemOffset) via
		 * page_item_fill_locator, the inverse of
		 * BTREE_PAGE_LOCATOR_GET_OFFSET.
		 */
		BTREE_PAGE_OFFSET_GET_LOCATOR(page, xlrec->item_index, &loc);

		/* Make space for the new item */
		page_locator_insert_item(page, &loc, item_size);

		/* Copy the tuple header + data into the new slot */
		memcpy(BTREE_PAGE_LOCATOR_GET_ITEM(page, &loc),
			   tuphdr_and_tuple,
			   item_size);

		/*
		 * Do NOT call PageSetLSN — OrioleDB page header (OrioleDBPageHeader)
		 * is incompatible with PG's PageHeaderData. PG's PageSetLSN writes
		 * to offset 0 which is OrioleDB's state field. PageServer tracks
		 * page versions by (key, LSN) pairs, not by in-page LSN.
		 */
		MarkBufferDirty(buf);
	}

	if (BufferIsValid(buf))
		UnlockReleaseBuffer(buf);
}

/*
 * Redo a LEAF_DELETE operation.
 *
 * Only modifies the BTreeLeafTuphdr at the given position.
 * The WAL record contains the full tuphdr to restore exact state.
 */
static void
orioledb_redo_leaf_delete(XLogReaderState *record)
{
	Buffer		buf;
	XLogRedoAction action;

	action = XLogReadBufferForRedo(record, 0, &buf);

	if (action == BLK_NEEDS_REDO)
	{
		Page		page = BufferGetPage(buf);
		Size		datalen;
		char	   *data = XLogRecGetBlockData(record, 0, &datalen);
		xl_orioledb_leaf_delete *xlrec = (xl_orioledb_leaf_delete *) data;
		BTreeLeafTuphdr *stored_tuphdr =
			(BTreeLeafTuphdr *) (data + SizeOfOrioleDBLeafDelete);
		BTreePageItemLocator loc;
		BTreeLeafTuphdr *page_tuphdr;

		BTREE_PAGE_OFFSET_GET_LOCATOR(page, xlrec->item_index, &loc);
		page_tuphdr = (BTreeLeafTuphdr *) BTREE_PAGE_LOCATOR_GET_ITEM(page, &loc);

		/* Restore the exact tuple header state from WAL */
		memcpy(page_tuphdr, stored_tuphdr, sizeof(BTreeLeafTuphdr));

		MarkBufferDirty(buf);
	}

	if (BufferIsValid(buf))
		UnlockReleaseBuffer(buf);
}

/*
 * Redo a LEAF_UPDATE operation.
 *
 * Replaces the tuple header and data at the given position.
 * If sizes differ, uses page_locator_resize_item for structural change.
 */
static void
orioledb_redo_leaf_update(XLogReaderState *record)
{
	Buffer		buf;
	XLogRedoAction action;

	action = XLogReadBufferForRedo(record, 0, &buf);

	if (action == BLK_NEEDS_REDO)
	{
		Page		page = BufferGetPage(buf);
		Size		datalen;
		char	   *data = XLogRecGetBlockData(record, 0, &datalen);
		xl_orioledb_leaf_update *xlrec = (xl_orioledb_leaf_update *) data;
		char	   *tuphdr_and_tuple = data + SizeOfOrioleDBLeafUpdate;
		LocationIndex new_item_size = xlrec->new_tuple_size;
		BTreePageItemLocator loc;

		BTREE_PAGE_OFFSET_GET_LOCATOR(page, xlrec->item_index, &loc);

		/* Resize if needed */
		if (new_item_size != xlrec->old_tuple_size)
			page_locator_resize_item(page, &loc, new_item_size);

		/* Overwrite tuple header + data */
		memcpy(BTREE_PAGE_LOCATOR_GET_ITEM(page, &loc),
			   tuphdr_and_tuple,
			   new_item_size);

		MarkBufferDirty(buf);
	}

	if (BufferIsValid(buf))
		UnlockReleaseBuffer(buf);
}
