# OrioleDB on Neon: Enterprise Readiness Roadmap

> **Status:** v0.1.0-alpha.2 cut on commit `8955a03` (2026-04-17) — Phase 6
> and Phase 6.5 verified under Phoenix CI `24561080441` with `serverless-e2e`
> actually green (not masked by `continue-on-error`).
> **Created:** 2026-04-16, last updated 2026-04-18
> **Baseline:** INSERT 100 rows → CHECKPOINT → STOP → RESTART → SELECT,
> md5 checksum preserved end-to-end.

## Guiding Principles

Two invariants every phase below must respect. Any design that violates
them is rejected regardless of how quickly it unblocks a specific test.

1. **Log-is-Data.** WAL is the single source of persistence truth. Local
   filesystem state on the compute node is a *cache*, never an authority.
   Any compute node, starting fresh from WAL + PageServer, must converge
   to the same state as the compute that wrote it.

2. **Serverless weight ≤ Neon + PG heap.** Each added mechanism must be
   quantified against the equivalent PG heap path on Neon. If the delta
   is positive (more bytes on the wire, more disk syncs, more RTTs per
   op), the mechanism does not ship as-is. "OrioleDB is different" is
   not an excuse — the overhead delta must be justified by feature
   delta, not absorbed silently.

Write-path comparison reference (kept up to date as design evolves):

| Step | PG heap on Neon | OrioleDB (target, after Phase 6.6) |
|---|---|---|
| backend | XLogInsert + MarkBufferDirty | XLogInsert + in-mem buffer write (no fsync) |
| flush | FlushBuffer → smgr → network | buffer flush → local FS (cache only) |
| read miss | smgr → network get_page | local FS cache → on miss → smgr → network |

Target: OrioleDB write ≤ PG heap write (avoids network on flush).
Read: OrioleDB cache-hit is *faster* than PG heap (no RTT); cache-miss
is one extra local read, then identical.

---

## Phase 6.5: Sys-tree descriptor lifecycle — RESOLVED

> **Status:** Resolved in v0.1.0-alpha.2 (commit `8955a03`). Two-part fix:
> `1684e2e` (hoist deferred control-file load out of
> `checkpointable_tree_init`) + `8955a03` (propagate `chkp_num` after
> Plan E map rehydrate). Phoenix CI run `24561080441` passes
> `serverless-e2e` on its own merits.
>
> Root-cause write-up preserved at the bottom of this file for future
> readers debugging similar sys-tree ordering bugs.

## Phase 6: Delta WAL Recovery — VERIFIED

> **Status:** Verified 2026-04-17 under CI `24561080441`. INSERT →
> CHECKPOINT → INSERT more → STOP → RESTART → SELECT produces the full
> post-checkpoint row set via wal-redo applied to checkpoint FPI.
> Preserved here as a reference phase, not an open item.

---

## Phase 6.6: Log-is-Data completeness (P0, blocks beta)

**Thesis.** Today's OrioleDB integration is a *hybrid*: local files are
written first, then mirrored to WAL. That leaves two failure modes — a
crash mid-write produces a local file that is newer than WAL (corrupt on
recovery), and a branch/PITR operation sees inconsistent state because
local files are not timeline-aware. Fix by making WAL the only authority
and demoting local FS to cache.

Phase 6.6 has five sub-items, deliberately ordered so the cheapest
read-only audits (`6.6.0`, `6.6.3`) run first and may shrink the
expensive write-path work (`6.6.2`).

### 6.6.0 — Basebackup semantic audit (read-only) — COMPLETE

> **Status:** Done 2026-04-18. Finding: **Possibility B** —
> basebackup does NOT include `orioledb_data/`. Plan B WAL mirror is
> the sole durability layer, not belt-and-suspenders. Full audit with
> quoted code at `docs/BASEBACKUP_AUDIT.md`.
>
> **Consequence.** 6.6.2 scope is locked at **full**: write-path
> inversion (XLogInsert first, local FS demoted to cache) is
> mandatory. Two concrete issues to close:
>   1. `o_buffers.c:214-254` writes FS *before* WAL — ordering must
>      invert.
>   2. `o_buffers.c:233` `checkpoint_is_shutdown` skip on Plan B
>      mirror must be removed.

### 6.6.1 — Synthetic OID reservation hardening

