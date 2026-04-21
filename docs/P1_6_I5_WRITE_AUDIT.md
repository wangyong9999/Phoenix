# P1.6 — I5-write Barrier Audit

> **Status:** v1.0 — 2026-04-21.
>
> **Scope:** `INVARIANTS.md §8` Audit #1 closure. Trace the actual
> ordering of commit-path WAL records relative to SafeKeeper durability,
> determine whether I5-write holds.
>
> **Finding:** I5-write violation confirmed. M1.2 / M1.3 records
> (undo FPI, xidmap FPI) are emitted **after** `XACT_COMMIT` is flushed
> to SafeKeeper and are **not** force-flushed. Crash between
> XACT_COMMIT-flushed and walsender-pushes-M1.2/M1.3 loses the Oriole
> commit state for just-committed xids. This is the concrete mechanism
> of the "6.6.4c-3 count=0" symptom; the `orioledb_recovery.signal`
> always-on workaround (task #24) merely re-replays records that are
> NOT at SafeKeeper — it does not fix the gap.

---

## 1 — Scope and method

**Question** (per `INVARIANTS.md §5 I5-write`): for a committing
transaction, does the set of WAL records carrying its effects reach
SafeKeeper atomically? In particular, is there an ordering such that
xidmap CSN durability precedes OR follows XACT_COMMIT durability
**atomically** (not partially)?

**Method.** Trace the physical ordering of WAL records emitted during
a standard user-tuple commit, from first pre-commit hook to the
return of the XACT_EVENT_COMMIT callback. Cross-reference with
SafeKeeper flush semantics.

**Scope boundaries.** This audit covers the most common commit
shape: a heap-joined Oriole transaction (user INSERT/UPDATE/DELETE on
an OrioleDB table with a synthesized PG heap xid). Independent-Oriole
and prepared-transaction (2PC) paths are edge cases noted in §6.

---

## 2 — Commit-path code trace

### 2.1 Call-graph (heap-joined txn, `synchronous_commit != off`)

```
CommitTransaction()                          // PG xact.c
  │
  ├── CallXactCallbacks(XACT_EVENT_PRE_COMMIT)
  │     │
  │     └── undo_xact_callback (undo.c:2038)
  │           └── case XACT_EVENT_PRE_COMMIT (undo.c:2145)
  │                 current_oxid_xlog_precommit()   // shmem only, no XLog
  │                 [wal_joint_commit for SWITCH_LOGICAL_XID: rare]
  │
  ├── RecordTransactionCommit()              // PG xact.c
  │     │
  │     ├── XLogInsert(RM_XACT_ID, XLOG_XACT_COMMIT)   → at LSN  L_xact
  │     ├── XLogFlush(L_xact)                          → blocks until
  │     │                                                SafeKeeper has
  │     │                                                everything ≤ L_xact
  │     └── (sync_commit=remote_flush waits for WAL reach SafeKeeper)
  │
  └── CallXactCallbacks(XACT_EVENT_COMMIT)
        │
        └── undo_xact_callback
              └── case XACT_EVENT_COMMIT (undo.c:2175)
                    set_oxid_xlog_ptr(oxid, XactLastCommitEnd)
                                  // remembers L_xact in shmem; no WAL
                    current_oxid_precommit()
                                  // shmem COMMITTING marker
                    csn = pg_atomic_fetch_add_u64(&nextCommitSeqNo, 1)
                                  // shmem
                    current_oxid_commit(csn)   // oxid.c:1473
                          │
                          ├── Phase M1.2 (oxid.c:1508-1531)
                          │     fsync_undo_range(...)
                          │       ├── evict_undo_to_disk
                          │       │     └── write_buffer_data (o_buffers.c:253)
                          │       │           └── XLogInsert(rmid=129,
                          │       │                  PAGE_IMAGE)        → L_u1, L_u2, ...
                          │       └── o_buffers_sync (o_buffers.c:700)
                          │             ├── o_buffers_flush            // writes to local file
                          │             └── FileSync                   // fsync LOCAL file
                          │                                           // (no XLogFlush)
                          │
                          ├── set_oxid_csn(oxid, csn)        // writes shmem xidBuffer
                          │
                          └── Phase M1.3 (oxid.c:1577-1588)
                                o_buffers_write(&buffersDesc, ..., OXID_BUFFERS_TAG, ...)
                                      └── write_buffer_data
                                            └── XLogInsert(rmid=129,
                                                   PAGE_IMAGE)          → L_x
                                o_buffers_sync(...)
                                      ├── o_buffers_flush               // local file
                                      └── FileSync                      // fsync LOCAL
                                                                        // (no XLogFlush)
```

### 2.2 Key ordering facts

- **L_xact is flushed to SafeKeeper** synchronously inside
  `RecordTransactionCommit` when `synchronous_commit >= local` (and
  under Neon's default `remote_flush`, this blocks until SafeKeeper
  ack).
- **L_u1..L_un (M1.2 undo FPIs) and L_x (M1.3 xidmap FPI) are appended
  to the WAL stream AFTER L_xact, at LSNs greater than L_xact.**
- `o_buffers_sync` and `fsync_undo_range` perform `FileSync` on the
  local OBuffers files — **neither calls `XLogFlush`**. Verified:
  `o_buffers.c:700-729`, `undo.c:1730-1765`.
- The entire OrioleDB source tree contains **only two** `XLogFlush`
  calls — `wal.c:586` (rollback path) and `undo.c:2197` (independent
  Oriole txn's own `wal_commit` flush). Neither covers the M1.2/M1.3
  records.

### 2.3 Durability reality in Neon

Neon's durability boundary is SafeKeeper ack. The mechanism: compute's
`walsender` streams WAL to SafeKeeper; `XLogFlush(lsn)` under
`sync_commit=remote_flush` blocks until SafeKeeper has acked `lsn`.
Records inserted but not flushed are in compute's WAL buffer / local
`pg_wal` files but **not necessarily** at SafeKeeper.

Under low traffic, `walsender` may not push L_u1..L_x to SafeKeeper
for an unbounded time. Under high traffic, the next commit's
`XLogFlush(L_xact_next)` will transitively flush L_u1..L_x (since
L_u1..L_x < L_xact_next). But **the window between
L_xact-flushed and L_u1..L_x-flushed is unbounded by design**.

---

## 3 — Finding: I5-write violation

**Claim.** The commit path as-coded violates I5-write. Specifically:
after a successful commit returns to the client, the following state
can persist indefinitely (until next commit or WAL-writer cycle):

| Durable at SafeKeeper | Not yet at SafeKeeper |
|---|---|
| PG `XACT_COMMIT` for xid X | OrioleDB xidmap CSN for xid X (M1.3 FPI, at L_x) |
| PG's commit decision says X is committed | OrioleDB undo-page content for xid X (M1.2 FPIs, at L_u1..L_un) |

**Crash in this window produces the following post-restart state:**

- PG replay of XACT_COMMIT marks xid X as committed (PG layer).
- OrioleDB reads xidmap block from PageServer — PageServer got
  xidmap state only up to L_xact, missing L_x. The xidmap entry for
  X reads as stale (typically `COMMITSEQNO_INPROGRESS` from before X
  started, or an earlier `COMMITTING` marker).
- Visibility check for rows inserted by X: PG's heap xid is
  committed, but Oriole-side CSN lookup returns non-committed → rows
  appear invisible to any reader.
- **Symptom**: `SELECT count(*)` shows a count lower than expected.
  Inserted rows still occupy leaf slots (their LEAF_INSERT records
  DID reach SafeKeeper as part of the pre-commit emit sequence), but
  MVCC hides them.

This is **exactly the 6.6.4c-3 symptom** (see `task #21` diagnosis
result; `commit 284618a` documents it as remaining blocker after
6.6.4c-1 / 6.6.4c-2 were fixed).

### 3.1 Why M1.2/M1.3's in-code comment is wrong

Both blocks' comments state:

> "Commit's XACT record's XLogFlush then pushes all the FPI records
> to SafeKeeper." — `oxid.c:1499`, paraphrased `oxid.c:1561`

This is **inverted**. The XACT_COMMIT XLogFlush executes inside
`RecordTransactionCommit`, which PG's callback ordering puts **before**
XACT_EVENT_COMMIT fires. The M1.2/M1.3 XLogInsert calls happen in
the callback — i.e., **after** the XACT_COMMIT flush. The comment's
implicit assumption (M1.2/M1.3 emit pre-XACT-flush) is not how the
code is structured.

### 3.2 Why the `signal always-on` workaround (task #24) doesn't fix it

The `orioledb_recovery.signal` path post-restart copies WAL from
SafeKeeper back into compute's `pg_wal/` and has PG replay selectively
into OrioleDB state. This works ONLY for records that reached
SafeKeeper in the first place. Records at LSNs > L_xact that were
never flushed are not in SafeKeeper's WAL, thus not in the copied
`pg_wal/`, thus not replayed. The workaround "unblocks" 6.6.4c-3 for
typical high-traffic test workloads where the next commit's flush
catches the tail, but leaves a real data-loss window under sparse
traffic or synchronous crash immediately after commit.

---

## 4 — Concrete crash scenarios

### 4.1 Sparse-commit workload

Workload:
1. Client BEGIN; INSERT 1 row; COMMIT at T0.
2. Idle for 30 seconds.
3. `kill -9 compute`; stateless restart.
4. Client reconnects, `SELECT count(*)` — **returns 0** (row invisible)
   despite PG-layer commit being durable.

Root cause: between T0 and kill, walsender may push or may not push
L_u1/L_x to SafeKeeper. Under sparse traffic, the push is not
guaranteed to happen before the 30s idle period ends.

### 4.2 Burst-commit immediate-crash

Workload:
1. 1000 sequential INSERTs each as own txn.
2. Last COMMIT returns to client.
3. `kill -9 compute` within microseconds of last COMMIT.
4. Restart → count could be anywhere from 0 to 999.

Observed in the E2E crash tests (6.6.4c-3 reports count=0 or low
numbers).

### 4.3 `synchronous_commit = off`

Workload: any commit with `sync_commit=off`.

Both XACT_COMMIT and M1.2/M1.3 records are async. On crash:
- Losing XACT_COMMIT is acceptable (PG async-commit semantics).
- But losing M1.3 while keeping XACT_COMMIT is still possible because
  XACT_COMMIT is at an earlier LSN; walsender might have pushed up
  to L_xact but not past it.

This is a narrower form of §4.1.

---

## 5 — Proposed fix

### 5.1 Option A (preferred): force-flush at end of `current_oxid_commit`

Add an `XLogFlush(GetXLogWriteRecPtr())` call immediately after the
M1.3 block in `current_oxid_commit` (oxid.c:1588, just after
`o_buffers_sync`). Gated on the same `smgr_hook != NULL` condition.

Net effect: commit returns only after both XACT_COMMIT and
M1.2/M1.3 records are at SafeKeeper. I5-write holds.

**Cost.** One extra WAL flush per commit in Neon mode. Under
`synchronous_commit=remote_flush`, this is one extra SafeKeeper
round-trip. Typical cost ~sub-millisecond; high-traffic workloads
amortize naturally since M1.2/M1.3 records follow L_xact at tiny
byte offsets, so the marginal "wait for SafeKeeper" delta is small.

**Simplicity.** Single-site change. No reordering of callbacks. No
impact on standalone OrioleDB.

### 5.2 Option B: move M1.2/M1.3 to XACT_EVENT_PRE_COMMIT

Reorder so M1.2/M1.3 emit before `RecordTransactionCommit`. Then PG's
XACT_COMMIT XLogFlush transitively covers M1.2/M1.3.

**Downside.** CSN assignment needs to happen at pre-commit instead of
commit, which means a pre-committed Oriole txn may fail to commit at
PG layer and leave the CSN as a "ghost" assignment. Cleanup adds
complexity.

### 5.3 Option C: change PG callback ordering

Insert a post-XACT-flush but pre-return hook in PG's
`RecordTransactionCommit`. Invasive PG-core change. Not viable.

### 5.4 Recommendation

**Option A.** Minimal, local, preserves existing callback semantics.

Drop-in sketch:

```c
/* end of current_oxid_commit(), after M1.3's o_buffers_sync(...) */
if (smgr_hook != NULL) {
    /*
     * Phase M1.4 (Neon Log-is-Data commit barrier — force-flush):
     * M1.2 + M1.3 emit XLogInsert records at LSNs > L_xact.
     * PG's RecordTransactionCommit already flushed L_xact before
     * XACT_EVENT_COMMIT fired. Flush again up to the current WAL
     * insert position so the M1.2/M1.3 records reach SafeKeeper
     * atomically with commit return.
     */
    XLogFlush(GetXLogWriteRecPtr());
}
```

(Function name `GetXLogWriteRecPtr` vs `GetXLogInsertRecPtr` vs
`XactLastRecEnd` — exact choice TBD in implementation; likely
`GetXLogInsertRecPtr()` since M1.3's XLogInsert is the last emit
before this point.)

---

## 6 — Residual edge cases (do not block Phase 2.1)

- **Independent-Oriole txn path** (undo.c:2180-2202 where
  `wal_commit(...)` is called, then `XLogFlush(flushPos)` at 2197 for
  sync-commit). This path DOES call XLogFlush, but only on `flushPos`
  returned by `wal_commit`, which is the WAL_REC_COMMIT LSN — emitted
  BEFORE `current_oxid_commit` runs. Same structural issue: M1.2/M1.3
  emit after. Fix applies identically.
- **Prepared transactions (2PC)**. Not audited; `PrepareTransaction`
  has its own xact-event sequence. Phase 2.2 follow-up.
- **Abort path symmetry**. `undo_xact_callback` XACT_EVENT_ABORT does
  emit WAL_REC_ROLLBACK (wal.c:559) which itself calls XLogFlush on
  `wait_pos` under sync-commit (wal.c:586). `current_oxid_abort` only
  writes shmem (`set_oxid_csn(oxid, COMMITSEQNO_ABORTED)`, oxid.c:1606)
  — no WAL emit. Since abort's ACK to client is not guaranteed under
  crash, and since signaled-abort rows are rolled back via UNDO_APPLY
  on restart regardless of xidmap state, no equivalent I5-write hole
  exists on abort. **Not a gap.**

---

## 7 — Impact on EXECUTION_PLAN

`EXECUTION_PLAN.md` v1.0 lists **A.6 — I5-write barrier fix** as
conditional under Track A, contingent on P1.6 finding a gap. **This
audit confirms the gap. A.6 is now a firm Phase 2.1 prerequisite.**

Specifically:

- A.6 must land before `orioledb_recovery.signal` retirement (Phase
  3). Without A.6, retirement exposes the latent bug that the signal
  path currently masks (imperfectly) via workaround.
- A.6 is a small change (single XLogFlush insertion). Can be landed
  early in Phase 2 without waiting for the rest of Track A.
- A.6 test scope includes §4.1 (sparse-commit crash) and §4.2
  (burst-commit immediate-crash) scenarios — the N8 crash matrix
  should be extended.

### 7.1 Revised Phase 2 ordering

With A.6 now confirmed as a blocker:

```
Phase 2 start
  │
  ├── A.6 (force-flush in current_oxid_commit)         ← urgent, small
  │     └── extended N8 crash test under §4.1 / §4.2
  │
  ├── B.3 (walingest OrioleDB-state summary structure) ← parallel, larger
  │
  ├── B.4 (summary extended to Q5 inventory)           ← parallel
  │
  ├── C.1–C.3 (basebackup delivery + compute new codepath) ← after B.3/B.4
  │
  [A.6 + B.3/B.4 + C land + 2-week observation]
  │
  └── Phase 3 (retire signal path)
```

A.6 is additive (doesn't remove anything), so it's safe to land
immediately after P1.6 audit conclusion without waiting on the
larger Phase 2.1 tracks.

---

## 8 — Update to `INVARIANTS.md §8`

Audit #1 ("I5-write compliance of M1.2 / M1.3") **resolves to
VIOLATION**. Replace §8 item 1 text with:

> **Audit 1 (closed 2026-04-21): I5-write violation confirmed.**
> Commit-path trace in `docs/P1_6_I5_WRITE_AUDIT.md` shows M1.2/M1.3
> emit WAL records after `XACT_COMMIT` is flushed, and those records
> are not themselves force-flushed. Window between commit return and
> next WAL-writer cycle can lose the Oriole-side commit state
> (xidmap CSN + undo FPIs) while PG-side XACT_COMMIT is durable. Fix
> tracked as Track A.6 in `docs/EXECUTION_PLAN.md`, now a firm Phase
> 2 prerequisite. Resolution: single `XLogFlush` at end of
> `current_oxid_commit` under `smgr_hook != NULL`.

---

## 9 — Change log

- **v1.0 (2026-04-21)** — initial audit. Finding: I5-write violation
  confirmed by commit-path code trace. Cross-references: 6.6.4c-3
  symptom (task #21), signal-always-on workaround (task #24), M1.2
  comment self-contradiction (oxid.c:1499). Fix proposal: Option A
  (force-flush at end of current_oxid_commit).
