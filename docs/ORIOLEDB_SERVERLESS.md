# OrioleDB on Neon: Serverless Architecture

> **Status:** Implemented — Page-level delta WAL (true Log-is-Data for B-tree pages)  
> **Last updated:** 2026-04-16

## Overview

This document describes the architecture for running OrioleDB as a fully
serverless storage engine on Neon's compute-storage separation infrastructure.
All OrioleDB state (B-tree pages, metadata, undo logs) is recoverable from
PageServer — compute nodes hold no persistent local state.

---

## 1. Architecture

```
                    ┌─────────────────────────────────────────────────┐
                    │              Stateless Compute                  │
                    │                                                 │
                    │  ┌──────────┐   ┌───────────┐   ┌───────────┐  │
                    │  │ OrioleDB │   │  PG Heap   │   │  Neon     │  │
                    │  │ B-tree   │   │  (native)  │   │  smgr     │  │
                    │  │ Engine   │   │            │   │  hooks    │  │
                    │  └────┬─────┘   └─────┬──────┘   └─────┬─────┘  │
                    │       │               │               │         │
                    │  Plan E/B FPI    Standard smgr    GetPage/WAL   │
                    └───────┼───────────────┼───────────────┼─────────┘
                            │               │               │
                    ════════╪═══════════════╪═══════════════╪══════ WAL Stream
                            │               │               │
                    ┌───────▼───────────────▼───────────────▼─────────┐
                    │                   SafeKeeper                    │
                    │            (WAL durability layer)               │
                    └───────────────────────┬─────────────────────────┘
                                            │
                    ┌───────────────────────▼─────────────────────────┐
                    │                   PageServer                    │
                    │                                                 │
                    │  ┌─────────────┐  ┌─────────────┐              │
                    │  │  WAL Decoder│  │ Page Store   │              │
                    │  │  rmid=129   │──│ Value::Image │              │
                    │  │  → FPI      │  │ (B-tree,     │              │
                    │  │  → WAL key  │  │  control,    │              │
                    │  └─────────────┘  │  map, undo)  │              │
                    │                   └──────┬───────┘              │
                    │                          │ GetPage              │
                    └──────────────────────────┼──────────────────────┘
                                               │
                                    ┌──────────▼──────────┐
                                    │   Cloud Storage     │
                                    │   (layer files)     │
                                    └─────────────────────┘
```

## 2. Data Flow Mechanisms

### Plan E: Checkpoint Full Page Images

OrioleDB's COW checkpoint produces clean page images. Plan E emits each page
as a WAL record with `REGBUF_FORCE_IMAGE | REGBUF_WILL_INIT`, creating a
Full Page Image (FPI) that PageServer stores as `Value::Image`.

| Data Type | RelFileLocator | Fork | Notes |
|-----------|---------------|------|-------|
| B-tree data pages | `(DEFAULT, datoid, relnode)` | MAIN | One FPI per 8KB block |
| Control file | `(DEFAULT, 0, 65500)` | MAIN | Block 0 only |
| Map files | `(DEFAULT, datoid, relnode)` | INIT | Full file, multi-block |

**Write path** (checkpoint):
```
OrioleDB checkpoint
  → write_page_to_disk() / write_checkpoint_control() / checkpoint_map_write_header()
  → XLogBeginInsert()
  → XLogRegisterBlock(REGBUF_FORCE_IMAGE | REGBUF_WILL_INIT)
  → XLogInsert(ORIOLEDB_RMGR_ID, ORIOLEDB_XLOG_PAGE_IMAGE)
  → walproposer → SafeKeeper → PageServer
  → wal_decoder: block_is_image() → Value::Image
```

**Read path** (normal backends):
```
btree_smgr_read()
  → smgropen(DEFAULT, datoid, relnode)
  → smgrread(MAIN_FORKNUM, blkno)
  → Neon smgr → PageServer GetPage
  → returns Value::Image from last checkpoint
```

**GUC:** `orioledb.skip_unmodified_trees = false` forces ALL trees to emit
FPIs at every checkpoint, even if unmodified. Without this, unmodified trees
would have no images in PageServer after the first checkpoint cycle.

### Plan B: Undo/Xidmap Mirroring

Undo logs and xidmap are mirrored to PageServer on every write via the
`OBuffersDesc.planBLogId` mechanism.

| Log Type | planBLogId | Synthetic relNumber |
|----------|-----------|-------------------|
| Undo | 1 | `1 * TAGS_PER_LOG + tag` |
| Xidmap | 2 | `2 * TAGS_PER_LOG + tag` |

**Write:** `write_buffer_data()` → `XLogInsert(FPI)` after every local write  
**Read:** `read_buffer_planb_fallback()` → `smgrread()` on local file miss

### WAL Decoder Routing

PageServer's WAL decoder handles OrioleDB records (rmid=129) in two ways:

1. **FPI records** (block refs with `REGBUF_FORCE_IMAGE`): Stored as
   `Value::Image` at the standard `(spc, db, rel, fork, blkno)` key —
   same path as native PG FPIs.

2. **Delta records** (row-level WAL without FPI): Stored under
   `ORIOLEDB_WAL_KEY_PREFIX (0x70)` keyed by LSN. Block refs from delta
   records are skipped (PG walredo cannot replay rmid=129 deltas).

## 3. Stateless Restart Flow

