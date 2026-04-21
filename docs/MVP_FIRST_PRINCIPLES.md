# OrioleDB-on-Neon — Log-is-Data MVP First Principles

> **Status:** draft v0.1 — discussion artifact. No implementation commitment
> until this document is answered end-to-end and reviewed.
>
> **Purpose:** before any more code changes, agree on the minimum set of
> questions whose answers together determine a correct Log-is-Data
> architecture. Every prior implementation round (Phase 4 wal-redo, N1
> signal, N2 FPI-per-mutation, B0/B1/B2 dispatcher) skipped at least one
> of these questions and paid for it with a regression. This time we
> answer the questions first, then derive implementation.
>
> **Non-goals:** phase plan, PR list, migration steps. Those come after.

---

## 0 — Scope statement

"Log-is-Data" for OrioleDB-on-Neon means:

1. The WAL stream, as accepted by SafeKeeper and stored by PageServer, is
   the sole durable source of OrioleDB state.
2. Compute is stateless; any local files in pgdata are a transient cache
   derivable from (basebackup + on-demand PageServer page reads).
3. Any `(key, LSN)` can be materialized from the log, which makes
   branching / PITR / Git-for-Data possible at **semantic event
   granularity**, not page-byte granularity.

The present document does not prescribe how to achieve this. It
enumerates the questions whose answers determine whether any proposed
design qualifies.

---

## 1 — Invariants (the "what")

A design is a valid Log-is-Data design **iff** it satisfies all four.

### I1 — Log is the only persistence source

Every bit of state that must survive `kill -9 compute && rm -rf pgdata`
is either:

- Encoded as a WAL record that reached SafeKeeper, OR
- Derivable on-demand from what already reached SafeKeeper.

Excluded: transient runtime state that can be freshly initialized on
cold start (locks, page-pool LRU, per-process CSN allocator cursors,
caches).

Violation means data loss on stateless restart.

### I2 — Every record is self-contained at its LSN

Given (base established by prior log) + this record's payload, the
outcome at this LSN is deterministic. No record relies on "what
happened to still be in memory".

Violation means branching / PITR at arbitrary LSN is impossible.

### I3 — Any `(key, LSN)` is materializable from the log

PageServer produces page state at target LSN by: find the most recent
base image ≤ target LSN for key, apply every delta `(key, l)` with
base_lsn < l ≤ target_lsn, return result.

This requires:
- Every key that was ever written has ≥ 1 base image retained in the
  log window up to the target LSN.
- The delta chain length between consecutive base images is bounded.

Violation means unbounded walredo cost and/or missing pages.

### I4 — Compute cold-start does not replay the log

On stateless restart: basebackup gives PG catalog state; all OrioleDB
data / sys-tree pages come from PageServer GetPage on demand. No
`orioledb_recovery.signal`, no selective WAL replay, no streaming.

Violation reproduces the 6.6.4c-3 fragility (we were living on this
shore for weeks).

---

## 2 — FPI is NOT a semantic mechanism

**Restating a lesson from prior rounds**: FPI (full-page image) and
DELTA (increment description) are both valid encodings of the payload of
a single semantic event. Both satisfy I2. FPI satisfies I3 the same way
a single-link chain does — it *is* a base image.

Where FPI breaks down is at the **event-semantic layer**. A record at
LSN X that encodes "insert tuple T into leaf P of index I" is what
Git-for-Data needs to see as the event at LSN X. If the payload is an
integrated 8 KB page image, the **event semantics are lost in the wire
format** — readers can only see "the bytes of page P changed to
these". Branching on semantic diff, replaying one tuple-level event at
a time, or sharing a base between two branches all become strictly
harder or impossible.

Therefore:

- **L1 (event layer) must be stable.** Every record is exactly one
  semantic event; `xl_info` identifies the event; the set of event
  types is closed.
- **L3 (encoding layer) may vary per event.** Same event can be
  encoded IMAGE today and DELTA later. But the encoding is an
  implementation detail, not the contract.
- **No architectural decision is allowed to cement FPI into L1/L2.**
  If a question below has "answer: always FPI", that's a red flag
  about the question, not about FPI.

---

## 3 — The MVP questions

These are the five questions whose joint resolution determines whether
an implementation is compliant with I1–I4. Ordering reflects dependency.

### Q1 — Event schema closure

**The question.** Is the set of WAL event types closed over every
state transition that has persistent effect?

**Judgment criteria.** For every persistent state change an OrioleDB
operation causes, there is exactly one event type that encodes that
change. No state change is encoded by "whatever was in memory after
emit". No state change is encoded by a catch-all blob whose content
isn't inspectable at the event layer.

