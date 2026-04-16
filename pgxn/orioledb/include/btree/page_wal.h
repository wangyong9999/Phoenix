/*-------------------------------------------------------------------------
 *
 * page_wal.h
 *		Page-level WAL emission declarations for OrioleDB on Neon.
 *
 * Copyright (c) 2024-2026, Oriole DB Inc.
 *
 * IDENTIFICATION
 *	  contrib/orioledb/include/btree/page_wal.h
 *
 *-------------------------------------------------------------------------
 */
#ifndef PAGE_WAL_H
#define PAGE_WAL_H

#include "btree/btree.h"
#include "btree/page_contents.h"
#include "btree/page_walrecord.h"

/* Page-level WAL redo dispatcher (called from orioledb_redo) */
extern void orioledb_page_redo(XLogReaderState *record);

/* Register B-tree's synthetic relations with PageServer (SMGR_CREATE) */
extern void orioledb_page_wal_register_tree(BTreeDescr *desc);

/* Check if page-level WAL is enabled (Neon mode) */
extern bool orioledb_page_wal_enabled(void);

/* Get stable block number from in-memory page descriptor */
extern BlockNumber orioledb_page_get_blkno(OInMemoryBlkno blkno);

/* Fill RelFileLocator for WAL block references */
extern void orioledb_page_wal_rlocator(BTreeDescr *desc,
									   RelFileLocator *rlocator);

/* Emit FPI for structural operations (SPLIT, MERGE, COMPACT, etc.) */
extern void orioledb_page_wal_emit_fpi(BTreeDescr *desc,
									   OInMemoryBlkno blkno, uint8 info);

/* Emit LEAF_INSERT delta WAL */
extern void orioledb_page_wal_leaf_insert(BTreeDescr *desc,
										  OInMemoryBlkno blkno,
										  uint16 item_index,
										  BTreeLeafTuphdr *tuphdr,
										  Pointer tuple_data,
										  uint16 tuple_len);

/* Emit LEAF_DELETE delta WAL */
extern void orioledb_page_wal_leaf_delete(BTreeDescr *desc,
										  OInMemoryBlkno blkno,
										  uint16 item_index,
										  BTreeLeafTuphdr *tuphdr);

/* Emit LEAF_UPDATE delta WAL */
extern void orioledb_page_wal_leaf_update(BTreeDescr *desc,
										  OInMemoryBlkno blkno,
										  uint16 item_index,
										  BTreeLeafTuphdr *tuphdr,
										  Pointer tuple_data,
										  uint16 tuple_len,
										  uint16 old_tuple_size);

/* Emit SPLIT FPIs (left page + right page as multi-block record) */
extern void orioledb_page_wal_split(BTreeDescr *desc,
									OInMemoryBlkno left_blkno,
									OInMemoryBlkno right_blkno);

/* Emit MERGE FPIs (left page + parent page) */
extern void orioledb_page_wal_merge(BTreeDescr *desc,
									OInMemoryBlkno left_blkno,
									OInMemoryBlkno parent_blkno);

/* Ensure page has a disk extent (pre-allocate if needed for Neon WAL) */
extern void orioledb_page_ensure_extent(BTreeDescr *desc,
										OInMemoryBlkno blkno);

#endif							/* PAGE_WAL_H */
