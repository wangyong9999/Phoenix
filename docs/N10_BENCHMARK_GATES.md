# N10 — Benchmark Gates

> **Purpose:** enforce that the Log-is-Data work does not silently make
> OrioleDB-on-Neon worse than PG-heap-on-Neon. Every phase that ships
> adds mechanism (commit-barrier flushes, extra FPIs, recovery paths).
> Without gates, these accumulate and we ship a regression.
>
> Plan reference: `ENTERPRISE_HARDENING_PLAN.md` §N10.

## Principle

For each baseline below, **OrioleDB-on-Neon ≤ PG-heap-on-Neon + budget**.
If a change makes us exceed the budget, it doesn't ship until either
(a) the change is redesigned to fit, or (b) the budget is relaxed in
writing with a reviewer explicitly acknowledging the cost.

The budgets are aggressive on purpose: they force us to favor
amortisation (commit groups, per-page de-dup) over raw FPI-per-mutation.

## Gates

### N10.1 — Write WAL bytes per INSERT
- Workload: `test_e2e_crud.sh` — 10k single-row INSERT commits.
- Measured: `(wal_advance_bytes) / 10000`.
- Source: compute-side `pg_current_wal_insert_lsn()` delta.
- **Budget**: OrioleDB-on-Neon ≤ 1.30 × PG-heap-on-Neon.
- Rationale: N2 adds one FPI per mutation (8 KB) + M1.2/M1.3 undo +
  xidmap flush. PG heap is ~512 B + XACT_COMMIT. With commit-group
  amortisation (N2.6) the per-mutation FPI cost should drop; until
  then we pay 8 KB × leaf-touch rate. 30% budget absorbs the common
  case of 1 row / 1 leaf with batching.

### N10.2 — Commit latency p50/p99
- Workload: 256-client pgbench-style INSERT loop, 5 min.
- Measured: `\timing` on each COMMIT statement.
- **Budget**:
  - p50 OrioleDB ≤ 1.15 × PG-heap p50
  - p99 OrioleDB ≤ 1.25 × PG-heap p99
- Rationale: commit-barrier adds a fixed 2-3 ms (one XLogFlush of
  the barrier FPIs). p99 budget is looser because R17 contention can
  spike on hot pages.

### N10.3 — Restart time for 10 GB tenant
- Setup: tenant with 10 GB OrioleDB data, 10k committed rows past the
  last checkpoint.
- Measured: wall time from `cargo neon endpoint start main` to
  `SELECT 1` returning.
- **Budget**: OrioleDB ≤ 2 × PG-heap restart time.
- Rationale: OrioleDB's L3 "recovery minimal" means basebackup +
  PageServer reads; no compute-side replay. Should be close to PG
  heap. 2× budget is permissive for first cut; tighten post-N5.

### N10.4 — Steady-state throughput
- Workload: pgbench `INSERT`-only, 64 clients, 10 min.
- Measured: TPS.
- **Budget**: OrioleDB TPS ≥ 0.90 × PG-heap TPS.
- Rationale: commit-barrier overhead should amortise across the
  batch; steady-state TPS should not drop more than 10%.

### N10.5 — Checkpoint WAL volume
- Workload: single CHECKPOINT on a 100k-row OrioleDB table after
  steady-state writes.
- Measured: WAL bytes written during the CHECKPOINT.
- **Budget**: checkpoint WAL ≤ 1.5 × (dirty-page count × 8 KB).
- Rationale: Plan E emits one FPI per dirty page. Anything more
  (like `skip_unmodified_trees=false` forcing full-tree emission)
  is a bug unless explicitly justified. 1.5× accounts for sys-tree
  re-emission.

### N10.6 — xidmap hot-loop contention
- Workload: 64 clients each doing 10k single-row txns (`INSERT;
  COMMIT;`) for 1 min.
- Measured: `wait_event_type = 'LWLock' AND wait_event LIKE 'xidBuffer%'`
  sample count via `pg_stat_activity`.
- **Budget**: xidBuffer wait samples ≤ 5% of total samples.
- Rationale: M1.3 commit-barrier touches xidmap on every commit.
  Without circular-buffer fast-path, this becomes a bottleneck.

## Measurement harness

`scripts/bench_commit_path.sh` (to be written) runs each workload
twice — once against a baseline PG-heap endpoint, once against a
matched OrioleDB endpoint on the same tenant — and prints a table:

| Gate | PG heap | OrioleDB | Budget | Status |
|------|---------|----------|--------|--------|
| N10.1 | 512 B  | 640 B    | ≤ 666 B | PASS |
| N10.2 p50 | 820 µs | 900 µs | ≤ 943 µs | PASS |
| ...

CI runs the harness on every PR against `main`. A regression past budget
blocks the merge until the budget line is signed off in the PR
description.

## Dependencies

- N10.1–N10.6 depend on N2 (data-page commit-barrier) landing — the
  budgets assume N2's FPI emission is in effect. If N2 is incomplete,
  N10 measures a moving target.
- N10.3 depends on N5 (recovery minimal) — until then, compute-side
  replay dominates the restart time and the 2× budget is unreachable.
- N10.5 depends on N4 (checkpoint thinning, in particular retiring
  the `skip_unmodified_trees=false` force path) — currently we pay
  O(total-data) per restart checkpoint.

## What this does NOT cover

- **Cold cache read latency.** Neon fetches pages on miss; OrioleDB's
  IOT delivers cache-hit rows faster than PG heap, but cold-miss
  latency is dominated by network RTT to PageServer (identical in
  both systems). Not budget-worthy.
- **Bulk COPY.** Different emission pattern (SPLIT-heavy); deserves its
  own test but not under N10.
- **Logical replication throughput.** Plugin-level, deferred to Phase 9
  (R14).

## Escalation

When a benchmark regresses past budget:
1. Surface in the PR description with the before/after table.
2. Reviewer flags `n10-budget-regression`.
3. Either redesign the change, or land a separate PR that relaxes the
   budget with written justification (and link the justification here
   under an `## Exceptions` section).
4. Never silently accept a regression. Every budget change is in git.
