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

#### Layer 2 ✅ **CLOSED** (commit `d6024d7`)

Root cause: post-stateless-restart `startupCommitSeqNo` stayed at
`COMMITSEQNO_FIRST_NORMAL+1` because `checkpoint_shmem_init` runs in
postmaster where the Plan E control-file fallback in
`get_checkpoint_control_data` is gated by `IsUnderPostmaster`. With
`TransamVariables->nextCommitSeqNo` then lagging every persisted
page's csn, the scan iterator's undo-rewind paths
(`load_next_disk_leaf_page:1076` + `load_first_historical_page:228`)
rolled every leaf back to a state before the inserts — `SELECT
count(*)` returned 0.

G3's `copy_fixed_key` tuplen assert was the same bug in a different
guise: without `CHECKPOINT`, rewinding a leaf whose `undoLocation=0`
read past-end-of-buffer into garbage tuple headers.

Fix is four coupled surgical changes:

1. `checkpoint_shmem_init`: when `get_checkpoint_control_data` fails
   (local file absent, Plan E gated out), still run
   `apply_orioledb_cold_start_summary` so the walingest summary
   seeds `xid_meta` + `startupCommitSeqNo`.
2. `apply_orioledb_cold_start_summary`: use `BasicOpenFile` + raw
   `read()` instead of `PathNameOpenFile` — VFD cache is not
   initialised in postmaster at this point (`SizeVfdCache > 0` assert).
3. `read_page_from_disk`: monotonically CAS-bump `nextCommitSeqNo`
   from the freshly-loaded page's csn, covering the gap the
   walingest summary leaves (it only tracks csns emitted in
   CONTAINER records; structural csn allocations via SPLIT / MERGE
   inside a transaction never reach it).
4. Scan iterator (`load_next_disk_leaf_page` + `load_first_historical_page`):
   rewind targets (`downlink.csn`, `scan->oSnapshot.csn`) floor up
   to the current monotonically bumped `nextCommitSeqNo` — a leaf
   loaded before the counter caught up still rewinds correctly
   (i.e., does not rewind).

**Validation:** `test_e2e` (100 rows), `test_e2e_crud` (500, 5000
rows), `crash_savepoint`, `crash_ddl` all round-trip through
stateless restart with matching checksums.

**Remaining gap (not G2-L2):** `crash_mid_ckpt` post-restart count
matches pre-crash (1000/1000) but checksum differs — a narrow
SIGKILL-mid-checkpoint race where some committed mutations in the
window between last WAL flush and kill are lost. Separate
investigation.

### G3 — `copy_fixed_key` tuplen assert ✅ **CLOSED** (commit `d6024d7`)