**Current state (to audit).** The declared event set in
`pgxn/orioledb/include/btree/page_walrecord.h`:

| Info | Name | Emit status | Encodes |
|---|---|---|---|
| 0x00 | `ORIOLEDB_XLOG_CONTAINER` | emitted | row-level sys-tree writes (DDL, undo, xidmap) — opaque blob replayed by compute-side `orioledb_redo` |
| 0x10 | `ORIOLEDB_XLOG_PAGE_INIT` | **reserved, not emitted** | new page materialized |
| 0x20 | `LEAF_INSERT` | emitted (currently FPI) | one tuple insert on a leaf |
| 0x30 | `LEAF_DELETE` | emitted (currently FPI) | one tuple mark-deleted |
| 0x40 | `LEAF_UPDATE` | emitted (currently FPI) | one tuple replaced |
| 0x50 | `LEAF_LOCK` | **reserved, not emitted** | row lock, tuphdr mutation |
| 0x60 | `COMPACT` | emitted (FPI) | page compaction (reclaims deleted tuples) |
| 0x70 | `SPLIT` | emitted (2× FPI) | page split into (left, right) |
| 0x80 | `MERGE` | emitted (FPI) | page merge |
| 0x81 | `PAGE_IMAGE` | emitted (FPI) | Plan E checkpoint + R22 post-split parent downlink |
| 0x90 | `ROOT_SPLIT` | **reserved, not emitted** | root split, new level |
| 0xA0 | `UNDO_APPLY` | emitted (FPI) | undo rollback applied |

**Open questions.**

1. **Reserved-not-emitted types (PAGE_INIT, LEAF_LOCK, ROOT_SPLIT)** —
   if they are never emitted, the state transitions they would encode
   must be subsumed by some other mechanism. Which one? Is the
   subsumption lossy (does compute-side memory state leak in)?
2. **CONTAINER (0x00)** is a blob carrying opaque sys-tree mutations.
   Does it satisfy I2? What is its payload schema precisely? Is it
   replayable by a pure function (walredo-compatible) or does it need
   compute-side state (this is the path that's been replayed by
   selective replay behind the signal — a direct I4 violation).
3. **Missing events?** Do any of these state transitions lack an
   event: CSN assignment at commit, undo-location advance at commit,
   xidmap insert/update at commit, sys-tree root downlink update,
   meta-page `ctid`/`bridge_ctid`/`numFreeBlocks` updates, S3 segment
   tracking (orthogonal — may be out of scope).

**Candidate answer direction.** The answer is a complete list: for each
OrioleDB state mutation, "which event encodes it, is that event pure,
is it currently emitted". The list either matches the current enum
exactly, or we find gaps that require either new events or reducing
CONTAINER into finer-grained events.

**Depends on.** Nothing upstream.

**Blocks.** Q2, Q3, Q4, Q5 all need the final event set.

---

### Q2 — Redo contract per event

**The question.** For every event type in Q1, is there a pure
function `redo(event, payload, base_pages) → output_pages` that
satisfies walredo's light-mode constraints?

**Judgment criteria.** For each event:

- Inputs: declared base pages (0 to N page-buffers from WAL block
  refs) + payload bytes (BufData + main data). No access to shmem,
  page pool, undo manager, oxid map, sys-tree lookups, comparators,
  tuple descriptors, or any global state.
- Outputs: the new state of each declared output page. Deterministic.
- Executable inside walredo light-mode (pgxn/neon_walredo + orioledb.so
  in `am_wal_redo_postgres` branch).

**Current state.**

| Event | Pure-function redo exists? | Notes |
|---|---|---|
| `LEAF_INSERT` | ✅ `orioledb_redo_leaf_insert` — page-local byte ops only | verified by reading page_redo.c |
| `LEAF_DELETE` | ✅ `orioledb_redo_leaf_delete` | |
| `LEAF_UPDATE` | ✅ `orioledb_redo_leaf_update` | |
| `LEAF_LOCK` | ✅ aliased to `leaf_delete` redo | only tuphdr is changed |
| `PAGE_INIT` | ❓ not implemented — today handled as FPI | needs payload schema + pure init |
| `COMPACT` | ❓ not implemented — FPI | redo could reorder/shrink items given a compaction plan in payload |
| `SPLIT` | ❓ not implemented — FPI | analogous to PG `_bt_split_redo`: init right from payload, truncate left |
| `MERGE` | ❓ not implemented — FPI | |
| `ROOT_SPLIT` | ❓ not implemented — FPI | meta-page update + new root init |
| `UNDO_APPLY` | ❌ **unclear if pure-function possible** | reversing a logical operation may require the undo record's full content as payload; if it does, it CAN be pure |
| `PAGE_IMAGE` | trivially: identity (redo is the IMAGE itself) | |
| `CONTAINER` | ❌ current redo path is `apply_btree_modify_record` which sits on `o_btree_modify` → needs B-tree context | this is the I4-violating path |

