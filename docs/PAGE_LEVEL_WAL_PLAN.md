# OrioleDB Page-Level WAL: Implementation Plan

> **Status:** Complete — Phase 1-5 implemented
> **Created:** 2026-04-16
> **Last updated:** 2026-04-16

## 1. Background & Motivation

### 1.1 Current State (Plan E — Checkpoint Shipping)

OrioleDB on Neon currently achieves "basic serverless" via Plan E:
- Checkpoint emits Full Page Images (FPIs) for all B-tree pages
- PageServer stores them as `Value::Image`
- On restart, compute reads pages from PageServer via `GetPage`

**Limitations:**
- Data freshness = last checkpoint interval (not last commit)
- PageServer cannot materialize pages between checkpoints
- Branching/PITR not supported (no per-modification WAL)
- `skip_unmodified_trees = false` causes O(total_data) WAL per checkpoint

### 1.2 Target State (Page-Level Delta WAL)

Convert OrioleDB's WAL from tree-level operations to page-level deltas,
matching PG's native btree WAL model (nbtxlog.c). This enables:
- Zero data-loss window (every modification in WAL)
- PageServer wal-redo for OrioleDB pages
- Branching at any LSN
- PITR at commit granularity

### 1.3 Key Design Insight: COW Is Not a Blocker

OrioleDB uses COW (Copy-on-Write) checkpoints where dirty pages are
relocated to new disk offsets. Initial analysis flagged this as a
"stable block number" problem, but deeper analysis shows:

