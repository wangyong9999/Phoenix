# N8 — Crash-Window Test Matrix

> **Purpose:** pin down Log-is-Data correctness under every realistic crash
> window. Each script exits 0 on PASS, nonzero on FAIL; CI runs them as
> discrete gates so a regression in one scenario doesn't mask others.

Plan reference: `ENTERPRISE_HARDENING_PLAN.md` §N8 (scripts N8.1–N8.7).

## Invariants every script must honour

1. Reset state at start (`cargo neon stop`; `rm -rf .neon`) so the scenario
   is isolated from prior runs.
2. After `SIGKILL`, wipe only `$PGDATA`, not the endpoint dir. This is the
   realistic Neon stateless-restart boundary — endpoint config persists,
   compute pgdata is treated as cache.
3. After restart, reconnect and read the invariant (count, checksum) that
   the scenario pins down. Compare against the pre-crash expected value.
4. On failure, emit the full `.neon/` log dump (same mechanism as
   `test_e2e_crash_mid_ckpt.sh`) so CI captures enough to debug.

## Scripts

### N8.1 — Mid-commit SIGKILL (already covered by `test_e2e_crash_mid_ckpt.sh`)
- Setup: `CREATE TABLE crash_verify USING orioledb; INSERT N rows`
- Midway: `CHECKPOINT; INSERT N more rows; race CHECKPOINT with SIGKILL`
- Post-crash: `SELECT count(*)` must equal `2N`.
- **Status**: script exists, currently failing with `count=0`.

### N8.2 — Compressed tables (R9)
- Setup: `CREATE TABLE crash_verify (…) USING orioledb WITH (compress = 5)`
- Flow: same as N8.1 but the table uses `ORIOLEDB_COMP_BLCKSZ` pages.
- Post-crash: `SELECT count(*)` must equal `2N`.
- **Risk pin-down**: compressed pages have a different extent-granularity
  (`ORIOLEDB_COMP_BLCKSZ` vs 8 KB), and Plan E's FPI emit path takes a
  different branch (io.c:1684+ else-clause). N8.2 proves that branch is
  actually exercised under a crash scenario, not just the uncompressed
  fast path.

### N8.3 — 2PC prepared transaction + SIGKILL (R10)
- Setup: `CREATE TABLE crash_verify USING orioledb; INSERT N rows`
- Flow:
  1. `BEGIN; INSERT N more rows; PREPARE TRANSACTION 'x';`
  2. SIGKILL compute before `COMMIT PREPARED`.
- Post-crash: prepared txn should still be pendable via
  `pg_prepared_xacts`; `COMMIT PREPARED 'x'` should succeed and the
  count should equal `2N`.
- **Risk pin-down**: OrioleDB's M1.2/M1.3 commit-barriers are in
  `current_oxid_commit`, which is the normal commit path. Prepared-txn
  commits go through `FinishPreparedTransaction` → a different
  `current_oxid_commit` invocation. N8.3 proves that path is also covered.

### N8.4 — SAVEPOINT + ROLLBACK TO + SIGKILL (R11)
- Setup: `CREATE TABLE crash_verify USING orioledb; INSERT N rows`
- Flow:
  1. `BEGIN; SAVEPOINT s1; INSERT N more rows; ROLLBACK TO s1;`
  2. `INSERT 1 sentinel row; COMMIT;`
  3. SIGKILL mid-flight (e.g., during a second txn's CHECKPOINT).
- Post-crash: `SELECT count(*)` must equal `N + 1`. No rows from the
  rolled-back savepoint should be visible.
- **Risk pin-down**: OrioleDB uses undo chains for subtxn rollback
  (`autonomousNestingLevel`). N8.4 proves the undo chain survives the
  crash correctly.

### N8.5 — xidmap wraparound pressure (R13)
- Setup: create a table, consume OIDs by advancing xidmap near its
  wraparound point via `orioledb_advance_xidmap_for_testing(N)` (may
  need a test-only GUC/function to be added).
- Flow: commit M rows, SIGKILL, restart.
- Post-crash: rows still visible; no stale-CSN misread.
- **Risk pin-down**: xidmap is backed by synthetic relation with
  circular buffer + o_buffers Plan B mirror. N8.5 covers the wraparound
  corner case.
- **Prerequisite**: test-only injection point to fast-forward xidmap
  (implementing that is a subtask under N8.5).

### N8.6 — Mid-checkpoint SIGKILL at several fractions (R5)
- Setup: same as N8.1 but inject a failpoint in `o_perform_checkpoint`
  at the start of each tree's checkpoint stage; fire SIGKILL when the
  failpoint hits.
- Variants: fail right after sys-tree pre-loop, fail after first user
  tree, fail mid-user-tree, fail right before `checkpoint_chkp_nums`.
- Post-crash: data invariant holds regardless of where the crash fired.
- **Risk pin-down**: R5 (mid-checkpoint partial FPIs) — prove
  idempotency across all phases of a checkpoint.
- **Prerequisite**: failpoint hook in `o_perform_checkpoint` (add under
  `#ifdef USE_INJECTION_POINTS`).

### N8.7 — Concurrent commits + SIGKILL mid-burst
- Setup: `CREATE TABLE crash_verify USING orioledb` with a PK.
- Flow: 20 parallel psql sessions each commit `M/20` rows via
  `INSERT ... ON CONFLICT DO NOTHING`; SIGKILL at a pseudo-random offset
  inside the burst.
- Post-crash: surviving rows must be consistent. The exact count depends
  on when the SIGKILL hit, but the md5 of the returned rows must be a
  prefix of the expected full-insert md5 sequence.
- **Risk pin-down**: R7 (concurrency at replay) and R17 (commit-barrier
  contention).

## Harness

All scripts share a `scripts/lib_e2e_common.sh` that bundles:
- `neon_reset`, `neon_boot`, `neon_endpoint_create`, `neon_endpoint_start`
- `run_psql` helper with `ON_ERROR_STOP`
- `wait_for_psql` with configurable timeout
- `compute_pid` (reads `.neon/endpoints/$NAME/pgdata/postmaster.pid`)
- `dump_logs_on_fail` trap
- `assert_equal`, `assert_count`

That harness lets each scenario stay ≤100 lines, readable. Adding a new
scenario should be a matter of: copy template, describe setup, describe
crash point, describe invariant.

## CI gating

Add each passing script to `.github/workflows/phoenix-ci.yml` under a
separate CI step, so CI reports which scenario regressed rather than a
single monolithic failure.

## Dependencies

- N8.1: no dependency (already runs; failing).
- N8.2: passes iff R9 is also passing.
- N8.3: independent of 6.6.4c-3.
- N8.4: independent of 6.6.4c-3.
- N8.5: needs injection point work first.
- N8.6: needs failpoint work first.
- N8.7: depends on concurrent harness (already exists as
  `test_e2e_concurrent.sh`; extend with SIGKILL arm).

Ordered by ROI: **N8.3, N8.4, N8.2, N8.7, N8.6, N8.5**. The first two are
quick to write and exercise distinct code paths (2PC + subtxn undo) that
the current 6.6.4 gate doesn't touch at all.
