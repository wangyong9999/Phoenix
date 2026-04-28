# Walingest Summary v3 — comprehensive design

**Status**: design (pre-implementation). Closes G7 + G8 + T7 + T9a (B.1–B.5) via
a single walingest summary extension. Replaces the lock-based G7 fix v1
(deferred SPLIT WAL emit) with a reconciliation-based approach that aligns
with the long-term Phase 2.3 DELTA encoding direction.

**Date**: 2026-04-27.

---

## 1 — Why this exists

The current state has **four distinct correctness gaps** that share one root
cause and one fix shape:

| Gap | Symptom | Root |
|---|---|---|
| G7 | Linux CI intermittent: post-crash hikey-mismatch PANIC | SPLIT and parent-FPI not atomic across XLogInsert; Plan E checkpointer can race ahead between them |
| G8 | DELETE-heavy workload + SIGKILL: parent-vs-merged-leaf inconsistency | MERGE has same shape as G7 |
| T7 | undoLocation cold-start: leaves whose `undoLocation` is set after last checkpoint return stale values; `load_first_historical_page` reads past-end | walingest summary doesn't track per-tree max undoLocation |
| T9a B.1 | High-INSERT workload after crash: post-restart INSERT may collide with pre-crash ctid | meta-page atomic counters (ctid) live in shmem; lost on crash; walingest doesn't track them either |
| T9a B.2–B.5 | Operational counters drift on cold-start | same root as B.1 |

**Common root**: walingest summary v0.2 is a per-tenant blob with shmem
scalars. It does **not** track per-tree state. All five gaps are forms of
"per-tree state needs walingest reconciliation between checkpoints". One
extension closes all of them.

---

## 2 — Direction B (chosen) vs alternatives

| Direction | Mechanism | Closes G7? | Closes G8? | Closes T7? | Closes T9a? | Long-term aligned? | Lock-based hack? |
|---|---|---|---|---|---|---|---|
| A v1 (current) | Defer SPLIT WAL emit to iter 2 | ❌ (Linux race) | n/a | ❌ | ❌ | ❌ | n/a |
| A v2 | Hold left+right locked across both XLogInsert | ✅ (with deadlock risk) | ✅ separately | ❌ | ❌ | ❌ | **YES** ← rejected |
| **B (chosen)** | walingest reconciliation: SPLIT_FINALIZE + summary v3 | ✅ | ✅ | ✅ | ✅ | ✅ Phase 2.3 foundation | ❌ |

Per CLAUDE.md / INVARIANTS.md, lock-based atomicity workarounds are
explicitly disallowed for commercial readiness ("不能遗留 Racing 锁问题"). A v2
fails this gate. B's mechanism is purely log-driven and idempotent.

---

## 3 — Architecture overview

```
                                              compute (cold-start)
                                                    │
                                                    │ basebackup
                                                    ▼
       OrioleDB                                ┌────────────────┐
       (WAL emit side)                         │ summary v3     │
       │                                       │                │
       │ ORIOLEDB_XLOG_SPLIT (leaf, leaf)      │ • next_oxid    │
       │ ORIOLEDB_XLOG_SPLIT_FINALIZE (parent) │ • next_csn     │
       │ ORIOLEDB_XLOG_MERGE                   │ • per_tree[]:  │
       │ ORIOLEDB_XLOG_MERGE_FINALIZE          │   - ctid       │
       │ ORIOLEDB_XLOG_LEAF_INSERT             │   - bridge_ctid│
       │ ORIOLEDB_XLOG_UNDO_APPLY              │   - undo_loc   │
       │                                       │   - free_blocks│
       ▼                                       │   - leaf_pages │
   SafeKeeper                                  │   - datafile_len│
       │                                       │ • pending_splits│
       ▼                                       │ • pending_merges│
   PageServer ─── walingest ───────────────────►                │
                  (summary updater)            │                │
                                               └────────────────┘
                                                    │
                                                    │ apply at compute init:
                                                    │ - seed shmem counters
                                                    │ - replay pending_splits
                                                    │   into compute meta page
                                                    │   IF unpaired (synthesis)
                                                    ▼
                                               OrioleDB shmem
```

**Key insight**: walingest is the canonical source of "what counters / pending
operations exist between last checkpoint and current LSN". Compute reads
this on cold-start; no rmid=129 replay required. I4 fully honored.

---

## 4 — Wire format v3