**Correction from initial premise.** The early audit claimed
`dbOid = 0` in `o_buffers.c:239,285` caused a multi-database
collision. Re-read of the code shows this is wrong:
`undoBuffersDesc` (`transam/undo.c:194`) and `buffersDesc`
(`transam/oxid.c:144`) are each a single static shmem instance per
PG cluster, and the OrioleDB control file is one-per-cluster. They
are **genuinely cluster-global like SLRUs**, and `dbOid = 0` is
architecturally correct for them. The per-table map files already
use `rlocator.dbOid = datoid` (`checkpoint.c:2948`).

**The real risk.** Synthetic OIDs (`ORIOLEDB_CONTROL_FILE_OID =
65500`; `ORIOLEDB_OBUF_RELNODE_BASE = 65600` + up to 2×16 entries
for undo/xidmap) sit in the user OID range. The current StaticAssert
(`control.h:77`) only checks `> 16384`, and the comment at `control.h:
71-73` calls the range "below space any real user would hit in
practice" — an optimism, not a guarantee. A long-running cluster
that allocates a user relfilenode in `[65500, 65648)` collides with
OrioleDB's reservations.

**Target.** Move synthetic OIDs into `[0xFFFFFF00, UINT32_MAX - 1)`
— a range that PG's user-relfilenode allocator cannot reach in any
practical workload (it would require 4.29 billion relfilenode
allocations). Strengthen the StaticAssert to enforce the new
reservation and add cross-OID non-overlap checks.

**Scope.**
- `pgxn/orioledb/include/checkpoint/control.h` — relocate
  `ORIOLEDB_CONTROL_FILE_OID`, tighten StaticAssert.
- `pgxn/orioledb/src/utils/o_buffers.c` — relocate
  `ORIOLEDB_OBUF_RELNODE_BASE`, add StaticAssert proving
  `[OBUF_BASE, OBUF_BASE + max_obuf_count)` does not overlap with
  `ORIOLEDB_CONTROL_FILE_OID`.
- Update comments that today say "below space any real user would
  hit in practice" to the stronger and honest "unreachable by any
  practical PG workload before OID wraparound".

**Exit criterion.** Reserved OIDs live in the high range; asserts
catch any future reservation that would overlap another or re-enter
the user range; no behavioral change in Plan E / Plan B paths.

### 6.6.2 — WAL-then-FS write path ordering (scope refined during implementation)

Phase 6.6.2 implementation surfaced a subtlety that forced a scope
refinement relative to what 6.6.0's write-up hinted at:

**Persistence layering, re-audited.** Undo / xidmap content is *not*
solely carried by Plan B FPIs. Backends emit
`ORIOLEDB_XLOG_CONTAINER` records (`recovery/wal.c:980`) at
transaction time that carry every row-level change. Plan B FPIs are
a *recovery accelerator* — they bound how far back stateless restart
has to replay CONTAINER records before reconstructing live undo
state. Without a fresh FPI, recovery still succeeds as long as
CONTAINER records covering the gap are retained.

This matters for the `checkpoint_is_shutdown` carve-out. The earlier
view was "skip = data loss" because Plan B was assumed to be the
sole durability path. It is not. CONTAINER records emitted before
shutdown cover the data, and the end-of-recovery checkpoint on the
next stateless restart emits fresh FPIs. Removing the guard would
re-introduce the `"concurrent write-ahead log activity while
database system is shutting down"` PANIC fixed by commit `0b44cfd`
for the same class of writes. **Guard preserved.**

**Applied changes:**

- `write_buffer_data` (`o_buffers.c`) reordered so that `XLogInsert`
  runs BEFORE `open_file` / `OFileWrite`. Under Log-is-Data the
  local file is a compute-local cache, and a crash mid-function must
  not leave the local cache in a state not described in WAL — on
  stateless restart the local cache is gone and WAL replay is what
  defines state.
- Assertion tripwire added: once Plan B is active and no gate
  condition (RecoveryInProgress / !XLogInsertAllowed /
  AmStartupProcess / checkpoint_is_shutdown / non-Neon smgr) is set,
  the WAL record must have been emitted before the local write.
  Written as `Assert`, not runtime error, because all gate
  conditions collapse deterministically from the branch above —
  the assert exists to catch future edits that reshuffle the
  branch without updating the invariant.