```
Compute cold start:
  1. sync_safekeepers → get LSN
  2. get_basebackup(LSN) → PG catalog at latest state
  3. inject: orioledb.skip_unmodified_trees = false
  4. check .orioledb_initialized marker

  If previously initialized (restart):
    5. Copy SafeKeeper WAL → pg_wal/ (patch zero-page headers)
    6. Write orioledb_recovery.signal with redo start LSN
    7. PG starts → detects signal → enters crash recovery
    8. Selective replay: only rmid=129 (OrioleDB) + XACT records
       - OrioleDB WAL rebuilds B-trees in memory
       - XACT records rebuild xid/CSN commit state
       - Non-OrioleDB records skipped (PG catalog already current)
    9. End-of-recovery: CheckPoint_hook fires
       - OrioleDB writes B-trees to local + emits FPIs
       - No PG checkpoint WAL record (avoids SafeKeeper gap)
   10. Normal operation: backends read via PageServer GetPage

  If first run:
    5. PG starts normally
    6. Write .orioledb_sync_lsn + .orioledb_initialized
    7. Normal operation
```

## 4. Modified Components

### OrioleDB Extension (`pgxn/orioledb/`)

| File | Changes |
|------|---------|
| `src/btree/io.c` | Plan E FPI emission in `write_page_to_disk()`, PageServer read in `btree_smgr_read()`, `orioledb_planE_materialize()` helper |
| `src/checkpoint/control.c` | Control file FPI emit + PageServer read fallback |
| `src/checkpoint/checkpoint.c` | Map file full-body FPI emit + PageServer read fallback, sync_lsn tracking |
| `src/utils/o_buffers.c` | Plan B write mirroring + read fallback |
| `include/utils/o_buffers.h` | `planBLogId` field |
| `src/transam/undo.c` | `planBLogId = 1` |
| `src/transam/oxid.c` | `planBLogId = 2` |
| `src/recovery/recovery.c` | Selective WAL replay support |
| `src/orioledb.c` | `skip_unmodified_trees` GUC |

### Vendor PostgreSQL (`vendor/postgres-v17/`)

| File | Changes |
|------|---------|
| `xlogrecovery.c` | `orioledb_recovery.signal` handling, selective WAL replay (rmid filter), consistency marking |
| `xlog.c` | `PerformRecoveryXLogAction` → CheckPoint_hook for OrioleDB, assert relaxations |
| `xlogprefetcher.c` | Skip prefetch errors during OrioleDB recovery |
| `xlogrecovery.h` | `IsOrioleDbRecoveryRequested()` export |
| `relcache.c` | OrioleDB table AM compatibility |

### Neon Stack (Rust)

| File | Changes |
|------|---------|
| `libs/wal_decoder/src/serialized_batch.rs` | rmid=129 FPI → Value::Image; delta → ORIOLEDB_WAL_KEY |
| `libs/wal_decoder/src/decoder.rs` | `MetadataRecord::OrioleDb` variant |
| `libs/pageserver_api/src/key.rs` | `ORIOLEDB_WAL_KEY_PREFIX`, `orioledb_wal_key()` |
| `pageserver/src/walingest.rs` | OrioleDB WAL ingest logging |
| `compute_tools/src/compute.rs` | Recovery orchestration, config injection |
| `pgxn/neon_walredo/` | OrioleDB fallback in wal-redo process |

## 5. PG Patches Inventory

### Neon Modifications (241 files)

Core: smgr hooks (`smgr_hook`), LSN tracking (`set_lwlsn_*` hooks), SLRU
remote reads, prefetch infrastructure, WAL service integration.

### OrioleDB Modifications (208 files)

Core: CSN-based MVCC (`CommitSeqNo`), Table Access Method (TAM), Index AM,
B-tree IOT engine, Undo subsystem, recovery hooks (`CheckPoint_hook`,
`xact_redo_hook`).

### Intersection: ~60 files modified by both

Key conflicts resolved in: `xlog.c`, `xlogrecovery.c`, `xlogprefetcher.c`,
`smgr.c`, `relcache.c`. Both projects use hook-based extensibility, so
changes are largely orthogonal.

## 6. Current Capabilities and Honest Boundaries

### What Works (Page-Level Delta WAL)

B-tree page content is fully covered by page-level WAL:
- Every leaf modification (INSERT/DELETE/UPDATE/LOCK) emits delta WAL
- Every structural operation (SPLIT/MERGE/COMPACT) emits FPI
- PageServer wal-redo can materialize any B-tree page at any LSN
- Checkpoint FPIs provide base images at COW boundaries
- `skip_unmodified_trees = false` is no longer needed

### What Still Requires WAL Replay

**XACT/CSN state** cannot be served by PageServer:
- OrioleDB's MVCC uses CSN (Commit Sequence Number), not PG's CLOG
- The oxid→CSN mapping is maintained in memory (xidBuffer in oxid.c)
- On restart, XACT WAL records (commit/abort) must be replayed to
  rebuild this mapping via `o_xact_redo_hook()`
- `orioledb_recovery.signal` is still needed to trigger XACT-only replay

**Early startup timing**: `shmem_startup_hook` runs before Neon smgr
is initialized, so `AmStartupProcess()` exclusion in `btree_smgr_read()`
remains necessary. Control file and map files are served from PageServer
via Plan E FPIs once smgr is ready.

### Remaining Limitations

1. **XACT replay still needed:** CSN mapping rebuild requires replaying
   RM_XACT_ID records. This is ~5% of WAL volume (vs 100% in old model).
   Eliminating this requires persisting CSN state to PageServer (future work).

2. **SafeKeeper WAL format:** WAL files need page header patching before
   replay (`patch_and_copy_wal_files()` in compute_tools).

3. **Branching/PITR for page content:** Enabled by page-level WAL. But
   XACT state at branch point also needs replay from branch LSN.

4. **PG version coupling:** Both Neon and OrioleDB patch PG internals.
   Major PG version upgrades require rebasing ~60 conflicting files.

5. **Internal page downlinks:** Internal B-tree pages may contain
   in-memory downlinks at FPI emission time. These pages are not served
   via GetPage (they live in page pool), so this is not a practical issue.