```
offset  size   field                                              version
0       4      magic = ORIOLEDB_STATE_MAGIC                       v1+
4       4      version = 3                                        v3
8       8      next_oxid                                          v1+
16      4      last_pg_xid_seen                                   v1+
20      4      _reserved                                          v1+
24      8      last_ingested_lsn_raw                              v1+
32      8      ingested_count                                     v1+
40      8      next_csn                                           v2+
─── v3 additions below ───
48      4      tree_count               (number of per-tree slots)
52      4      pending_split_count      (entries in pending_splits)
56      4      pending_merge_count      (entries in pending_merges)
60      4      _reserved                (alignment)
64      tree_count * 56  per_tree[]
        ┌─ tree id (u64): hash of (datoid, relnode, tree_type)
        ├─ ctid (u64)                                            T9a B.1
        ├─ bridge_ctid (u64)                                     T9a B.2
        ├─ num_free_blocks (u64)                                 T9a B.3
        ├─ leaf_pages_num (u64)                                  T9a B.4
        ├─ datafile_length[2] (u64 × 2)                          T9a B.5
        └─ undo_location (u64)                                   T7
…       pending_split_count * 32  pending_splits[]               G7
        ┌─ tree_id (u64)
        ├─ left_blkno (u32)
        ├─ right_blkno (u32)
        ├─ child_hikey_lsn (u64)   (LSN of the SPLIT record)
        └─ child_hikey_offset (u32)
        └─ _reserved (u32)
…       pending_merge_count * 24  pending_merges[]               G8
        ┌─ tree_id (u64)
        ├─ left_blkno (u32)
        ├─ parent_blkno (u32)
        └─ merge_lsn (u64)
```

**Tree ID derivation**: `hash(datoid, relnode, tree_type)`. tree_type
distinguishes user-tree, sys-tree (O_TABLES, O_INDICES, etc.), bridge index.
64-bit hash collision risk per tenant is negligible.

**Sparse encoding**: trees not yet seen since last checkpoint don't appear in
per_tree[]. Compute, when reading the summary, falls through to reading the
meta page via PageServer GetPage for any tree not in the summary (the
GetPage path returns the last-checkpoint version, which is correct fallback).

---

## 5 — Reconciliation logic (walingest side)

### G7 / SPLIT pairing

```
On ORIOLEDB_XLOG_SPLIT (leaf+leaf, no parent yet):
  pending_splits.append({tree_id, left, right, hikey_ref})

On ORIOLEDB_XLOG_SPLIT_FINALIZE (parent FPI):
  match by tree_id + (left, right)
  if matched:
    pending_splits.remove(entry)
    per_tree[tree_id].leaf_pages_num += 1
  else:
    log warning (unexpected — likely test scenario)

On compute cold-start apply:
  for split in pending_splits:
    synthesize parent downlink update from child hikey
    bump compute's meta-page chain
```

### G8 / MERGE pairing — same shape with MERGE_FINALIZE

### T9a B.1 / ctid

```
On ORIOLEDB_XLOG_LEAF_INSERT (or LEAF_UPDATE, LEAF_DELETE; bumps occur on insert):
  decode tuple ctid
  per_tree[tree_id].ctid = max(per_tree[tree_id].ctid, tuple_ctid + 1)
```

### T7 / undoLocation

```
On ORIOLEDB_XLOG_UNDO_APPLY:
  decode undoLocation from record
  per_tree[tree_id].undo_location = max(prior, undoLocation)

On ORIOLEDB_XLOG_LEAF_INSERT/UPDATE/DELETE that ALSO emits an undo record:
  decode undoLocation
  per_tree[tree_id].undo_location = max(prior, undoLocation)
```

### T9a B.3, B.4, B.5

Drive from the same WAL stream. Detail spec deferred to Round 1 design follow-up.

---

## 6 — Compute cold-start apply

`apply_orioledb_cold_start_summary` (already exists for v2; extend for v3):

1. Seed `xid_meta->nextXid = next_oxid`
2. Seed `nextCommitSeqNo = next_csn`
3. For each `per_tree[i]` entry:
   - Locate tree by `tree_id` (or skip if tree not yet opened in shmem)
   - Seed `metaPage->ctid`, `bridge_ctid`, `numFreeBlocks`, `leafPagesNum`,
     `datafileLength[]`, `undo_location`
4. For each `pending_splits[i]`:
   - Synthesize the parent-side downlink update in compute's tree state
   - This is the I4-honoring substitute for replaying the missing SPLIT_FINALIZE
