# OrioleDB-on-Neon: Enterprise Hardening Plan

> **Version:** v1.0 (2026-04-19)
> **Status:** Authoritative — supersedes per-phase notes for long-term direction.
> **North Star:** `docs/LOG_IS_DATA_ARCHITECTURE.md` (commit-barrier model, unchanged).
> **Milestone ledger:** `docs/ENTERPRISE_ROADMAP.md` (reality of what shipped).
> **Session scratch:** moved to `docs/archive/` once folded in here.

This document is the single long-term view. It enumerates every residual
risk between today's tree and "enterprise-grade OrioleDB+Neon = full
replacement for PG+Neon", sequences the fixes, and sets the gates.

---

## 1. Working invariants (never violated by any phase)

1. **Commit-durable state is in SafeKeeper before commit returns.**
   Applies to every bit of state whose loss would make a committed txn
   invisible or corrupt: data pages, undo entries, oxid→CSN, root
   downlinks after a split, sys-tree catalog writes.
2. **Non-commit-durable state is lazy-flushed via page I/O.**
   Checkpoint stats, freelist counters, map header fields unrelated to
   root downlink — none of these justify extra WAL.
3. **No compute-local file is an authority.** Local FS is a cache;
   wiping pgdata must reconstruct identical observable state from
   basebackup + SafeKeeper WAL alone.
4. **Overhead vs PG+Neon is bounded.** Each new mechanism comes with a
   measured write-amp / latency delta against PG heap. Uncontrolled
   drift is a design bug, not a tradeoff.
5. **OrioleDB's differentiators are preserved.** IOT, COW checkpoints,
   undo-based MVCC, CSN visibility — none of them are compromised to
   make serverless cheap. Serverless adapts; storage-engine design
   stays.

---

## 2. Architecture layers (target shape)

```
┌─────────────────────────────────────────────────────────────┐
│ L1  Commit Barrier — every commit's critical state is in    │
│     SafeKeeper before commit returns                         │
│                                                              │
│   L1.a  xidmap (oxid→CSN)          — M1.3  ✓ landed         │
│   L1.b  undo range written by txn   — M1.2  ✓ landed         │
│   L1.c  data pages dirtied by txn   — M1.4  OPEN (was gap)  │
│   L1.d  rootDownlink after split    — M1.5  OPEN            │
│   L1.e  sys-tree writes touched     — M1.6  OPEN            │
└─────────────────────────────────────────────────────────────┘
                          ↓
┌─────────────────────────────────────────────────────────────┐
│ L2  COW Checkpoint — relocation + compaction, not full FPI  │
│                                                              │
│   L2.a  Emit FPI only for (a) COW relocation,                │
│          (b) compaction of a delta chain, (c) structural     │
│          ops (SPLIT/MERGE/ROOT_SPLIT — already done)         │
│   L2.b  Atomic completion (control file committed last)     │
│   L2.c  Idempotent re-start after mid-checkpoint crash       │
└─────────────────────────────────────────────────────────────┘
                          ↓
┌─────────────────────────────────────────────────────────────┐
│ L3  Recovery Minimal                                         │
│                                                              │
│   L3.a  Basebackup + PageServer GetPage = authoritative     │
│          state; no compute-side replay needed for data      │
│   L3.b  Shmem reconstruction only: xidmap cache,            │
│          in-flight txn slots, tree descr cache              │
│   L3.c  Signal mechanism (orioledb_recovery.signal)         │
│          retired once L1.c is proven                         │
└─────────────────────────────────────────────────────────────┘
```

**Current completion:** L1 ≈ 50% (a,b done; c,d,e open). L2 stub.
L3 currently dependent on fragile signal path — the source of the
Phase 6.6.4c-3 failure.

---

## 3. Risk register (indexed, with mitigation mapping)

Each risk has a stable ID (**R1…R15**) so commits and tests can cite
it. A risk is **CLOSED** only when a phase exits with its exit criterion
met and a regression test is in CI.