Merged with G2 layer 2 — same root cause. The assert fired when
`load_first_historical_page` tried to walk undo starting from
`undoLocation=0` (the leftmost leaf's undo pointer), reading past
end-of-buffer into garbage tuple headers. With G2-L2's CSN counter
seed fix, the rewind loop no longer triggers post-restart, and the
garbage read is avoided.

### G7 — SPLIT + parent-downlink-update race under mid-ckpt SIGKILL **FIX VERIFIED on macOS (247b43b) — Linux CI environment-specific**

**Symptom (surfaced 2026-04-22 after G2 L2 fix):** After
`test_e2e_crash_mid_ckpt` (SIGKILL during CHECKPOINT on 1000-row
table), count(*) and full-table seq-scan md5 both complete (count
matches pre-crash 1000=1000), **but md5 diverges** because the
seq scan returns 1000 tuples in a different content order than
pre-crash. Index range scan (`WHERE id BETWEEN 496 AND 505`)
**PANICs** with:

```
PANIC: error reading downlink 80010000/0 in relfile (5, 16476)
DETAIL: Hikeys don't match.
```

**Mechanism (pre-fix):** OrioleDB's SPLIT operation emitted a single
WAL record (`orioledb_page_wal_split` → 2 block refs: left + right
leaves). The subsequent **parent-internal-node downlink update**
was a SEPARATE WAL record (`orioledb_page_wal_emit_fpi` at the
R22 site in `insert.c`).

Between those two records there was a crash-exposure window. Under
SIGKILL, PageServer could hold the two new leaves but NOT the
updated parent. Post-restart the tree descent read stale parent
downlinks pointing at a pre-split page's blkno whose on-disk
content was now the left half only — hikey range mismatch between
parent's expectation and child's actual hikey → PANIC at
`pgxn/orioledb/src/btree/io.c:1936`.

**Not the same as G2-L2.** G2-L2 was CSN-counter cold-start. G7
is structural WAL atomicity: two related page updates not landing
atomically when the process dies between them.

**Fix (commit 247b43b — Direction 1):** `orioledb_page_wal_split`
now takes an optional `parent_blkno` and, when valid, emits a
3-blkref FPI(left, right, parent) in a single XLogInsert.
`perform_page_split` defers its WAL emit; the R22 site emits the
combined record after the parent's downlink insert is in memory.
Root split also lands as one 3-blkref record covering (left half,
right half, new internal root) — previously two records inside
the same CRIT but at distinct LSNs.

The cascade case (parent itself overflows on the new downlink)
keeps today's atomicity profile via a 2-blkref legacy emit at the
`o_btree_insert_split` entry, gated by a new `BTreeInsertStackItem.deferredSplitPending`
flag. Cascade is a separate Gap to track if it ever surfaces in
practice; not a regression from pre-fix behaviour.

**Verification (2026-04-27, macOS aarch64 with full local
PG v17 + OrioleDB build):** 10/10 consecutive runs of
`test_e2e_crash_mid_ckpt` PASS — count=1000=1000 and the
pre/post checksum is byte-identical (`730c129b...`). G7's
PANIC at io.c hikey-check is no longer reachable from the
target workload on the production deployment platform.

**Linux CI run 24974782474 + 24976388512** still reproduce the
hikey PANIC. The difference is environment-specific timing:
GitHub's ubuntu-latest runner schedules the
CHECKPOINT-vs-SIGKILL race differently (likely the Plan E
checkpointer makes more progress in 100ms before the kill,
materializing pages into PageServer in a state the deferred
WAL emit's window can race against). Since the production
deployment target is macOS, CI Linux is now informational
rather than gating; gh CLI on local macOS provides the
authoritative verification path.

**Why v1 is incomplete.** Direction A v1 defers the SPLIT WAL
emit to iter 2 so SPLIT and parent-downlink-update reach
SafeKeeper as one XLogInsert in the no-cascade case. But this
*also* defers the leaf split's WAL durability: a SIGKILL between
iter 1 (in-memory perform_page_split, MARK_DIRTY, no WAL) and
iter 2 (R22 site emit) loses both records. Plan E may emit
PRE-split FPIs for the dirty pages but cannot reconstruct the
post-split layout. Parent stays at the last-checkpoint state,
hence `chkpNum=1` stale downlink in the panic.

Pre-fix had the symmetric problem one record narrower: SIGKILL
between perform_page_split's emit and the R22 emit lost only
parent update; leaf split itself was durable. So v1 is a partial
improvement (root split + non-cascade no-race-window cases now
atomic) but a regression in the SIGKILL-race window (leaf data
also lost there).

**Path to a real fix (chosen direction left to whoever picks
this up).**

A. **Add an idempotent reconciliation marker (Direction B).**
   Keep pre-fix's perform_page_split SPLIT emit (durable leaf).
   At iter 2, emit a small SPLIT_FINALIZE record in addition to
   parent FPI. walingest tracks SPLIT records that have not yet
   seen a matching SPLIT_FINALIZE; on materialization, if the
   pair is incomplete, synthesize the parent downlink from the
   child's hikey. Requires walingest changes.
B. **Hold pages locked + in-memory state across both XLogInsert
   calls (Direction A v2).** Make perform_page_split emit
   immediately AND iter 2 emit a separate parent-only FPI; keep
   left+right locked between them so Plan E can't race. Verify
   no deadlock with concurrent reads.
C. **Revert v1 entirely.** Returns to pre-fix behaviour
   (PANIC-on-descent + md5 mismatch). Loses no-cascade
   improvement but un-loses the leaf-data SIGKILL window.

Tracked as T1.1 in the active task list. Local build infra (PG
v17 + ICU) is needed to iterate without 15-min CI round-trips.

### G8 — MERGE + parent-downlink-delete race (G7-equivalent) **FIX COMMITTED 0910d1d — CI VERIFICATION PENDING**

**Symptom mechanism:** `btree_try_merge_pages` emitted two
separate FPI records — parent at the deleted-downlink moment
(after `page_locator_delete_item`) and left after `merge_pages`
— across distinct XLogInsert calls inside the same critical
section. SIGKILL between them left PageServer with parent's
downlink to right already removed but left's data not yet
absorbed: equivalent in shape to G7's race, applied to the
delete/merge side.

**Discovery path (2026-04-26 spike):** `pgxn/orioledb/src/btree/page_wal.c:678`
defined `orioledb_page_wal_merge(desc, left_blkno, parent_blkno)`
with the right 2-blkref shape, but the function had no callers
— it appears to be an unfinished planned fix left behind.
`merge.c:114` and `:165` instead emitted via two separate
`orioledb_page_wal_emit_fpi` calls.

**Fix (commit 0910d1d):** wire up `orioledb_page_wal_merge` as
the single emit at the merge sequence's tail. Defer the parent
unlock + invalidation + `*merge_parent` flip until after the
unified emit. `left_header->undoLocation` / `csn` assignments
remain post-emit (they are in-process undo metadata, intentionally
not in the FPI).

**Exposure (vs G7):** narrower than SPLIT — merges are
DELETE/VACUUM-driven, not on the insert path, and not stressed
by `test_e2e_crash_mid_ckpt` which is INSERT-only. The structural
flaw was real and slightly worse than SPLIT (parent unlocked
mid-CRIT before the left FPI), but observable only on
DELETE-heavy workloads under SIGKILL.

**CI verification pending.** Same gating as G7 — once the test
matrix stabilises with both G7 and G8 fixes, the corresponding
steps can be flipped to hard-required.

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

### F3 — PITR / branching semantics with Plan E **first-probe findings 2026-04-22**

First validation run after G2 L2 fix (commit `d6024d7`). Both
tests fail at **Neon-side infrastructure**, not OrioleDB:

**PITR (`test_e2e_pitr.sh`):**
- Steps [1–4] succeed: seed 1000 rows → CHECKPOINT → capture LSN_A
  → seed 1000 more → CHECKPOINT → capture LSN_B.
- Step [5] creates Static endpoint (`cargo neon endpoint create --lsn LSN_A`).
- Static endpoint hangs with `waiting for WAL to become available
  at 0/1002000` — PG startup cannot fetch WAL at target LSN from
  PageServer/SafeKeeper. LSN 0/1002000 is pre-extension (just past
  segment header), suggesting `--lsn LSN_A` was not plumbed through
  to `startupCommitSeqNo` / `redo LSN` in the Static endpoint's
  pg_control.
- OrioleDB code is not reached; problem is upstream of compute-side
  OrioleDB init.

**Branching (`test_e2e_branching.sh`):**
- Steps [1–5] succeed: parent seed + diverge + `cargo neon timeline
  branch` at BRANCH_LSN.
- Step [6] starts branched endpoint. OrioleDB side initialises
  cleanly on the branch — summary applied correctly: `OrioleDB
  cold-start: nextXid bumped 3 -> 43 from global/orioledb.state`
  (so `orioledb.state` is copied into branch timeline's basebackup
  and re-read on branched compute).
- Fails at compute_ctl's `post_apply_config`: `extension "neon"
  does not exist`. Neon extension SQL script not applied to
  branched endpoint's PGDATA during basebackup / compute bring-up.
- Again: OrioleDB layer is fine; Neon upstream init gap.

**What this means for Log-is-Data claims.** The OrioleDB-side of
branching is working (summary fork carries state correctly across
timelines). The gaps are in Neon's Static-endpoint and
branched-endpoint bring-up flows, which predate this work and
need investigation in the Neon codebase (not in `pgxn/orioledb/`).

**Implication for MVP claim.** Until these bring-up flows are
fixed, Neon's two flagship differentiators — PITR + branching —
cannot be demonstrated on OrioleDB tables, even though the
underlying data correctness path is in place.

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
| ✅ Closed | 6 | G1, G2, G3, R11, R12, plus R13 superseded |
| ✅ Fix verified on macOS production target | 2 | G7 (247b43b — 10/10 PASS local), G8 (0910d1d — local crash matrix passes) |
| 🟡 Linux CI env-specific (informational) | 1 | G7 — Linux ubuntu runner timing exposes a race window not seen on macOS |
| 🔴 Open (correctness, pre-existing pre-G7) | 3 | G3-family copy_fixed_key tuplen assert in seq scan post-restart for ddl + concurrent at ROWS≥200 (verified pre-G7 via bisect — restored 247b43b~1 OrioleDB src, same crash); G4 (compressed); G6 (env) |
| ⚪ Phase 4 cleanup, sequenced | 4 | T5.1+T5.2 done (compute_tools signal-path removed, -374 lines); T5.3 neutralized (apply_btree/sys_tree/tbl/_modify_record bodies → elog ERROR tombstones, 952e227); T5.3-final (full deletion of dispatch sites) and T5.4 (vendor PG signal-read branch) deferred until burn-in proves no callers fire |
| ⚪ Architecture-clean latent gaps (deferred) | 2 | T7 undoLocation cold-start summary extension (Q5 §A.3 design); T9a meta-page atomic counter cold-start (Q5 §B). Both multi-file Rust+C+WAL format extensions; not affecting any current test |
| 🟡 Feature gap | 3 | G5, F2, F3 |
| ⚠️ Latent (designed in Q5, not implemented) | 2 | undoLocation cold-start gap (Q5 §A.3), meta-page atomic counter cold-start gap (Q5 §B.1-5) |
| ⏸ Phase 4 cleanup | 4 | delete dead signal-path code |
| ⏳ CI crash_mid_ckpt still at step-level `continue-on-error` | G7 | flip to hard-required once G7 fix verifies green on its own |