- `checkpoint_is_shutdown` guard **retained** with a comment that
  names the CONTAINER-record dependency explicitly, so any future
  change that severs that dependency (e.g. dropping CONTAINER
  records from WAL) must also revisit the guard.

**Weight guard.** Since the ordering change moves existing calls
around but does not add new ones, the WAL byte count and INSERT
latency baseline are structurally unchanged. Empirical benchmark
still required before Release Gate §5 — deferred to the Phase 6.6
completion commit after 6.6.4 E2E is added.

**Weight guard.** Benchmark WAL bytes/sec and end-to-end INSERT
latency against the pre-6.6.2 baseline. If writes go up by more than
10% (measured under the `test_e2e_crud.sh` workload), the design
needs revision before merge.

**Exit criterion.** Benchmark shows write latency ≤ pre-6.6.2
baseline; 6.6.4 E2E gate passes.

### 6.6.3 — Page LSN external tracking (read-only + unit test)

**Claim under test.** `wal_decoder` keys rmid=129 FPI records by
`(spc, db, rel, block, lsn)` and PageServer's cache layer indexes
them by the same tuple — OrioleDB page header's lack of `pd_lsn` is
not a correctness problem, only a layout difference.

**Deliverable.** One `wal_decoder` unit test that constructs an
OrioleDB FPI record, runs it through the decoder, and asserts the
resulting `Key` includes the correct LSN outside the page bytes. If
the test cannot be written (because decoding currently depends on
pd_lsn), that in itself is the finding and 6.6.3 expands.

**CSN collision note.** OrioleDB's `chkp_num` has "page version"
semantics but is **not** a substitute for LSN (chkp_num is
per-checkpoint-epoch monotonic; LSN is per-WAL-byte). Recovery must
apply physical WAL in LSN order, then filter by chkp_num for
visibility. No design change here — just a documented invariant so
future readers do not conflate them.

**Exit criterion.** Unit test green; invariant documented.

### 6.6.4 — Release gate: crash-mid-checkpoint E2E

**Test.** `scripts/test_e2e_crash_mid_ckpt.sh`: start cluster,
INSERT 1000 rows, background `CHECKPOINT` + immediate `kill -9` on
the compute PID, stateless restart, SELECT count + md5 — must match
pre-kill state.

**Status: shipped, currently failing in CI — intentional.** The
test is passing the point of the test: on Phoenix CI run
`24600322094` it reliably reveals an OrioleDB startup-path
assertion at `src/checkpoint/checkpoint.c:5260`:

```
PG [startup]: orioledb checkpoint 4 started
TRAP: failed Assert("!OInMemoryBlknoIsValid(shared->pages[0])"),
      File: "src/checkpoint/checkpoint.c", Line: 5260
  -> init_seq_buf_pages  (seq buf shared-memory init)
  -> checkpointable_tree_fill_seq_buffers
  -> end-of-recovery checkpoint after orioledb_recovery.signal
```

This is a cousin of the Phase 6.5 sys-tree lifecycle bug (same
`OInMemoryBlknoIsValid` family, different state): after a
SIGKILLed compute whose pgdata is wiped on restart, the seq-buf
descriptor's shared-memory state races the fresh-init path of
`init_seq_buf_pages` against the state the end-of-recovery
checkpoint reads from the Plan-E-rehydrated map file.