| ID | Class | Summary | Blast radius | Status | Mitigation |
|---|---|---|---|---|---|
| R1 | Durability | Committed rows past last checkpoint are only in compute memory + CONTAINER WAL; PageServer cannot materialize them. Any crash → loss unless compute-side replay fires. | Full tx loss. **Root cause of 6.6.4c-3.** | OPEN | N1 (immediate triage) + M1.4 (data-page commit-barrier) |
| R2 | PITR | Branching/PITR at arbitrary LSN cannot reconstruct OrioleDB pages between checkpoints (PageServer has no wal-redo for rmid=129 deltas). | Enterprise feature gap. | OPEN | M1.4 + Phase 7 PITR validation |
| R3 | Physrepl | Physical replicas read PageServer — stale past last checkpoint until a commit-barrier materialises new pages there. | Replica lag visible beyond checkpoint; read-anomalies. | OPEN | M1.4 unlocks it; Phase 7 validation |
| R4 | Recovery | End-of-recovery checkpoint with `skip_unmodified_trees=false` can emit empty FPIs for user trees whose shmem is empty, clobbering PageServer's good state. | Data loss on restart. | OPEN | N1 (guard) + retire the GUC in Phase 6 cleanup |
| R5 | Atomicity | Mid-checkpoint SIGKILL leaves partial chkp=N FPIs in PageServer; control file still points at chkp=N-1 which is fine, but "half of N" can confuse idempotency. | Subtle recovery corruption. | MITIGATED by existing guards; verification pending | Phase 8.1 dedicated test matrix |
| R6 | Detection | `.orioledb_initialized` / `.orioledb_sync_lsn` marker files in endpoint_dir drive recovery triggering. If marker is missing, no signal is written, no replay. | Total data loss on restart. | OPEN — **this is what is biting 6.6.4c-3** | N1 replaces detection with unconditional signal on Primary |
| R7 | Concurrency | Two backends racing first read — only one can replay-through-signal. | Inconsistent visibility. | Latent | Becomes moot once M1.4 removes replay dependency |
| R8 | Structure | SPLIT/MERGE/ROOT_SPLIT emit FPIs today; LEAF_LOCK / PAGE_INIT / standalone ROOT_SPLIT emit paths exist as dead code. | Unused code rots; no present correctness issue. | LOW | M1.1 cleanup |
| R9 | Compression | Compressed pages use `ORIOLEDB_COMP_BLCKSZ` (not 8 KB). Plan E FPI path unclear for compressed pages. | Compressed tables crash-unsafe on Neon. | UNVERIFIED | Phase 8.2 — add compressed-table variant of the 6.6.4 E2E |
| R10 | 2PC | `current_oxid_commit` is the main commit path; prepared-transaction commit may go through a different path not covered by L1.a/L1.b barriers. | Prepared-txn crash loss. | UNVERIFIED | Phase 8.3 — audit + add commit-barrier at the 2PC site |
| R11 | Subtxn | CSN assigned at top-level commit; subtxn rollback uses undo chains. Recovery of nested state on crash untested. | Savepoint-semantic loss. | UNVERIFIED | Phase 8.4 — SAVEPOINT E2E |
| R12 | DDL | CREATE/ALTER TABLE writes to sys-tree; depends on M1.6 (L1.e). Until that's done, DDL-before-crash can lose catalog state. | DDL not replayable. | OPEN | M1.6 |
| R13 | Freeze | xidmap wraparound pressure. Neon-specific since xidmap is backed by synthetic relation. | Long-running tenant correctness. | UNKNOWN | Phase 8.5 |
| R14 | LogiRepl | `orioledboutput` logical decoding plugin interop with Neon walservice. | Subscription break. | DEFERRED | Phase 9 |
| R15 | Ext-compat | pg_stat_statements, pg_cron, pgvector etc. against OrioleDB on Neon. | Ecosystem gap. | DEFERRED | Phase 9 |
| R16 | WAL retention | PITR target LSN must be within SafeKeeper / PageServer retention. Enterprise PITR UI must surface the lower bound so users cannot request lost LSNs. | Silent PITR miss. | OPEN | N7.2 surface retention in PITR API |
| R17 | Commit-barrier contention | Multiple backends committing on the same leaf page emit N FPIs. WAL amplification 10-100×. | Throughput cliff on hot-key workloads. | FORESEEN | N2.6 commit-group amortisation |
| R18 | Ordering at commit | Barrier items (undo, CSN, data pages) must all be in WAL **before** `XACT_COMMIT` is flushed. Crashes between barrier-emit and XLogFlush are safe (rollback), crashes between different barrier items are safe only because all are in the same WAL stream flushed atomically by `XLogFlush(commit_lsn)`. | Split-brain if XACT_COMMIT is flushed while a barrier item is still buffered. | ANALYSED — relies on PG's `XLogFlush(commit_lsn)` atomicity. Documented. | Design invariant baked into N2 |