**Open questions.**

1. **UNDO_APPLY**: can the reversal be made a pure function of (prior
   page state + payload that fully describes the undo step), or does
   it need to walk the undo chain? The current `apply_undo_callback`
   does `MARK_DIRTY` and emits UNDO_APPLY FPI *after* applying
   in-memory undo — so the FPI captures post-undo state, making redo
   trivially identity. DELTA-encoding UNDO_APPLY requires redesigning
   the payload to describe the byte-level effect of one undo step, not
   the undo record itself.
2. **CONTAINER**: this record's redo goes through `apply_btree_modify_record`
   which navigates the B-tree. That's **not** a pure function for
   walredo. Options:
   - Replace CONTAINER with fine-grained events (one event per
     sys-tree key change).
   - Accept that CONTAINER cannot be walredo'd and move the affected
     state transitions to a different mechanism entirely.
3. **SPLIT / MERGE**: these touch ≥ 2 pages atomically. The redo
   function must output both new page states. Payload must encode the
   split point + item assignment. This is implementable but requires
   design work.

**Candidate answer direction.** Per event: write the exact function
signature and payload schema. Mark each as "pure OK", "pure requires
payload redesign", or "not feasible as pure". The "not feasible" set
either gets redesigned to become feasible or marks a subsystem
(typically CONTAINER) that needs an architectural solution beyond "add
redo function".

**Depends on.** Q1.

**Blocks.** Q3 (base image interaction), Q4 (encoding choice availability).

---

### Q3 — Base image lifeline

**The question.** For every `(key, LSN)` that may ever be queried, is
there guaranteed to be at least one base image `(key, base_lsn)` in
the retained log with `base_lsn ≤ LSN`, and is the delta chain between
successive base images bounded?

**Judgment criteria.** For every page that enters existence, a base
image at its birth LSN is guaranteed. For every page that lives past
some retention threshold, a refresh base image is emitted before the
old one is GC'd. Expected worst-case chain length is known and bounded.

**Current state.**

- **Birth**: new pages are created by PAGE_INIT / SPLIT (right page) /
  MERGE (merged page) / ROOT_SPLIT (new root). All currently emitted
  as FPI, so birth IS a base image. ✅
- **Refresh**: Plan E checkpoint writes every dirty page as FPI via
  `btree_smgr_write` during checkpoint. This refreshes bases on a
  per-checkpoint cadence.
- **Auto-refresh (PG-style first-write-after-checkpoint)**: absent.
  Pages clean at checkpoint but hot between checkpoints accumulate
  delta-chain length up to the next checkpoint.
- **PageServer layer compaction**: assumed to work on OrioleDB key
  space (same `(rel, blkno, LSN)` layout as PG heap) but not
  verified. If it works, compaction synthesizes image layers from
  delta chains on the storage side independently of compute.

**Candidate strategies.**

| Strategy | Cost | Worst-case chain | Sufficient for MVP? |
|---|---|---|---|
| **A.** Plan E checkpoint only | 1 FPI × dirty page × checkpoint | between checkpoints — can be huge for hot pages | **No** — unbounded walredo cost |
| **B.** A + first-write-after-checkpoint auto-FPI | 1 FPI × page × checkpoint cycle (at most) | ≤ mutations-per-cycle-per-page | **Yes** if auto-FPI condition is well-defined (Q4) |
| **C.** A + PageServer layer compaction | 1 FPI × dirty page × checkpoint + compaction-generated images | compaction-controlled | **Yes** if compaction verified |
| **D.** B + C | overlap | best | nice-to-have, not required |

**Open questions.**

1. **Does PageServer layer compaction work unchanged on OrioleDB keys?**
   The keys use `rel_block_to_key` same as heap. But OrioleDB
   `Value::WalRecord` has OrioleDB-specific payload. Layer compaction
   collapses Value::Image + Value::WalRecord chain into a newer
   Value::Image by calling walredo on the chain. It will call the same
   walredo that we're setting up. **So compaction works iff Q2
   answers "pure-function redo" for the event types that produce
   WalRecords.**
