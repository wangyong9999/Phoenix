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

### G2 — Post-restart `SELECT count(*)` returns 0 after clean+SIGKILL paths **OPEN**

**Symptom family:** `crud.sh`, `crash_mid_ckpt.sh`, `crash_savepoint.sh`
all report `before: count=N / after: count=0` across a stop/restart or
crash/restart boundary. Table still exists (catalog OK), tree root
loads from PageServer with correct `itemsCount=N` (internal node with
N downlinks), but `SELECT` returns 0 rows.

**Mode-independence:** reproduces identically in both default (lazy)
and `ORIOLEDB_LEGACY_SIGNAL_RECOVERY=1` paths — **not caused by Phase 3**.

**Known facts (from spike diagnostics):**
- Root page's `evictable_tree_init_meta` reports `itemsCount=35
  level=1` (internal node with 35 downlinks), so root is there.
- PageServer log shows **zero GetPage requests** for user-table
  leaf block numbers during the failing SELECT.
- Local `orioledb_data/<datoid>/<relnode>-1` data file doesn't exist
  (Plan E, expected) — leaves must come from PageServer.

**Leading hypothesis:** 2-slot datafile (`datafileLength[chkpNum%2]`)
vs PageServer single-blkno-per-rel semantics produces stale FPIs
after a chkpNum parity flip. Not yet conclusively proven.

**Impact:** blocks `test_e2e_crash_mid_ckpt` and `test_e2e_crud` from
serving as CI gates. Currently step-level `continue-on-error: true`
in `phoenix-ci.yml`.

**Tracking:** no designated owner yet. Next action: instrument
`btree_smgr_read` + `read_page_from_disk` with elog of `chkpNum`,
`disk_blkno`, and actual page bytes read; compare FPIs sent to
PageServer vs bytes returned at GetPage time.

### G3 — Concurrent-write SIGKILL produces invalid leaf tuples **OPEN**

**Symptom:** `test_e2e_crash_concurrent.sh` at [6/10] SELECT post-restart
hits `Assert("tuplen <= sizeof(dst->fixedData)")` in
`pgxn/orioledb/src/btree/page_contents.c:605 copy_fixed_key`.

**Context:** only under default (lazy) mode, because signal-path
mode hits R10 hang earlier (never reaches [6/10]).

**Hypothesis:** under 4 concurrent `INSERT` backends + mid-workload
SIGKILL, two backends' `orioledb_page_wal_leaf_insert` FPIs both
target the same `(rel, fork, blkno)`. PageServer's last-writer-wins
at that LSN keeps the second, discarding the first. If the second
was emitted before integrating the first's content (due to page
lock boundary + crash timing), the surviving FPI has a half-consistent
item table.

**Not blocking CI** — `test_e2e_crash_concurrent.sh` is not in
phoenix-ci.yml.

**Impact:** concurrent-workload crash safety unproven. Fix likely
requires either commit-serialized FPI emission or a walredo merge
strategy.

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
