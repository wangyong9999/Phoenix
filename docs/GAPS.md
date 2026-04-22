# Known Gaps — OrioleDB on Neon

Single source of truth for open bugs + feature gaps. Updated 2026-04-22.

---

## Gap ID 格式

- **G-prefixed** = core bugs (block correctness)
- **R-prefixed** = risks in the execution-plan register (see
  `docs/EXECUTION_PLAN.md` §6)
- **F-prefixed** = feature gaps (OrioleDB doesn't support something PG does)

## G-Gaps (correctness bugs)

### G1 — Tree manifest not durable outside checkpoint ✅ **CLOSED**

Closed by commit `9f1bfed` (B.5 — emit INIT fork block 0 FPI at
o_btree_init). See `docs/B5_SUMMARY_V3_SCHEMA.md` for analysis and
`docs/P3_PREFLIGHT_AUDIT.md` for the empirical basis.

### G2 — Post-restart `SELECT count(*)` returns 0 **OPEN (partially dissected)**

**Symptom family:** `crud.sh`, `crash_mid_ckpt.sh`, `crash_savepoint.sh`
all report `before: count=N / after: count=0` across a stop/restart or
crash/restart boundary. Table still exists (catalog OK), tree root
loads from PageServer with correct `itemsCount=N` (internal node with
N downlinks), but `SELECT` returns 0 rows.

**Mode-independence:** reproduces identically in both default (lazy)
and `ORIOLEDB_LEGACY_SIGNAL_RECOVERY=1` paths — **not caused by Phase 3**.

#### Layer 1 — SPLIT/MERGE blkno collision (**closed commit `6d852c6`**)

`evictable_tree_init_meta` called `o_btree_init` (which after B.5
allocates root extent off=0 and bumps `datafileLength[0]` to 1),
then unconditionally wrote `file_header.datafileLength` (==0 for
fresh tree) back — clobbering the root's reservation. Next
ensure_extent handed out offset 0 again. SPLIT emission saw
`left.off == right.off == 0` and the prior R11 bandaid (two
single-block PAGE_IMAGE records at the same PageServer key) lost
data via last-writer-wins.

Fix: `Max(live, file_header.datafileLength)` preserves root's
extent. R11 bandaid removed; replaced with `Assert(left_disk !=
right_disk)` so any new clobber fails loudly.

Post-fix: zero collision warnings in CRUD workload.

#### Layer 2 — Post-restart count=0 persists **OPEN**

Even with layer-1 closed, `SELECT count(*)` returns 0 across
restart.

**Empirical findings (2026-04-22 session):**

- Reproduces at `ROWS=500` (PK tree grows to level-1, ~10 pages).
  Passes at `ROWS=100` (PK tree stays level-0, 2 pages).
  Threshold implies bug triggers once multi-split forces an
  internal node / root-split path.
- **G2 layer 2 and G3 share the same underlying bug.** Same
  workload (500 INSERTs) without `CHECKPOINT` → hits G3's
  `copy_fixed_key` tuplen assert on first post-restart SELECT.
  With `CHECKPOINT` → silently returns 0 rows (scan still
  traverses but filters everything out, or traverses into empty
  pages). Single-backend — not concurrency-specific as G3's
  original report suggested.
- **`write_page_to_disk` is NEVER called during SQL CHECKPOINT
  in Neon mode.** Gate at `pgxn/orioledb/src/btree/io.c:1673` was
  never hit during instrumented CRUD runs. PageServer sees ONLY
  per-operation FPIs from `orioledb_page_wal_leaf_insert`,
  `orioledb_page_wal_split`, `orioledb_page_wal_emit_fpi`
  (R22 internal-node path at `insert.c:1198`, COMPACT at
  `insert.c:1255`, UNDO_APPLY paths).
- Post-restart PageServer reads at PK tree (blknos 5..9 of 10
  nblocks) return pages with valid on-disk headers
  (`checkpointNum=1, page_version=1`). Content problem is in the
  body (item table / tuples), not the header.

**Consumer-side probe findings (same session):**

Dumped full `BTreePageHeader` body (flags, level, itemsCount,
chunksCount, dataSize, undoLocation, csn) from `btree_smgr_read`
on every post-restart full-page smgr fetch:

- PK tree (5,16476), 500-row workload:
  - blkno=7: root, level=1, itemsCount=3 (3 downlinks) ✓
  - blkno=4: leaf, itemsCount=172, csn=30 ✓
  - blkno=5: leaf, itemsCount=170, csn=31 ✓
  - blkno=6: leaf, itemsCount=158, csn=31 ✓
  - **Total 172+170+158=500 tuples present in materialized pages**.
- Root downlinks correctly resolve to leaf blknos; tree traversal
  reaches all three data leaves.
- `oxid_get_csn` instrumentation (static probe counter at
  `oxid.c:1648`) logs 0 lookups from the post-restart backend
  during the failing `SELECT count(*)`. **The scan doesn't
  reach the per-tuple visibility check at all.**

**Sharpened hypothesis (shifted from I3 to scan-layer):**

Bug is in the **scan iterator** — tree descent reaches leaves
(verified by PageServer read traffic) but the sequential scan
returns zero rows without invoking visibility. Suspect areas:

- `init_btree_seq_scan` / `init_checkpoit_number` at
  `pgxn/orioledb/src/btree/scan.c:1106-1146` — if `numSeqScans`
  arithmetic or `checkpointNumber` acquisition misreads
  `metaPage` under the fresh-compute, fresh-shmem init sequence,
  subsequent page-load may skip items.
- `init_page_find_context(... scan->oSnapshot.csn ...)` at
  `scan.c:1245` — scan's snapshot CSN initialization post-restart
  may land on a sentinel that filters all items before the
  per-tuple `oxid_get_csn` hook.
- `page_load_and_fix` equivalent — once a page is loaded,
  `read_page_from_disk` zeros `o_header[0..16)` and restores only
  `checkpointNum`. In-memory `state=0, pageChangeCount=0`. If the
  scan iterator asserts on `pageChangeCount` matching an earlier
  captured value (e.g. from downlink), it may silently bail.

**Rejected hypotheses (from this session):**

- 2-slot `datafileLength[chkpNum%2]` vs single-blkno-per-rel
  keyspace — in non-S3 mode the checkpoint COW emit path is
  never reached, so no slot-collision opportunity exists.
- FPI-emit content corruption (split / R22 internal-node) — ruled
  out by consumer probe: pages materialize with correct items.
- MVCC / commit-barrier / xidmap gap (I5) — ruled out by
  `oxid_get_csn` probe: visibility check is never invoked.

**Impact:** blocks `test_e2e_crash_mid_ckpt` / `test_e2e_crud` from
being CI hard gates. Current step-level continue-on-error in
phoenix-ci.yml.

**Next action.** Instrument `init_btree_seq_scan` entry and the
iterator's per-page "should we iterate items" decision. Compare
first-session vs. post-restart values of `scan->checkpointNumber`,
`scan->oSnapshot.csn`, and any early-exit paths. 100-row case
passes → baseline for diffing the exact breakpoint at 500-row.

### G3 — `copy_fixed_key` tuplen assert on post-restart SELECT **OPEN**

**Symptom:** Post-restart SELECT hits
`Assert("tuplen <= sizeof(dst->fixedData)")` in
`pgxn/orioledb/src/btree/page_contents.c:605 copy_fixed_key`.

**Reproduction (revised 2026-04-22):** single-backend workload of
500 INSERTs + stateless restart (no `CHECKPOINT`) triggers this
assert — originally reported under `test_e2e_crash_concurrent.sh`
but NOT concurrency-specific. Adding a `CHECKPOINT` before the
restart masks the assert and instead yields G2-layer-2 silent
count=0. Same underlying bug in two guises.

**Hypothesis (revised):** FPI emitted by the split path
(`orioledb_page_wal_split`) or the R22 internal-node downlink path
(`orioledb_page_wal_emit_fpi` at `insert.c:1198`) captures an
internally inconsistent page — item count / tuple-size header /
data region mismatch — and a post-restart scan dereferences a
tuple whose declared length exceeds the fixed key buffer.

**Impact:** merged with G2 layer 2 — fixing either fixes both.

### G4 — `test_e2e_crash_compressed` checkpointer assert **OPEN**

**Symptom:** `TRAP: failed Assert("cur->extent.offset < extent.off")`
in `src/catalog/free_extents.c:341`, raised inside checkpointer
after control-file FPI emitted.

**Mode-independence:** reproduces in both default and legacy modes
identically — pre-existing compressed-table interaction with the
free-extents tree.

**Not blocking CI** — not in phoenix-ci.yml.

**Impact:** compressed tables under crash/restart can't be validated.
Likely an invariant violation in `free_tree_{off_len,len_off}` COW
paths.

### G5 — OrioleDB doesn't support `PREPARE TRANSACTION` **OPEN / FEATURE GAP**

**Error:** `cannot use PREPARE TRANSACTION in transaction that uses
orioledb table`.

**Category:** feature gap, not a bug. 2PC not implemented for
orioledb-storage tables.

**Impact:** `test_e2e_crash_2pc.sh` cannot run to completion under
any mode. Test script itself remains for future coverage once
feature lands.

### G6 — compute_tools chrono OutOfRangeError at `compute.rs:1036` **OPEN / ENV**

**Symptom:** panic with `OutOfRangeError(())` on
`startup_end_time.signed_duration_since(compute_state.start_time)
.to_std().unwrap()`.

**Environmental:** happens intermittently on WSL2 dev hosts with
clock skew between compute_state initialisation and postmaster
start. Probably won't reproduce in CI.

**Fix candidate:** replace `.unwrap()` with `.unwrap_or_default()`
or guard against negative durations.

---

## R-Gaps (risks from EXECUTION_PLAN.md)

### R10 — crash_concurrent end-of-recovery checkpoint hang **PARTIALLY OBSOLETE**

Original: sys-tree (1,8) CLASS_CACHE `checkpoint_ix` hang during
signal-path end-of-recovery checkpoint.

**Post-阶段-3b status:** not reached under default (lazy) mode
because no end-of-recovery checkpoint fires. Reachable only when
user explicitly opts back in via `ORIOLEDB_LEGACY_SIGNAL_RECOVERY=1`.
Will be fully obsoleted by Phase 4 (delete signal-path code).

### R11 — SPLIT/MERGE FPI same-blkno collision ✅ **CLOSED** (commit `dcd452b`)

### R12 — WSL2 HTTP proxy hijacks localhost probe ✅ **CLOSED** (commits `f9dd441`, `f98d588`)

### R13 — force map-file write on signal-path EoR 🟡 **SUPERSEDED**

Partial fix (commit `d49cf21`) landed. Rendered mostly moot by
B.5 (commit `9f1bfed`) + 阶段 3b default flip. Will be removed
in Phase 4 cleanup alongside signal-path.

---

## F-Gaps (feature gaps, not bugs)

### F1 — 2PC for orioledb tables

See G5 above.

### F2 — Physical replication for orioledb tables

`test_e2e_physrepl.sh` presumably exercises this but hasn't been
part of recent validation matrix. Status unknown.

### F3 — PITR / branching semantics with Plan E

`test_e2e_pitr.sh`, `test_e2e_branching.sh` — interaction with Plan E's
`checkpoint_map_write_header` + basebackup flow not validated.

---

## Phase 4 cleanup candidates (not bugs; refactors)

- **Delete** `apply_btree_modify_record` and the CONTAINER compute-side
  replay worker pool (`recovery/worker.c:674-678` etc.) — post-Phase-3b
  these are unreachable code paths.
- **Delete** the `orioledb_recovery.signal` read branch in vendored PG
  (`vendor/postgres-v17/src/backend/access/transam/xlog.c:5490` block
  and `xlogrecovery.c:820-850`).
- **Delete** compute_tools' signal-path helpers:
  `patch_and_copy_wal_files`, `write_orioledb_recovery_signal`,
  `.orioledb_sync_lsn` diag logging.
- **Delete** OrioleDB side `IsOrioleDbRecoveryRequested` branches in
  `checkpoint.c`, `recovery.c`, `btree/io.c`.

Prerequisite: stabilisation period (≥ 1 week in lazy default, G2+G3
tracked separately).

---

## Quick scoreboard

| Category | Count | Notes |
|---|---|---|
| ✅ Closed | 4 | G1, R11, R12, plus R13 superseded |
| 🔴 Open (correctness) | 4 | G2, G3, G4, G6 |
| 🟡 Feature gap | 3 | G5, F2, F3 |
| ⏸ Phase 4 cleanup | 4 | delete dead signal-path code |
| ⏳ CI lifted to hard-required after | G2 fix | then flip step-level `continue-on-error` |