- **Between checkpoints:** `extent.off` is stable — usable as `blkno`
- **At checkpoint:** COW relocation produces a new `extent.off` — emit FPI
  as new base image (analogous to PG's `full_page_writes` after checkpoint)
- **New pages (from split):** Use `REGBUF_WILL_INIT` (entire page in WAL)
  or pre-allocate extent at split time

The COW checkpoint naturally provides base-image boundaries for delta chains.

### 1.4 OrioleDB IOT In-Memory Pointer Design

OrioleDB's IOT (Index-Organized Table) uses dual addressing:
- **In-memory:** `OInMemoryBlkno` (uint32 buffer pool index) used as
  parent→child pointers, avoiding hash lookups for tree navigation
- **On-disk:** `FileExtent.off` (48-bit byte offset) in downlinks,
  stable within a checkpoint epoch

WAL records reference the on-disk `extent.off` as BlockNumber.
In-memory `OInMemoryBlkno` is never persisted to WAL.

## 2. WAL Record Type Catalog

| Type | Info Byte | Blocks | Strategy | Source |
|------|-----------|--------|----------|--------|
| PAGE_INIT | 0x10 | 1 (WILL_INIT) | FPI | btree.c:53 |
| LEAF_INSERT | 0x20 | 1 | Delta | insert.c:923+ |
| LEAF_DELETE | 0x30 | 1 | Delta | modify.c:871 |
| LEAF_UPDATE | 0x40 | 1 | Delta | modify.c:515,678 |
| LEAF_LOCK | 0x50 | 1 | Delta | modify.c:946 |
| COMPACT | 0x60 | 1 | FPI | insert.c:1004 |
| SPLIT | 0x70 | 2-3 | FPI (all) | split.c:454-455 |
| MERGE | 0x80 | 2-3 | FPI+Delta | merge.c:110,158 |
| ROOT_SPLIT | 0x90 | 3 | FPI (all) | insert.c:246-247 |
| UNDO_APPLY | 0xA0 | 1 | FPI | undo.c:252,688 |

**Strategy rationale:**
- Single-page modifications (INSERT/DELETE/UPDATE/LOCK): delta is smaller
  than FPI (~200-500 bytes vs 8KB)
- Structural operations (SPLIT/MERGE/COMPACT/ROOT_SPLIT): page is fully
  reorganized, delta would be as large as FPI — use FPI for simplicity
- UNDO_APPLY: complex state restoration — FPI is safest

## 3. Block Number Mapping

```
blkno = page_desc->fileExtent.off   (stable within checkpoint epoch)

RelFileLocator:
  spcOid    = DEFAULTTABLESPACE_OID
  dbOid     = desc->oids.datoid
  relNumber = desc->oids.relnode
  forkNum   = MAIN_FORKNUM

Special cases:
  New page (no extent yet):  pre-allocate extent at creation, or WILL_INIT
  COW-relocated page:        FPI at new extent.off = new base image
  Compressed page:           FPI only (no delta WAL for compressed pages)
```

## 4. Phased Implementation

### Phase 1: Foundation (Week 1-2)
- WAL record struct definitions (page_walrecord.h)
- blkno mapping functions (page_wal.c)
- Redo function skeleton (page_redo.c)
- Round-trip test framework
- **Milestone:** round-trip test passes for LEAF_INSERT

### Phase 2: Single-Page Delta WAL (Week 3-4)
- LEAF_INSERT emission + redo
- LEAF_DELETE emission + redo
- LEAF_UPDATE emission + redo
- LEAF_LOCK emission + redo
- COMPACT emission (FPI)
- **Milestone:** deltas arrive at PageServer, decodable

### Phase 3: Multi-Page Atomic WAL (Week 5-6)
- SPLIT emission (2-3 FPIs in one record)
- MERGE emission
- ROOT_SPLIT emission
- UNDO_APPLY emission (FPI)
- Pre-allocate extent for split right page
- **Milestone:** SPLIT pages visible in PageServer

### Phase 4: wal-redo Integration (Week 7-8)
- orioledb.so light-mode (skip shmem init in wal-redo)
- RMGR registration in wal-redo
- wal_decoder: route deltas as Value::WalRecord (not skip)
- seccomp adjustments
- Fallback: delta→Image at ingest if wal-redo fails
- **Milestone:** kill→restart→SELECT works without compute WAL replay

### Phase 5: Cleanup & Honest Accounting (Week 9-10)
- Remove skip_unmodified_trees=false — no longer needed with page-level WAL
- Narrow WAL replay to XACT-only — skip rmid=129 (pages from PageServer)
- KEEP orioledb_recovery.signal — still needed for XACT CSN replay
- KEEP AmStartupProcess exclusion — shmem_startup precedes Neon smgr init
- Update documentation with honest capability boundaries
- **Milestone:** restart uses PageServer for pages, WAL replay only for CSN

## 5. Invariants (Must Hold Throughout)

1. Row-level WAL (ORIOLEDB_XLOG_CONTAINER) always emitted — compute crash recovery
2. COW checkpoint FPIs always emitted for relocated pages — base images
3. Plan B (undo/xidmap mirroring) unchanged
4. Non-Neon mode behavior unchanged (#ifdef or runtime check)
5. Compressed pages use FPI path (no delta WAL)

## 6. Fallback Strategy

If Phase 4 (wal-redo integration) hits blockers:
- Keep Phase 2-3 page-level WAL emission
- wal_decoder materializes delta+base into Value::Image at ingest time
- Result: "high-frequency FPI" — better than checkpoint-only, no wal-redo needed

## 7. Historical Audit

### Prior approaches (superseded by this plan):

| Approach | Date | Outcome |
|----------|------|---------|
| Plan E (Checkpoint FPI) | 2026-04-13 | Working — basic serverless achieved |
| Plan B (Undo mirroring) | 2026-04-14 | Working — undo/xidmap via PageServer |
| Snapshot save/restore | 2026-04-12 | Removed — superseded by Plan E/B |
| WAL replay (recovery.signal) | 2026-04-13 | Working but transitional |
| FPI-on-Write (Path B) | 2026-04-16 | Analyzed — viable but high write amplification |
| Page-level delta WAL (Path C) | 2026-04-16 | **Selected** — this plan |

### Key design decisions and rationale:

1. **COW ≠ unstable blkno** — COW relocates pages at checkpoint, but
   checkpoint = FPI emission = natural base-image boundary. Between
   checkpoints, extent.off is stable.

2. **SPLIT uses FPI, not delta** — page reorganization (btree_page_reorg)
   changes entire page structure, delta would be larger than FPI.

3. **Two WAL streams coexist** — row-level for compute, page-level for
   PageServer. Both always emitted in Neon mode.

4. **Compressed pages excluded from delta** — compression changes extent
   granularity (ORIOLEDB_COMP_BLCKSZ vs ORIOLEDB_BLCKSZ), adding
   complexity without proportional benefit.
