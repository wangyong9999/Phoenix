# Test Matrix Alignment — OrioleDB on Neon vs PG Heap on Neon

**Phase:** 7 (test-matrix alignment)
**Status:** Planning + harness skeleton in place; full coverage expansion is
multi-week and staged through v0.3.0-beta.1 → v1.0.0-rc.1.

## Thesis

First-commercial readiness = OrioleDB-on-Neon passes the **union** of two
existing test suites, converted to exercise OrioleDB tables and the
OrioleDB-specific WAL path:

1. `pgxn/orioledb/test/sql/` — 43 in-tree OrioleDB regression tests.
   These today assume local-FS persistence. Under Neon that assumption
   is false, so every such test must additionally survive a stateless
   restart mid-test.
2. `test_runner/regress/` — Neon's own regression suite, written against
   PG heap. The restart/branching/PITR/crash subset (see below) is
   what validates the stateless guarantee; each of those tests needs an
   OrioleDB-backed variant that asserts the same invariants against
   `USING orioledb` tables.

Running either suite alone is insufficient: OrioleDB's suite covers
storage-engine correctness but not stateless restart; Neon's suite
covers stateless restart but not OrioleDB's IOT/undo semantics.

## 7.1 — OrioleDB SQL regressions under Neon mode

### Harness

`scripts/run_orioledb_tests_neon.sh`. For each selected `.sql` file:

1. Spin up a fresh `.neon` tenant + endpoint.
2. `CREATE EXTENSION orioledb`.
3. Run the full `.sql` through psql; capture all output.
4. `CHECKPOINT`, stop endpoint, start endpoint (stateless restart).
5. Re-run the SELECT-only subset of the file; capture output.
6. Diff — divergence = test fails.

### Known limitations of the skeleton

The current skeleton does a rowcount-of-output heuristic, not a
byte-for-byte diff. Phase 7.1 v2 must refine to:

- Before restart: execute the full test; also execute the select-only
  subset and dump to `before_selects.out`.
- After restart: execute the select-only subset and dump to
  `after_selects.out`.
- Byte-diff the two; any divergence fails.

Some tests are intrinsically per-process (query runtime stats,
`pg_stat_*` views). These go into a SKIP_LIST with a one-line comment
explaining why. Populate the SKIP_LIST organically as failures surface.

### Test inventory

| Test file | Covers | Under Neon: additional risk surface |
|---|---|---|
| `alter_storage.sql`, `alter_type.sql` | DDL that rewrites table storage | Plan E FPI must cover the rewritten B-tree shape post-restart |
| `bitmap_scan.sql`, `btree_print.sql`, `btree_compression.sql` | B-tree internals | rebuild from FPI + CONTAINER WAL replay |
| `createas.sql`, `ddl.sql`, `database.sql` | Basic DDL + multi-DB | exercises 6.6.1 reserved OID window across DBs |
| `exclude.sql`, `foreign_keys.sql`, `generated.sql`, `identity.sql` | Constraints + defaults | visibility preserved across restart |
| `explain.sql` | EXPLAIN output stability | likely SKIP_LIST (plan hash may differ) |
| `fillfactor.sql` | storage tuning | FPI must reflect post-restart |
| `savepoint.sql`, `transaction.sql`, `isolation*.sql` | Txn semantics | CSN / undo preserved across restart (Plan B) |
| `toast.sql`, `compression.sql` | Large values | TOAST detoasting after stateless restart |
| `index*.sql`, `include.sql`, `collate.sql`, `lookup.sql` | Index access paths | post-restart index correctness |
| `merge.sql`, `partition_*.sql` (none listed but if added) | Merge / (no partition support yet) | N/A for now |
| `orioledb_json_upgrade.sql`, `upgrade.sql` | Extension upgrade | likely SKIP_LIST (out of scope for restart diff) |

Exact list: 43 files. Audit them individually during 7.1 v2 and
mark each as `(restart-sensitive | plan-stable | skip)`.

## 7.2 — Neon test_runner OrioleDB variants

### Target subset

The PG-heap tests in `test_runner/regress/` that are most directly
testing Neon's stateless-restart / branching / PITR / crash machinery
(grep for "restart", "branch", "pitr", "checkpoint", "crash"):