---

## 4. Phase sequencing

Phases are additive: Phase N's exit criteria are sufficient to start
Phase N+1, not necessary. When two phases are independent, both are
cleared to run in parallel.

### N1 — Unblock 6.6.4c-3 (IMMEDIATE)
*Goal:* stop the current CI regression, no architectural change.

- **N1.1** Make `orioledb_recovery.signal` writing **unconditional on
  Primary start** with a known sync_lsn. Remove `was_initialized`
  gating. If sync_lsn is missing it is a first-start → no-op.
- **N1.2** Add a startup-time assertion: if `skip_unmodified_trees=
  false` is in effect AND `IsOrioleDbRecoveryRequested()=false`, the
  compute must refuse to continue (today this is the exact combo
  that clobbered FPIs; fail fast is safer than quiet corruption).
- **N1.3** Keep diagnostics that were added in commit `00771f9` until
  the whole signal story is retired by N2.

**Exit:** `Run crash-mid-checkpoint recovery` green in CI on two
consecutive pushes.

### N2 — L1.c: data-page commit barrier (M1.4)
*Goal:* close R1/R2/R3/R4/R6 at the root. Data pages dirtied by a txn
reach SafeKeeper before the txn's commit returns, identical in spirit
to the undo/xidmap barriers that landed in M1.2/M1.3.

- **N2.1** Per-backend dirty-page set: each B-tree page mutation
  already goes through `page_state` — piggyback a per-xact list.
  Scope: pages whose mutation belongs to the current oxid.
- **N2.2** In `current_oxid_commit` (after xidmap flush, before
  `pg_write_barrier`), iterate that set and emit `REGBUF_FORCE_IMAGE`
  via the existing btree WAL FPI path. The page-level WAL records
  already exist — we're changing the emission trigger.
- **N2.3** After emit, the set is cleared. Aborted txns drop the
  set without emitting (undo unwinds the in-memory page state).
- **N2.4** Remove the `skip_unmodified_trees=false` GUC injection
  from `compute_tools/src/compute.rs`; with N2.2 the EOR checkpoint
  workaround is obsolete.
- **N2.5** Remove `orioledb_recovery.signal` signalling. PG starts
  from basebackup, reads pages from PageServer, done. Signal file
  support stays in PG source as a tool for operators to force replay,
  but compute_ctl no longer writes it.
- **N2.6** Commit-group amortisation for R17. If backend A emitted
  an FPI for page P at LSN L, and backend B's commit touches P with
  mutations recorded before L, B's barrier is a no-op for P —
  A's FPI already covers it (CSN-based visibility takes care of
  "which txns' rows are committed inside the FPI"). Track per-page
  `lastCommitBarrierLsn`; on commit, if a backend's dirty-marker LSN
  for that page ≤ `lastCommitBarrierLsn`, skip the emit. If it's
  newer, emit and update the marker. This is the standard
  amortisation trick used by PG's `XLOG_FPI` for full-page writes.
- **N2.7** Ordering invariant (R18): all barrier emits produced
  inside `current_oxid_commit` reach WAL via `XLogInsert`;
  `RecordTransactionCommit` → `XLogFlush(commit_lsn)` flushes them
  together with `XACT_COMMIT`. Do **not** call `XLogFlush` inside
  the barrier — it would serialise commits and defeat N2.6.

**Exit:** `Run crash-mid-checkpoint recovery` green **without**
`skip_unmodified_trees=false` in postgresql.conf AND without
`orioledb_recovery.signal`.

### N3 — L1.d + L1.e: map header and sys-tree commit barrier
- **N3.1** Root-split completion triggers map-file header FPI before
  the commit proceeds past the oxid barrier.
- **N3.2** Sys-tree writes (o_tables, shared_root_info, evicted_data,
  etc.) are audited; any that commit-visible lives through CONTAINER-
  only path gets an FPI at commit.

