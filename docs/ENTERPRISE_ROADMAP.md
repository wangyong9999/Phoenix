# OrioleDB on Neon: Enterprise Readiness Roadmap

> **Status:** POC validated — stateless restart works (100 rows recovered)
> **Created:** 2026-04-16
> **Baseline:** INSERT → CHECKPOINT → STOP → RESTART → SELECT = 100 rows ✅

## Phase 6: Delta WAL Recovery (CRITICAL)

**Goal:** Verify data recovery for non-checkpointed modifications

**Test:** INSERT → CHECKPOINT → INSERT more → STOP (no 2nd CHECKPOINT) → RESTART → SELECT

This exercises the complete page-level delta WAL → wal-redo pipeline:
  PageServer finds: base image (checkpoint FPI) + delta (LEAF_INSERT WAL)
  → sends both to wal-redo process
  → orioledb_page_redo applies delta to base image
  → returns correct page with both checkpoint and post-checkpoint data

**If fails:** Fall back to FPI-only mode (change delta to FPI for all ops)

## Phase 6.5: Sys-tree descriptor lifecycle on stateless restart

> **Status:** Candidate fix in flight — commit `1684e2e` hoists the
> deferred control-file load out of `checkpointable_tree_init` into
> `sys_tree_init_if_needed`. Verification pending CI run
> `24560180590`. If E2E reaches `[9/9] PASS`, close this phase and
> cut v0.1.0-alpha.2 per the Release Gate.

**Symptom** observed in Phoenix CI run `24558882594` (step 7,
fresh backend after stateless restart):

```
TRAP: failed Assert("OInMemoryBlknoIsValid((td)->rootInfo.metaPageBlkno)"),
      File: "src/checkpoint/checkpoint.c", Line: 5289
autovacuum launcher → GetSnapshotData → orioledb snapshot hook
                    → checkpointable_tree_init (1, 2) chkp_num=3
                    → checkpointable_tree_fill_seq_buffers
                    → BTREE_GET_META(td)   [metaPageBlkno invalid]
```

**State at crash:**

- Startup process successfully loaded control file from PageServer,
  reset the 24 sys-tree `initialized` flags via
  `sys_trees_reset_initialized()`, ran end-of-recovery checkpoint 3,
  emitted map-file + data-page + control-file FPIs, updated
  `sync_lsn`.
- First fresh backend after PG reached "accepting connections"
  (autovacuum launcher) hit the OrioleDB snapshot hook and tripped
  the assert initializing sys tree `(1, 2)` with `chkp_num=3`.

**Actual root cause (identified 2026-04-17):** Not the hypothesis
above — the fault is specifically in the *timing* of
`sys_trees_reset_initialized()`. The old code called it from inside
`checkpointable_tree_init`, which in turn is called from the middle
of `sys_tree_init(i, init_shmem=true)`. By the time the reset runs,
`sys_tree_init` has already written the freshly allocated
`metaPageBlkno` into `sysTreesShmemHeaders[i].rootInfo`. The reset
nukes it, the caller returns unaware, and
`sys_tree_init_if_needed` then sets
`header[i].initialized = true`. Header `i` is now permanently in a
`{initialized=true, metaPageBlkno=invalid}` state. The next fresh
backend (a process where `sysTreesDescrs[i].initialized=false`
locally) reads `header[i]`, sees `initialized=true`, takes the
`sys_tree_init(i, init_shmem=false)` branch, copies the invalid
`metaPageBlkno` into its local descriptor, and the next
`BTREE_GET_META(td)` dereference trips the assert.

Trees `i=0, i=2..23` escape the trap: `i=0` is `BTreeStorageInMemory`
(skips the path entirely), and `i=2..23` see `lastCheckpointNumber
!= 0` so the deferred-load branch no longer fires for them. Only
`i=1` — sys tree `(1, 2)` — is left poisoned, matching the crash
address.

**Not a workaround candidate.** Disabling autovacuum or
max_worker_processes just delays the crash until the first real
user backend. The architectural fix must restore sys-tree
shared-memory state cleanly after `sys_trees_reset_initialized()`,
or skip the reset when `lastCheckpointNumber` is already non-zero
and the control file contents are coherent.

**Fix applied (`1684e2e`):** Move the deferred control file load
into a new `sys_trees_load_control_if_deferred()` helper and call
it at the very top of `sys_tree_init_if_needed`, before the per-tree
alloc loop starts handing out `metaPageBlkno` pages. The reset now
runs while all headers are still in their initial-shmem invalid
state, so the alloc loop hands out fresh pages cleanly and no
header ends up half-initialized. The old call site inside
`checkpointable_tree_init` is removed; a comment there records why
and points readers to the new helper.

## Phase 7: Full CRUD + Structural Operations

**Tests:**
- UPDATE 50 rows → CHECKPOINT → STOP → RESTART → verify updates present
- DELETE 50 rows → CHECKPOINT → STOP → RESTART → verify deletions
- INSERT 5000 rows (trigger multiple SPLITs) → CHECKPOINT → RESTART → verify all
- Mixed operations → RESTART → verify

## Phase 8: Branching

**Test:** INSERT data → create timeline branch → SELECT on branch
- Verifies OrioleDB page-level WAL supports Neon's branching
- Tests ancestor timeline page resolution for OrioleDB synthetic relations

## Phase 9: Stress & Reliability

**Tests:**
- pgbench with OrioleDB tables (1000 TPS for 60s → restart → verify)
- WAL volume measurement (delta vs FPI overhead)
- Concurrent INSERT from multiple backends → crash → restart

## Phase 10: CSN Improvement

- Verify xidBuffer checkpoint flush covers all committed CSN
- Test transaction visibility after restart (CSN-based MVCC)
- Evaluate: commit-time flush vs checkpoint-only flush

## Priority Order

1. **Phase 6.5 (sys-tree descriptor lifecycle)** — blocks `serverless-e2e` CI green; must be real fix, no workaround
2. Phase 6 (delta WAL) — covers post-checkpoint recovery
3. Phase 7 (CRUD) — confidence in basic operations
4. Phase 8 (branching) — Neon's value proposition
5. Phase 9 (stress) — production readiness
6. Phase 10 (CSN) — completeness

## Release Gate

Release tags (`vX.Y.Z-alpha.N`) are only cut when:

1. Phoenix CI run for the tag commit is **fully green**, including
   the `serverless-e2e (Plan E round-trip)` job. `continue-on-error`
   on that job is a temporary scaffold for infrastructure bring-up,
   **not** a release gate — the job must actually pass.
2. `scripts/test_e2e.sh` reaches `[9/9] PASS — OrioleDB serverless
   round-trip verified` with matching row count + md5 checksum
   across the stop/start cycle.
3. Release notes honestly enumerate what works (`Verified`) vs what
   is deferred (`Known limitations`). Reference roadmap phase
   numbers rather than hand-waving.
4. No open `P0` items in the task tracker at the release commit.

`v0.1.0-alpha.1` was cut before this gate existed and shipped with
a failing `serverless-e2e` job masked by `continue-on-error`. It
is preserved as a historical baseline; future alphas follow this
gate.