5. Same for `pending_merges[i]`

If compute hasn't opened a particular tree yet at cold-start (most cases —
trees open lazily on first SQL access), per_tree entries are queued for
deferred application at tree-open time. This matches the current pattern
in `evictable_tree_init_meta`.

---

## 7 — OrioleDB-side WAL emit changes

### G7 — restore pre-fix SPLIT emit; add SPLIT_FINALIZE

`pgxn/orioledb/src/btree/split.c::perform_page_split`:
- Restore the `orioledb_page_wal_split(left, right)` immediate emit
  (revert the deferred-emit logic added in 247b43b)
- Leaf split is now WAL-durable as soon as perform_page_split returns

`pgxn/orioledb/src/btree/insert.c::o_btree_insert_split` (R22 site, level > 0):
- After inserting the parent downlink in-memory:
- Replace the current 3-blkref atomic emit with a **2-blkref FPI** of
  parent only, info = `ORIOLEDB_XLOG_SPLIT_FINALIZE` (new)
- Add explicit reference to the matching SPLIT record's LSN/blkno via
  XLogRegisterData payload

The deferred-emit window (v1's flaw) is closed because perform_page_split
emits immediately. The lock window (A v2's flaw) is avoided because we
don't hold cross-XLogInsert locks. Reconciliation handles the gap.

### G8 — wire MERGE_FINALIZE the same way

`pgxn/orioledb/src/btree/merge.c::btree_try_merge_pages`:
- Restore the merge_pages emit immediate (already done in 0910d1d via
  `orioledb_page_wal_merge`; keep as-is)
- Add `ORIOLEDB_XLOG_MERGE_FINALIZE` for the parent-downlink-delete step
- Defer parent unlock until MERGE_FINALIZE is emitted (current pattern)

### LEAF_INSERT — already emits ctid

No emit-side change. walingest summary updater extracts ctid from existing
record payload.

### UNDO_APPLY — already emits undoLocation

No emit-side change.

---

## 8 — New record types

```c
/* pgxn/orioledb/include/btree/page_walrecord.h */
#define ORIOLEDB_XLOG_SPLIT_FINALIZE   0xB0   /* G7 reconciliation marker */
#define ORIOLEDB_XLOG_MERGE_FINALIZE   0xC0   /* G8 reconciliation marker */
```

Both are FPI records on the parent page only. walredo's light-mode
already handles per-page FPI without context (I2-clean). Both records carry
a XLogRegisterData payload: `{tree_id, paired_record_lsn}` so walingest can
match the pair without scanning back.

---

## 9 — Risk register

| # | Risk | Mitigation |
|---|---|---|
| 1 | walingest summary blob grows unboundedly if SPLIT_FINALIZE is delayed (e.g., never emitted due to bug) | Bound `pending_splits` to N entries (default 1024); spill to a separate sys-tree if exceeded; emit a metric |
| 2 | Wire format v2→v3 migration: PageServer running v2 reads v3 from new walingest? | Reject v3 from v2 reader (UnsupportedVersion); upgrade is coordinated via deploy: walingest bumps version after PageServer is upgraded |
| 3 | Tree ID hash collision (different trees, same 64-bit hash) | Hash collision in 64-bit space among ~10 trees per tenant has probability < 2^-50; acceptable. Add a check: on collision, emit warning and use second-source disambiguation |
| 4 | Drop table / truncate invalidates per_tree[] entry | Walingest snoops O_TABLES catalog drop in CONTAINER records and removes corresponding per_tree[] / pending_* entries |
| 5 | Compute applies pending_splits[] but tree isn't open yet | Queue in per-tenant memory; apply on first tree-open; this matches B.5 flow |
| 6 | Direction B introduces walingest contract bug → corrupts summary | Comprehensive crash matrix; Direction B can be reverted if needed (pre-fix behavior is the fallback) |
| 7 | T9a B.5 datafileLength per-checkpoint slot — checkpoint number tracking complexity | Defer B.5 to Round 4 (split from B.1-B.4) if it complicates Round 1 |
| 8 | G3-family `copy_fixed_key tuplen` assert at ROWS≥200 may NOT be closed by T7 | T7 closes 1 specific manifestation (undoLocation=0). If G3-family at ROWS≥200 has a different root, separate investigation needed in parallel |

---

## 10 — Implementation rounds

Each round is independently testable + revertable.

| Round | Scope | Effort | Verification |
|---|---|---|---|
| **R0** | Update Q5 design doc + GAPS to reference v3 | 0.5 day | Doc-only |
| **R1** | wal_decoder v3 wire format scaffolding + unit tests | 1-2 days | `cargo test -p wal_decoder --features testing` passes |
| **R2** | OrioleDB SPLIT_FINALIZE emit + restore perform_page_split | 1-2 days | macOS test_e2e_crash_mid_ckpt 30/30 |
| **R3** | walingest SPLIT pairing + summary update | 2-3 days | macOS + Linux CI 5/5 |
| **R4** | OrioleDB MERGE_FINALIZE emit + walingest pairing | 1-2 days | DELETE-heavy crash test 10/10 |
| **R5** | Per-tree ctid (T9a B.1) | 1 day | new test: high-INSERT crash + ctid no-overlap verify |
| **R6** | Per-tree undo_location (T7) | 1-2 days | check if G3-family ROWS≥200 closes |
| **R7** | Per-tree free_blocks/leaf_pages (T9a B.3, B.4) | 1-2 days | operational counter spot-check |
| **R8** | Per-tree datafile_length (T9a B.5) | 1-2 days | per-checkpoint counter validate |
| **R9** | T3 hard gate flip + Phase 4 cleanup | 0.5 day | Linux CI 5+ consecutive green |

Total: ~12–17 working days. Each round is committable + revertable independently.

---

## 11 — Test additions (new scenarios)

1. **SIGKILL between SPLIT and SPLIT_FINALIZE** — explicit timing harness;
   verify reconciliation synthesizes parent state correctly
2. **SIGKILL between MERGE and MERGE_FINALIZE** — same shape for delete path
3. **SIGKILL during high-INSERT workload, restart, INSERT again** — verify
   ctid not reused (T9a B.1 closure)
4. **SIGKILL after UNDO_APPLY emit, restart, run rollback path** — verify
   undoLocation seeded correctly (T7)
5. **30× repeat of test_e2e_crash_mid_ckpt on Linux CI** — verify flake-rate
   approaches 0 (G7 closure on production-equivalent hardware)
6. **OrioleDB workload at ROWS=2000 ddl + concurrent post-restart** —
   verify G3-family assert doesn't fire (or, if it does, T7 isn't its root)

---

## 12 — What this does NOT cover

- **G3-family at ROWS≥200 in seq scan** if the root is NOT undoLocation
  cold-start: needs separate OrioleDB internals investigation
- **G4 — checkpointer assert** in test_e2e_crash_compressed: orthogonal,
  separate Gap
- **G6 — chrono OutOfRangeError**: env-specific, separate
- **F1 — 2PC support**: feature gap, scoped separately
- **F2 — physical replication**: feature gap, scoped separately
- **F3 — PITR / branching**: feature gap, scoped separately
- **Phase 2.3 DELTA encoding for INSERT/UPDATE/DELETE**: future; this work
  establishes the walingest summary pattern that will host it

---

## 13 — Acceptance criteria

This work is complete when ALL of the following hold:

1. `crash_mid_ckpt` step in Phoenix CI runs **20 consecutive times** with
   zero failures across both macOS and ubuntu-24.04
2. New crash test for SIGKILL-between-SPLIT-and-SPLIT_FINALIZE passes
3. New crash test for high-INSERT ctid no-overlap passes
4. Phoenix CI workflow's `continue-on-error: true` is removed; build is
   hard-gated on crash_mid_ckpt
5. GAPS scoreboard rows for G7, G8, T7, T9a all flipped to ✅ Closed
6. walingest summary v3 wire format documented in
   `docs/Q5_COLDSTART_SOURCES.md` §2 with C-side reader spec
7. No `continue-on-error` workarounds remain in any CI step

---

## 14 — Open questions for review

- Q1: Bound on `pending_splits[]` size — default 1024 entries safe? Workload
  estimate: 100 ops/sec × 10 sec checkpoint interval = 1000 ops. Mostly SPLIT
  is rare (only on tree growth). Safe.
- Q2: Should T9a B.5 (datafileLength per-checkpoint slot) be folded in or
  deferred? Recommendation: defer to R8 for incremental shipping.
- Q3: Should we proactively restructure SPLIT/MERGE FPI as DELTA (Phase 2.3
  work) in the same passes? Recommendation: NO; that's a separate semantic
  refactor and would balloon scope.