**Exit:** DDL crash-and-replay E2E covering `CREATE TABLE`,
`CREATE INDEX`, `ALTER TABLE ADD COLUMN` — all byte-identical across
wipe-pgdata restart.

### N4 — L2 checkpoint thinning
- **N4.1** Delete the `checkpoint_is_shutdown` carve-outs in
  `o_buffers.c`, `checkpoint.c`, `control.c`. With N2 + N3 the
  carve-outs are dead (commit-barrier already covered the shutdown
  boundary).
- **N4.2** Remove the full-tree FPI loop (`check_tree_needs_checkpointing`
  back to the default of skipping unmodified trees).
- **N4.3** Evaluate `xidFile` removal (dup of xidmap post-M1.3).

**Exit:** Checkpoint WAL volume in steady-state ≤ 1 KB/checkpoint-not-
counting-relocation. Measured under `test_e2e_crud.sh` workload.

### N5 — L3 recovery minimal
- **N5.1** Narrow `orioledb_redo` to shmem-reconstruction only — skip
  CONTAINER records that describe data already persisted via L1.c.
- **N5.2** Evaluate whether `orioledb_recovery.signal` remains as an
  operator tool. If nothing inside compute_ctl writes it, and PG's
  normal crash-recovery path suffices, retire the signal-specific
  code path in xlogrecovery.c.
- **N5.3** Remove the synthetic-relation `ExistsFile` gating for
  startup basebackup (audit at Phase 6.6.0).

**Exit:** `orioledb_redo` processes only XID/RELATION/JOINT_COMMIT/
SAVEPOINT/SWITCH_LOGICAL_XID records under rmid=129. Data and page
records are skipped. Reconstruction tests pass.

### N6 — Structural WAL cleanup (M1.1)
Low priority; unblocked anytime.

- Emit the dead types (LEAF_LOCK, PAGE_INIT, standalone ROOT_SPLIT)
  or delete them from the header.

### N7 — Enterprise feature validation
All gated on N1–N5.

- **N7.1** Branch at arbitrary LSN. Target: create a branch **mid-
  transaction-batch**, expect to see the dataset as of that LSN.
- **N7.2** PITR to arbitrary LSN. Target: `pg_restore` style restore
  to any LSN within SafeKeeper retention.