| PG-heap test | OrioleDB variant needed | Invariant it pins |
|---|---|---|
| `test_pageserver_restart.py` | `test_pageserver_restart_orioledb.py` | compute reconnects after PS bounce |
| `test_pageserver_restarts_under_workload.py` | OrioleDB variant | pgbench-like TPS through PS restarts |
| `test_endpoint_crash.py` | OrioleDB variant | data preserved across compute SIGKILL (direct dup of `test_e2e_crash_mid_ckpt.sh`, but in pytest form) |
| `test_branching.py` | OrioleDB variant | equivalent of `test_e2e_branching.sh` but under pytest |
| `test_ancestor_branch.py` | OrioleDB variant | ancestor-timeline page resolution for OrioleDB synthetic relations |
| `test_branch_and_gc.py` | OrioleDB variant | GC of historical LSNs below branch points |
| `test_pitr_gc.py` | OrioleDB variant | PITR + GC interaction |
| `test_pageserver_crash_consistency.py` | OrioleDB variant | PS crash during compute writes |
| `test_subscriber_branching.py`, `test_subscriber_restart.py` | OrioleDB variant | logical subscribers + branching — BLOCKED on 6.8.2 |

### Implementation pattern

For each target, copy the PG-heap test, replace every CREATE TABLE
(and `test_decoding` uses) with `USING orioledb`, keep the rest of the
assertion logic verbatim. If the test uses pg-specific introspection
(`pg_class.reltype`, `pg_statio_*`), flag those queries in a comment;
some become OrioleDB-aware (`orioledb_tables`) but many are legitimate
to keep since OrioleDB mirrors pg_class entries.

Runs under the same pytest harness:

```
poetry run pytest test_runner/regress/test_pageserver_restart_orioledb.py
```

Scaffold the first three variants as the v0.3.0-beta.1 gate. Rest trail
into v1.0.0-rc.1.

## 7.3 — CRUD + concurrency + crash E2E

Small, focused bash tests that complement the pytest suite:

| Script | Status | Covers |
|---|---|---|
| `scripts/test_e2e.sh` | ✅ shipped alpha.2 | INSERT round-trip |
| `scripts/test_e2e_crash_mid_ckpt.sh` | ✅ shipped Phase 6.6.4 | SIGKILL mid-CHECKPOINT |
| `scripts/test_e2e_branching.sh` | ✅ skeleton Phase 6.7.1 | timeline branching |
| `scripts/test_e2e_pitr.sh` | ✅ skeleton Phase 6.7.2 | PITR at LSN |
| `scripts/test_e2e_physrepl.sh` | ✅ skeleton Phase 6.8.1 | primary + replica convergence |
| `scripts/test_e2e_crud.sh` | ⏳ Phase 7.3 | UPDATE 50 / DELETE 50 / INSERT 5000 with SPLIT |
| `scripts/test_e2e_concurrent.sh` | ⏳ Phase 7.3 | 2 backends concurrent INSERT → restart |

Each of `test_e2e_crud.sh` and `test_e2e_concurrent.sh` follows the
same shape as `test_e2e.sh`: init → populate → CHECKPOINT → stop/start
→ md5 verify. The only difference is the workload between the
CHECKPOINT and stop.

## Milestones

- **v0.2.0-beta.1** — Phase 6.6.4 crash E2E + 7.3 CRUD/concurrent E2E.
- **v0.3.0-beta.1** — Phase 7.2 first three variants
  (`test_pageserver_restart_orioledb.py`, `test_endpoint_crash_orioledb.py`,
  `test_branching_orioledb.py`) passing.
- **v1.0.0-rc.1** — Phase 7.1 at 80% parity (most SKIP_LIST entries
  justified), Phase 7.2 full subset green except 6.8.2-blocked
  subscriber tests.

## Open questions

1. **SKIP_LIST governance.** When a test is skipped, who reviews? Suggest
   that every SKIP_LIST addition requires a one-paragraph justification
   in the source comment and a follow-up GitHub issue (no silent
   skips).
2. **Harness byte-diff refinement.** The current
   `run_orioledb_tests_neon.sh` uses a rowcount heuristic; v2 must
   byte-diff. Split out into v2.1 as a small follow-up.
3. **Pytest harness reuse.** The existing pytest `test_runner/`
   fixtures depend on a running Neon control plane. Confirm that a
   pytest fixture can switch between PG-heap and OrioleDB table AM
   via parametrize, so we don't need to duplicate every fixture.