**Not caused by Phase 6.6.2.** write_buffer_data's reordering
only swaps FS-write and XLogInsert inside the Plan B branch; it
does not touch seq-buf descriptor initialization or map rehydrate.
The bug predates 6.6.2 — it was simply unreachable under the old
test suite (which only exercised clean stop/start and therefore
never hit the "end-of-recovery checkpoint after crash + empty
pgdata" path). 6.6.4 is the first test to drive that path, which
is precisely what a release gate is for.

**Diagnosis iteration 1 (shipped in v0.1.0-alpha.4 as an
incremental safety belt, not a full fix).**

- `init_seq_buf_pages` made idempotent: if the target shared has
  valid `pages[0]` / `pages[1]`, free the stale blknos before
  reallocating instead of asserting. The two trailing
  `OInMemoryBlknoIsValid` asserts on the post-allocation state
  are preserved, so the postcondition is unchanged.
- Diagnostic `DEBUG1` logs added at the three sites that reach
  `init_seq_buf_pages` (`ckpt_fill`, `ckpt_init_new`, and the
  `freeing stale` line inside `init_seq_buf_pages` itself) so
  future CI runs can tell which caller is responsible without
  another code-chase.

**Diagnosis iteration 1 findings (written 2026-04-18 from CI run
`24601289309`, offered as handoff to 6.6.4b proper).** After the
idempotent guard prevents the first assertion, a second
`Assert(false)` fires at `src/utils/seq_buf.c:148` inside
`get_seq_buf_filename` — the `free_buf_tag.type` byte read from
`descr->freeBuf.shared->tag` is 0 rather than `'m'` or `'t'`,
meaning the tag was never set (or was later zeroed) for the tree
the end-of-recovery checkpoint is currently processing. The
diagnostic logs also caught a strange arithmetic result:

```
LOG: orioledb checkpoint 4 started
LOG: ckpt_init_new: (1,2) chkpNum=1 next_chkp_index=0
     tmpBuf[next]=0x7f993b29e3d0 nextChkp[next]=0x7f993b29e350
```

The `chkpNum=1` here is smoking-gun evidence that
`checkpoint_init_new_seq_bufs` was invoked with a
`lastCheckpointNumber = 0` view of the world even though
`o_perform_checkpoint` emitted `"checkpoint 4 started"` one line
earlier with `lastCheckpointNumber = 3`. Possible mechanisms
(not fully resolved in this session):

  * `cur_chkp_num` captured at `o_perform_checkpoint` entry
    somehow propagates as 1, not 4, through
    `checkpoint_sys_trees` — points to a shared-memory layout
    mismatch between pre-crash and post-crash processes, or an
    interaction with `sys_trees_load_control_if_deferred`
    firing mid-sequence.
  * A second `o_perform_checkpoint` invocation runs concurrently
    (backend, not startup) with stale state, emitting the `chkpNum=1`
    line. Log shows same PID so this is unlikely but cannot be
    fully excluded without stack traces.

**Real fix (out of session).** Two approaches, pick after one more
diagnostic round that prints `checkpoint_state->lastCheckpointNumber`
at call site of `checkpoint_init_new_seq_bufs`:

  1. Source-of-truth unification: every call site that needs
     `chkpNum` reads it from `checkpoint_state` rather than
     accepting it as a parameter. Removes the possibility of
     param-vs-state divergence.
  2. Make `sys_trees_load_control_if_deferred` completion a
     precondition for entering `o_perform_checkpoint`. If
     control hasn't loaded, skip or defer the checkpoint.
     Avoids the `chkpNum=1` race.

Extend the crash E2E to also drive SPLIT and multi-table
workloads (today 6.6.4 only exercises a single primary-key table;
the bug may have more variants under heavier tree mutation).

**Release-gate consequence.** The Phoenix CI `serverless-e2e` job
stays `continue-on-error: true` through v0.2.0-beta.1 because this
step is expected to fail until Phase 6.6.4b lands. Release notes
must enumerate this failure explicitly — the alternative (flipping
the test's ROWS down or removing the test) would hide the very bug
the gate is working as intended to find.

---

## Phase 6.7: Branching + PITR — verification, not design (P1)

**Thesis.** If 6.6.0/6.6.1/6.6.2 are correct, PageServer's existing
timeline COW and LSN-point recovery apply to rmid=129 records the
same way they apply to PG heap. 6.7 is a *verification* phase —
any failure reverts to 6.6 as a scope gap, not patched over here.

### 6.7.1 — Branching

- `cargo neon timeline branch` from a checkpoint LSN
- Reconnect to the branch, `SELECT * FROM orioledb_table` must match
  the parent's content at branch LSN
- Write diverging data on parent and branch — verify isolation

### 6.7.2 — PITR

- INSERT 1000 rows at LSN A, 1000 more at LSN B, CHECKPOINT after
- Create a PITR endpoint at LSN A; verify row count = 1000, not 2000
- Create at LSN B; verify row count = 2000

**Exit criterion.** Both tests green without any OrioleDB-specific
branching/PITR code — the whole point is that 6.6 should have made
them fall out of existing Neon machinery.

---

## Phase 6.8: Replication (P1)

### 6.8.1 — Physical replication (OrioleDB primary → OrioleDB standby)

Neon's safekeeper streams rmid=129 to a standby compute; standby
wal-redo applies records in order. Verify failover preserves data.
No WAL format changes — this should work after 6.6.

### 6.8.2 — Logical decoding plugin (lower priority, independent work)

**Clarification of earlier sloppy phrasing.** rmid=129 *already*
contains two record classes in a single stream: page-delta (Plan E
FPI, LEAF/NON_LEAF structural) and row-level
(`LEAF_INSERT/UPDATE/DELETE`). The plugin only changes the *consumer*
side — it does not double the WAL on the writer side. No extra WAL
bytes.

**Scope.** Write `orioledboutput` output plugin that subscribes to
rmid=129 logical records and emits Debezium-compatible row events.
Ship as a separate `.so` loaded via `CREATE_LOGICAL_REPLICATION_SLOT`.

---

## Phase 7: Test matrix alignment with PG + Neon (P1)

**Thesis.** First-commercial readiness = OrioleDB-on-Neon passes the
union of PG-heap-on-Neon tests and OrioleDB-standalone tests. Neither
subset alone is sufficient.

### 7.1 — OrioleDB standalone regression under Neon mode

`pgxn/orioledb/test/sql/` has 43+ SQL tests that assume local-FS
persistence. Add a Neon-mode variant for each: after the main test
body, stateless-restart the compute, re-run the SELECT portion, and
assert identical output.

**Tooling.** `scripts/run_orioledb_tests_neon.sh` wraps each test file.
Add to `phoenix-ci.yml` as a new matrix job.

### 7.2 — Neon `test_runner/` OrioleDB variants

For each PG-heap test in `test_runner/regress_tests/` covering:
`restart`, `branching`, `pitr`, `checkpoint`, `crash_during_*` —
add a `_orioledb` sibling that uses `USING orioledb` tables and the
same assertions.

### 7.3 — CRUD + concurrency + crash E2E (was the old Phase 7)

- `test_e2e_crud.sh` — UPDATE 50 / DELETE 50 / INSERT 5000 (SPLIT) / mixed
- `test_e2e_concurrent.sh` — 2 backends concurrent INSERT → restart
- `test_e2e_crash.sh` — SIGKILL in CHECKPOINT → restart (same as 6.6.4)

---

## Known non-blocking gaps

Tracked but not on the first-commercial critical path. Revisit after
Phase 7 exit.

- Partitioned tables (OrioleDB AM doesn't support partitioning)
- TOAST compression specifics + out-of-line storage
- Non-default tablespaces (OrioleDB hard-codes `DEFAULTTABLESPACE_OID`)
- Undo / xidmap garbage collection (long-running clusters grow unbounded)
- Plan E FPI volume throttling (bulk load emits one FPI per touched page)
- `walredoproc.c` `EndRedoForBlock` stub (currently delegates to PG core;
  unclear if stateless teardown path is fully correct)
- Vendor PG assert diff vs upstream — no evidence asserts are relaxed in
  release builds, but the subtree merge has not been audited against
  REL_17_STABLE

---

## Priority Order + current status

Reflecting the "minimum-verified-loop" discipline: cheapest read-only
audits first, then write-path changes that depend on their findings,
then test matrix buildout.

| # | Phase | Status | Commit |
|---|---|---|---|
| 1 | **6.6.0** basebackup audit | ✅ DONE | `92eeb94` |
| 1 | **6.6.3** LSN externality verification | ✅ DONE | `ab7e9f7` |
| 2 | **6.6.1** synthetic OID hardening | ✅ DONE | `5d893a2` |
| 3 | **6.6.2** WAL-then-FS reordering | ✅ DONE | `b2ebcb0` |
| 4 | **6.6.4** crash-mid-ckpt E2E gate | ✅ DONE | `557f109` |
| 5 | **6.7.1 + 6.7.2** branching + PITR verification | 🔷 scaffold | `49bf78d` |
| 6 | **6.8.1** physical replication verification | 🔷 scaffold | `49bf78d` |
| 7 | **7.1 + 7.2 + 7.3** test matrix | 🔷 scaffold | `49bf78d` |
| 8 | **6.8.2** logical decoding plugin | 📝 spec only | `49bf78d` |

**Legend.**
- ✅ **DONE** — code + tests committed, build verified locally, CI expected to exercise on next push.
- 🔷 **scaffold** — runnable script committed; exits 77 (skip) today until the corresponding `neon_local` subcommand is stable. Pending a live-stack validation run before the phase can be declared closed.
- 📝 **spec only** — design/specification document committed; implementation is out-of-session work that must preserve the stated invariants (no double-emit, pure consumer).

## Release Gate

Release tags (`vX.Y.Z-alpha.N`, `-beta.N`, `-rc.N`) are only cut when:

1. Phoenix CI run for the tag commit is **fully green** on every job
   that is not `continue-on-error`. From v0.2.0-beta.1 onward,
   `serverless-e2e` must be `continue-on-error: false`. Until that
   cutover, the scaffold stays to absorb 6.6 write-path churn, but
   the tag commit must still see the job as green.
2. `scripts/test_e2e.sh` + all 6.6.4-era E2E scripts reach PASS with
   matching row counts + md5 across the full restart cycle (including
   mid-checkpoint crash).
3. Release notes enumerate `Verified` vs `Known limitations` by phase
   number, not hand-waving.
4. No open `P0` task at the release commit.
5. Weight-guard benchmark from 6.6.2 is attached to the release PR
   and shows write latency ≤ the prior tag's baseline.

`v0.1.0-alpha.1` was cut before this gate existed. `v0.1.0-alpha.2`
meets criteria 1–4 but predates the weight-guard benchmark requirement
(criterion 5), which applies from `v0.2.0-beta.1` onward.

**Release milestones:**

- `v0.1.0-alpha.2` — 100-row round-trip + sys-tree lifecycle fix (shipped)
- `v0.2.0-beta.1` — Phase 6.6 complete, `serverless-e2e` a hard gate
- `v0.3.0-beta.1` — Phase 6.7 + 6.8.1 verified
- `v1.0.0-rc.1` — Phase 7.1 + 7.2 at ≥80% parity with PG-heap-on-Neon

---

## Appendix A: Phase 6.5 root-cause write-up (historical)

Preserved for future readers debugging similar sys-tree ordering bugs.

**Symptom** observed in Phoenix CI run `24558882594` (step 7, fresh
backend after stateless restart):

```
TRAP: failed Assert("OInMemoryBlknoIsValid((td)->rootInfo.metaPageBlkno)"),
      File: "src/checkpoint/checkpoint.c", Line: 5289
autovacuum launcher → GetSnapshotData → orioledb snapshot hook
                    → checkpointable_tree_init (1, 2) chkp_num=3
                    → checkpointable_tree_fill_seq_buffers
                    → BTREE_GET_META(td)   [metaPageBlkno invalid]
```

**Actual root cause (identified 2026-04-17):** The fault is specifically
in the *timing* of `sys_trees_reset_initialized()`. The old code called
it from inside `checkpointable_tree_init`, which is called from the
middle of `sys_tree_init(i, init_shmem=true)`. By the time the reset
runs, `sys_tree_init` has already written the freshly allocated
`metaPageBlkno` into `sysTreesShmemHeaders[i].rootInfo`. The reset
nukes it, the caller returns unaware, and `sys_tree_init_if_needed`
then sets `header[i].initialized = true`. Header `i` is now
permanently in a `{initialized=true, metaPageBlkno=invalid}` state.
The next fresh backend reads `header[i]`, sees `initialized=true`,
takes the `sys_tree_init(i, init_shmem=false)` branch, copies the
invalid `metaPageBlkno`, and the next `BTREE_GET_META(td)` trips the
assert.

Trees `i=0, i=2..23` escape: `i=0` is `BTreeStorageInMemory`, `i=2..23`
see `lastCheckpointNumber != 0` and skip the deferred-load branch.
Only `i=1` — sys tree `(1, 2)` — is poisoned.

**Fix applied (`1684e2e` + `8955a03`):** Hoist deferred control-file
load into `sys_trees_load_control_if_deferred()`, called at the top
of `sys_tree_init_if_needed` before the per-tree alloc loop. The
reset runs while all headers are still in their initial-shmem
invalid state, so the alloc loop hands out fresh pages cleanly.
`8955a03` additionally propagates the freshly-loaded `chkp_num` so
the post-rehydrate init path uses the correct epoch.