- **N7.3** Physical replica convergence. Target: OrioleDB primary
  writes → read-only replica reads the same rows within X seconds
  (X ≤ Neon's PG heap baseline).
- **N7.4** OrioleDB's in-tree `make installcheck` SQL tests run
  under Neon stateless mode, green.

### N8 — Crash-window and correctness matrix
Each of these has a dedicated script under `scripts/` and runs in CI.

- **N8.1** Mid-commit SIGKILL, commit before and after the barrier.
- **N8.2** Compressed tables under 6.6.4 E2E (R9).
- **N8.3** 2PC prepared txn + SIGKILL (R10).
- **N8.4** SAVEPOINT + ROLLBACK TO + SIGKILL (R11).
- **N8.5** xidmap wraparound simulation (R13).
- **N8.6** Mid-checkpoint SIGKILL at several chkp-fraction points
  (R5 regression grid).
- **N8.7** Concurrent commits from N backends, SIGKILL mid-burst.

### N9 — Ecosystem + upstream
- **N9.1** `orioledboutput` logical decoding plugin with Neon
  walservice (R14).
- **N9.2** Compat with pg_stat_statements / pg_cron / pgvector (R15).
- **N9.3** Submit Layer 1 / Layer 2 hooks upstream to OrioleDB where
  they are generally applicable (not Neon-specific).

### N10 — Benchmarks and release hardening
- **N10.1** Write-path WAL bytes/sec vs PG heap on Neon.
- **N10.2** Commit latency p50/p99, targeted ≤ PG heap + 15%.
- **N10.3** Restart time for 10 GB state, targeted ≤ 2× PG heap.
- **N10.4** Steady-state throughput, targeted ≥ 0.9× PG heap.

---

## 5. Release mapping

| Tag | Scope | Exit gate |
|---|---|---|
| `v0.1.0-alpha.6` | N1 | 6.6.4c-3 green on two consecutive runs |
| `v0.1.0-alpha.7` | N1 + N2 | `skip_unmodified_trees=false` injection removed; 6.6.4c green |
| `v0.2.0-beta.1` | N3 + N4 | DDL crash-replay green; checkpoint WAL gate met |
| `v0.2.0-beta.2` | N5 + N6 | orioledb_redo narrowed; dead WAL types resolved |
| `v0.3.0-beta.1` | N7 | Branch / PITR / physrepl validated |
| `v0.3.0-beta.2` | N8 | Crash-window matrix green |
| `v1.0.0-rc.1` | N9 + N10 (partial) | Logical decoding; ≥ 80% bench gates met |
| `v1.0.0` | N9 + N10 (full) | All benches within budget; docs honest |

---

## 6. Explicitly rejected directions

| Proposal | Why it is rejected |
|---|---|
| PageServer wal-redo for rmid=129 | Couples PageServer to OrioleDB semantics; increases attack surface; adds seccomp complexity; M1.4 solves the same problem at commit-time with zero PageServer churn. |
| Every mutation emits block-keyed WAL | Over-shoots the invariant; balloons WAL volume for workloads that already pay checkpoint FPI cost. |
| Keep `skip_unmodified_trees=false` permanently | O(total_data) WAL per restart is fatal for serverless cold-start costs. |
| Compute-side replay as the long-term data path | Single-copy, fragile on marker detection, racy on concurrent reads, impossible for physical replicas. |
| Shim OrioleDB rows onto PG heap | Loses IOT, undo, CSN. Destroys the entire OrioleDB value proposition. |
| Dual durability paths (commit-barrier **and** lazy) | Two bug surfaces; two test matrices; and when they diverge (they will) the divergence goes undetected for months. |

---

## 7. Doc hygiene

- `docs/LOG_IS_DATA_ARCHITECTURE.md` — North Star, v2.1. Unchanged by
  this plan. It describes the *model*; this file schedules the *work*.
- `docs/ENTERPRISE_ROADMAP.md` — records *what actually shipped per
  phase*, with CI run IDs. Continue updating per-phase.
- `docs/SESSION_NOTES_M1_PROGRESS.md` — fold into this doc, then
  archive under `docs/archive/` when M1 closes.
- `docs/archive/PAGE_LEVEL_WAL_PLAN_v1_2026-04-16.md` — already
  archived; keep as the historical record of the rejected strategy.
- Per-release tag notes continue as small commits; this plan is the
  contract those notes reference.

---

## 8. Decision log — why this plan, not the old one

The previous architecture plan (archived as v1) targeted per-mutation
block-keyed eager WAL as the complete answer. The v2.1 North Star
revised that to commit-barrier, which is a better invariant (matches
PG heap's "WAL at commit, lazy buffer flush" design). This doc's
contribution is the **third step**: making the commit-barrier cover
the one piece it doesn't yet (data pages — L1.c / M1.4), then
systematically retiring the replay-path workarounds that were there
only to paper over the L1.c gap.

Once L1.c is in place, the recovery model collapses to "basebackup +
PageServer reads = authoritative state" with no compute-side replay
dependency. That is the enterprise shape.

---

## 9. Open questions (tracked, not blockers)

- Should L1.c emission batch across committing backends to amortise
  the WAL flush? If yes, at what group size?
- Can OrioleDB's existing `evicted_data` sys-tree be repurposed as
  the per-backend dirty-page set, avoiding a new shmem structure?
- For compressed pages, does L1.c emit the compressed byte-range or
  the decompressed 8 KB image? (Decompressed is simpler — investigate
  WAL volume impact.)
- Is `CheckPoint_hook` the right insertion point for post-N5 EOR
  work, or do we want a dedicated `AfterRecoveryDoneHook`?

Decisions land in this section, dated, before the corresponding phase
starts.

---

## 10. Review log

- **2026-04-19 r1**: First draft. Folded in R1-R15, N1-N10.
- **2026-04-19 r2**: Added R16 (WAL retention), R17 (commit-barrier
  contention) and R18 (commit ordering invariant). Expanded N2 with
  N2.6 commit-group amortisation and N2.7 ordering-invariant clause
  so the design is self-sufficient without re-deriving it at
  implementation time.
- **2026-04-19 r3** (*pending*): Update after the first N2 spike lands
  — measured WAL amp under `test_e2e_crud.sh`. If amp > 2× PG heap,
  revisit N2.6 sizing.