2. **What's the checkpoint cadence vs. per-page mutation rate in
   realistic workloads?** Determines whether strategy A alone is ever
   enough.
3. **Is strategy B required for MVP, or do we lean on C?** B adds a
   compute-side mechanism (read checkpointNum, emit FPI on threshold,
   update in-memory after Plan E — see Q4). C adds no compute-side
   complexity but depends on PageServer.

**Candidate answer direction.** MVP picks strategy B as the baseline
(self-sufficient compute-side guarantee). Verifies C empirically as
a relief mechanism. A alone is rejected.

**Depends on.** Q2 (for compaction feasibility).

**Blocks.** Q4.

---

### Q4 — Compute emit decision

**The question.** At `XLogInsert` time, how does compute decide whether
to encode the event as IMAGE or DELTA?

**Judgment criteria.**

- The decision must never emit DELTA when no base image exists at or
  before this LSN in the log (correctness violation — PageServer
  cannot materialize).
- The decision may emit IMAGE when a base exists (redundancy, WAL
  cost, but safe).
- The decision signal must be readable at emit time without blocking
  I/O to PageServer (obvious — we're inside XLogInsert).

**Current state.**

- All LEAF_INSERT/DELETE/UPDATE emit IMAGE unconditionally (N2
  first-cut). Strategy Q3/D-ish but wasteful.
- `OrioleDBPageHeader` has `state` (8 B, pg_atomic_uint64),
  `pageChangeCount` (4 B), `checkpointNum` (4 B). No pd_lsn.
- `checkpointNum` is updated on the on-disk page image during Plan E
  write (`io.c:write_page_to_disk`), **but not on the in-memory
  page**. Updates land back in memory only when the page is evicted
  and reloaded. This is a gap for any mechanism that reads
  `checkpointNum` at emit time.

**Candidate signals.**

| Signal | Precision | Robustness | Cost |
|---|---|---|---|
| **S1.** `page.checkpointNum < o_get_latest_chkp_num()` | per-page × per-checkpoint | needs in-memory update after Plan E and after own FPI emit | low |
| **S2.** compute-local map `(rel, blkno) → last_emitted_fpi_lsn` | per-page × per-emit | needs crash-persistent design or always-conservative on cold start | medium |
| **S3.** Per-page LSN field in header | per-page × per-emit | requires header layout change; conflicts with `state` at offset 0 | high (layout migration) |
| **S4.** Always emit IMAGE | trivially correct | no signal needed | very wasteful but a valid MVP fallback |

**Open questions.**

1. **If we pick S1**, is "per-page × per-checkpoint" precision good
   enough for I3's chain-length bound? Worst case: 1 FPI per page per
   checkpoint cycle. If checkpoint cadence is adequate, yes. If
   checkpoints are rare (OrioleDB's COW checkpoint can be infrequent),
   chain length within a cycle is unbounded again.
2. **If we pick S2**, what happens to the map on cold-start? Either
   (a) rebuild on first query (conservative for unknown pages, emit
   IMAGE), (b) persist in a sys-tree. (a) is simpler; (b) couples
   emit-decision state with log-persistence.
3. **If we pick S4 for MVP**, does that undermine Log-is-Data's
   semantic-event goal (Section 2)? Answer: yes — every event still
   carries semantic meaning (xl_info names it), but every event's
   payload is IMAGE. That's exactly the state we're trying to leave.

**Candidate answer direction.** MVP picks S1 and fixes the in-memory
update gap (Plan E + post-emit). Accept 1 FPI per page per checkpoint
cycle as the upper bound. Strategy B in Q3 = S1 in Q4.

**Depends on.** Q3.

**Blocks.** Q5 (partially — emit decision affects what sys-tree writes
look like which cold-start must read).

---

### Q5 — Compute cold-start state sources

**The question.** Enumerate every piece of runtime state compute must
initialize on cold-start. For each, identify its source: basebackup /
PageServer sys-tree / PageServer data page / lazily rebuilt / nowhere
(designed gap).

**Judgment criteria.** No item in the list resolves to "nowhere" or
"compute-side WAL replay". Every item resolves to one of the four
allowed sources. The sum of sources is sufficient to start accepting
connections and running transactions.

**Current state — state inventory to be verified.**

| State | Needed at | Current source | Compliant with I4? |
|---|---|---|---|
| PG catalog pages | anytime | basebackup | ✅ |
| OrioleDB data pages (IOT leaves, internals) | on table access | PageServer GetPage | ✅ |
| OrioleDB sys-tree meta pages (where roots live) | on first sys-tree read | PageServer GetPage (assumed) | ❓ needs verification |
| Sys-tree root downlinks | on first tree descent | PageServer GetPage of meta page | ❓ |
| `CATALOG_XID_UNDO_LOCATION` entries | on xid → undo lookup | PageServer GetPage of sys-tree leaf | ❓ |
| `CHKP_NUM` value (latest committed chkp) | at startup, for deciding sync_lsn | `.orioledb_sync_lsn` file + sys-tree | ❓ |
| Max CSN / next CSN to assign | on BEGIN of first new txn | **unclear** — currently rebuilt by selective replay | ❌ violates I4 |
| Max undo location / next undo offset | on first undo write | **unclear** — same as above | ❌ |
| xidmap (OXID → CSN mapping) for recent txns | on tuple visibility check | sys-tree GetPage | ❓ |
| `EXTENTS_OFF_LEN` / `EXTENTS_LEN_OFF` | on new-page allocation | sys-tree GetPage (assumed) | ❓ |
| Meta-page atomic counters (`ctid`, `bridge_ctid`, `numFreeBlocks`, `leafPagesNum`) | on various ops | **unclear** — meta-page is a page, so GetPage, but atomics reset on load | ❌ possibly |

**Open questions.**

1. **Max CSN / max undo location** — the two that break I4 today via
   selective replay. The question is: at what LSN in the log is the
   latest value captured? Is it captured via:
   - M1.2 / M1.3 commit-barrier writes to undo/xidmap sys-trees — if
     yes, reading the sys-tree on startup resolves it, and the
     barrier is the mechanism that makes it reach PageServer.
   - Something else currently not captured at all — in which case
     I4 is structurally unachievable without new mechanism.
   This needs verification by reading the M1.2/M1.3 code and tracing
   what keys they update.
2. **Atomic counters on meta-page** — e.g.
   `BTreeMetaPage.ctid` is a `pg_atomic_uint64`. On cold-start, after
   GetPage, the atomic is reinitialized from the byte content. But
   atomic updates in a running system — do they produce WAL records?
   If yes, great. If no, the counter is persisted only at checkpoint
   (via Plan E FPI of meta-page), which may be stale.
3. **Sys-tree meta-page location** — how does compute know which
   `(rel, blkno)` is the meta-page for sys-tree N on startup? Probably
   hardcoded by sys-tree ID, but verify.

**Candidate answer direction.** Complete the inventory. Every item
must resolve to a source compliant with I4. Items that don't resolve
require redesign: either (a) ensure the state is written to a sys-tree
or catalog via a WAL record that reaches PageServer, or (b) the state
is truly not persistent and can be re-derived safely (e.g., next CSN =
max-committed-CSN-in-xidmap + 1).

**Depends on.** Q1 (knowing all events that affect this state).

**Blocks.** Nothing — Q5 is the terminal question for I4 compliance.

---

## 4 — How to use this document

1. **Phase 0 — answer Q1 to closure.** Go through OrioleDB state
   transitions systematically (building on N3's sys-tree audit as a
   start) and produce a definitive mutation → event mapping. Any
   transition that doesn't map to an event surfaces a gap.
2. **Phase 0.5 — answer Q2 per event.** For each event, draft the
   redo function signature and payload schema. Flag the ones that
   can't be pure.
3. **Phase 0.75 — decide Q3 strategy.** Given Q2, is compaction
   viable? Is strategy B (first-write-after-checkpoint) required?
4. **Phase 0.9 — decide Q4 signal.** Pick S1/S2/S3/S4 and commit to
   fixing the gaps.
5. **Phase 0.95 — complete Q5 inventory.** Verify I4 compliance item
   by item.

Only when Q1–Q5 are answered do we derive an implementation plan. The
implementation plan is a consequence of the answers, not a separate
design step.

---

## 5 — What this document is NOT

- **Not an implementation plan.** No file paths, no PR list, no migration
  steps. Those come after Q1–Q5 are closed.
- **Not a decision record.** This is a question statement. Decisions
  made from it get recorded in `ENTERPRISE_HARDENING_PLAN.md` under
  whatever phase name they end up with.
- **Not a justification for discarding prior work.** Phase 4's
  `page_redo.c` correctly answers Q2 for LEAF_INSERT/DELETE/UPDATE.
  M1.2/M1.3 commit barriers are probably on the critical path for Q5.
  A-phase side-channel removal is a prerequisite for any DELTA
  routing. These stay.

---

## 6 — Review log

- v0.1 (current) — first-principles draft, ready for iteration.
