# OrioleDB-on-Neon — Log-is-Data Invariants (I1–I5)

> **Status:** v1.0 — authoritative. Any design or implementation change in
> this repo must satisfy all five. A proposal that contradicts an
> invariant is invalid until either the proposal is corrected or this
> document is updated (with recorded rationale).
>
> **Reading order:** §0 context → §1–§5 per-invariant → §6 dependency
> map → §7 verification log → §8 open audits → §9 change log.

---

## 0 — Why this document exists

Log-is-Data means the WAL stream, as committed at SafeKeeper and
ingested by PageServer, is the sole authoritative source of OrioleDB
state. Compute is stateless.

Prior implementation rounds (Phase 4, N1, N2, B0/B1/B2) each skipped
at least one of the invariants below and paid for it with a
regression. This document records the closure set. Every future change
must be checkable against these five.

Compactness is intentional: invariants are contracts, not plans. The
detailed reasoning and candidate implementations live in
`MVP_FIRST_PRINCIPLES.md`, `LOG_IS_DATA_ARCHITECTURE.md`, and
`ENTERPRISE_HARDENING_PLAN.md`.

---

## 1 — I1 : Log is the only persistence source

**Statement.** Any OrioleDB state change observable by a future reader
must be carried by a WAL record that reached SafeKeeper, or must be
deterministically derivable from records that reached SafeKeeper.

**In scope:** committed tuple data, index entries, xidmap CSN
assignments, undo-location advances, sys-tree schema, free-space
bookkeeping, CHKP_NUM, any other state whose absence at cold-start
would mis-answer a SELECT.

**Out of scope (runtime-only):** locks, page pool LRU, comparator
caches, backend-local allocator cursors, in-flight request state.

**Violation example.** A counter kept only in shmem, advanced on
mutation, not mirrored to WAL — crash loses it, next cold-start can't
reconstruct it.

**Couples with.** I3 (no point in WAL-ing something PageServer can't
materialize). I5-write adds an atomicity refinement on top.

---

## 2 — I2 : Each record is semantically self-contained at its LSN

**Statement.** Given (declared base pages established by prior log) +
(payload bytes), the redo function for the event type at this LSN
produces a deterministic output. The redo function's allowed inputs
are explicitly restricted to:

- Bytes of the block-ref buffers declared by the record.
- Payload bytes: `XLogRecGetBlockData` (BufData) + `XLogRecGetData`
  (main data).
- Walredo-process-global constants (`BLCKSZ`, endianness, page-version
  tables).

**Forbidden inputs** (any read of these breaks I2): shared memory,
process-local globals, the file system, network, clock, RNG, B-tree
descriptors, comparator / tuple descriptor, any sys-tree lookup,
oxid map, undo manager, page pool.

**Violation example (live).** `apply_btree_modify_record`
(`recovery/recovery.c:1858`) calls `o_btree_modify`, which requires
`BTreeDescr *tree` (comparator, tuple descriptor) and page-pool
access. The `ORIOLEDB_XLOG_CONTAINER` (info=0x00) replay path goes
through this. Such a redo cannot run inside walredo light mode — it
is an I2 violation.

**Couples with.** I3 (every record on a delta chain must be
I2-compliant to be appliable). I4 (only if I2 holds can
`walingest` consume a record without compute participation).

---

## 3 — I3 : Any (key, LSN) is materializable from the log

**Statement.** For any `(key, target_lsn)` that a legitimate query may
reference, PageServer can produce the page state by: (a) finding a
base image `(key, base_lsn)` with `base_lsn ≤ target_lsn`; (b)
applying every delta `(key, l)` with `base_lsn < l ≤ target_lsn` in
LSN order; (c) returning the result.

**Constraints this places on WAL emission.**

1. Every key that ever exists has at least one base image retained in
   the log window covering any reachable query LSN.
2. Base-image sources must be documented and sufficient: page-birth
   events (PAGE_INIT / SPLIT right / ROOT_SPLIT new root) produce an
   initial base; a refresh mechanism (Plan E checkpoint / first-write
   after checkpoint / PageServer layer compaction) keeps bases from
   drifting too far before delta chains grow unbounded.
3. Between consecutive base images for a key, the delta-chain length
   must have a known finite upper bound.

**Violation examples.**

- **Missing base.** Page is born with a record that carries no
  extractable initial state — chain has no anchor, `GetPage` fails.
- **Unbounded chain.** Hot page never re-FPI'd between checkpoints
  with wide intervals — `GetPage` latency diverges as deltas
  accumulate.
- **Non-pure delta.** Some delta on the chain is I2-violating —
  walredo cannot apply it; chain is effectively broken at that point.

**Couples with.** I1 (records must reach PageServer first). I2 (every
chain step must be purely appliable). I4 (compute serves all reads via
this path, no fallback).

---

## 4 — I4 : Compute cold-start does zero WAL replay

**Statement.** Compute startup does not apply any WAL records — neither
PG-layer (XACT / XLOG / CLOG / ...) nor OrioleDB-layer (rmid=129).
All state required to begin serving queries comes from:

- **Path A**: one-time delivery via `get_basebackup(lsn)` after
  `sync_safekeepers()` — PageServer's `wait_lsn` blocks until
  `walingest` has ingested up to that LSN, then a consistent snapshot
  is materialized and shipped.
- **Path B**: on-demand per-page reads via `GetPage(key, lsn)` —
  `wait_lsn` blocks the request if needed.

**How PG+Neon achieves this (verified behavior).** PageServer's
`generate_pg_control` (`libs/postgres_ffi/src/xlog_utils.rs:138-179`)
sets `checkpoint.redo = lsn` and `state = DB_SHUTDOWNED`. The shipped
WAL segment (`generate_wal_segment`,
`libs/postgres_ffi/src/xlog_utils.rs:432`) is empty (headers only).
PG-on-Neon's startup path is patched to skip crash recovery given
these markers. PG's shmem counters are initialized from the
`CheckPoint` struct that `walingest` maintains continuously by
consuming every XACT/NEXTOID/CHECKPOINT/RUNNING_XACTS record. The
comment at `xlog_utils.rs:159` is explicit: *"In Neon, we don't do
WAL replay at startup in either case."*

**Required for OrioleDB (design obligation, not yet implemented).**
The analog mechanism: `pageserver/src/walingest.rs` must maintain an
OrioleDB-state summary (nextOXID, nextCSN, undoLocationMax, running
OXIDs, CHKP_NUM, ...) from every rmid=129 record it ingests. This
summary must be deliverable via basebackup (as a pg_control
extension, a neon.signal extension, or a dedicated blob). Compute
reads it once to initialize OrioleDB shmem, then accepts queries —
no pg_wal/ copy, no recovery signal, no selective replay.

**Violation example (live).** The
`compute_tools/src/compute.rs:1772-1835` path copies SafeKeeper WAL
files into `pgdata/pg_wal/` and writes `orioledb_recovery.signal` to
trigger PG's recovery to selectively replay rmid=129 records, which
re-run `apply_btree_modify_record` on compute-side B-tree state. This
duplicates work PageServer's `walingest` already performs
(diverging paths, source of the 6.6.4c-3 class of bugs) and violates
I4 directly.

**Terminology note.** `walingest`'s processing of WAL is **ingest**,
not replay: each record is consumed once to update PageServer key
space + summary. Replay is the forbidden act of consuming
already-ingested WAL a second time, on the compute side, to rebuild
state that basebackup should have delivered.

**Couples with.** Depends on I1 (records reach SafeKeeper / PageServer)
+ I2 (walingest can apply without compute) + I3 (per-page materialization
works). Supports I5-read (transaction visibility after cold-start
uses xidmap + CSN, both reachable via I3).

---

## 5 — I5 : Transaction atomicity on write, transaction view on read

**Statement (two clauses, single invariant).**

- **I5-write (commit atomicity).** For a committing transaction, the
  set of WAL records carrying its effects (LEAF mutations on user
  trees, undo record writes, xidmap CSN assignment, per-xid
  undo-location write, XACT_COMMIT) reaches SafeKeeper as an atomic
  unit: either all members of the set have reached SafeKeeper or none
  of the post-commit subset have. There is no valid intermediate
  state in which xidmap records a CSN for xid X while XACT_COMMIT for
  X has not reached SafeKeeper (or inverse).
- **I5-read (transaction view at any LSN).** For any `target_lsn`,
  the transaction is observable as *entirely visible* or *entirely
  invisible* to any reader snapshot at that LSN, even though its
  effects span multiple LSNs. Visibility is resolved through the
  xidmap + CSN mechanism, not by requiring the reader to co-locate
  all the transaction's WAL records.

**Why both clauses.** I5-write is the emit-side guarantee. I5-read is
the visibility guarantee. Each is necessary and neither subsumes the
other — a system can satisfy I5-write (atomic commit bytes land
together) while still botching I5-read (visibility lookup races with
the flush).

**Violation examples.**

- **I5-write.** sys-tree barrier flushes the xidmap page before
  XACT_COMMIT is flushed. Crash in between: post-restart,
  `xidmap[42] = CSN X` is durable but PG's commit record is not.
  Reader sees the rows committed; PG's transaction accounting says
  aborted. Data integrity break.
- **I5-read.** Branch / PITR to a LSN mid-commit: reader sees some
  of the tuple inserts but xidmap does not yet have a CSN for their
  xid. Depending on fallback behaviour, reader sees half a
  transaction's effects as visible. Git-for-Data's
  "snapshot-at-arbitrary-LSN" semantic is broken.

**Couples with.** I1 (both clauses refine "reached SafeKeeper" into a
per-transaction atomicity). I3 (xidmap itself must be I3-materializable
for I5-read to work). I4 (cold-start-reconstructed xidmap must agree
with the commit bytes reality).

---

## 6 — Dependency map

```
I1 (persistence) ─────┬────▶ I3 (materialization)  ◀──────┐
                      │                                    │
I2 (per-record pure) ─┴────▶                               │
                             │                             │
                             ▼                             │
                      I4 (compute zero-replay)             │
                             ▲                             │
                             │                             │
I1 + I2 + I3 ─────────▶ I5 (transaction atomicity) ────────┘
```

- I1 + I2 are foundations.
- I3 is the PageServer-side materialization contract.
- I4 is the compute-side consequence: once I3 holds, compute lives
  entirely off basebackup + GetPage.
- I5 is the transactional overlay: I1/I2/I3 must hold *with respect
  to whole-transaction atomicity*, not just per-record determinism.

---

## 7 — Verification log (what was checked, not asserted)

Every invariant claim in §1–§5 that is phrased in the present tense
("does", "is", etc.) was verified against the code cited. Key anchors:

| Anchor | File:line | Verifies |
|---|---|---|
| `generate_pg_control` | `libs/postgres_ffi/src/xlog_utils.rs:138-179` | I4 Path A — pg_control instructs PG to skip replay |
| `generate_wal_segment` | `libs/postgres_ffi/src/xlog_utils.rs:432-506` | I4 — basebackup-delivered WAL segment is empty |
| `wait_lsn` in basebackup | `pageserver/src/page_service.rs:3761-3776` | I4 Path A — ingest-catchup is built into the API |
| `wait_lsn` in GetPage | `pageserver/src/page_service.rs:2222-2230` | I4 Path B — GetPage blocks until LSN ingested |
| `wait_lsn_timeout` | `pageserver/src/config.rs:647` | I4 — 60s ceiling on catchup wait |
| `orioledb_page_redo` | `pgxn/orioledb/src/btree/page_redo.c` (full file) | I2 — LEAF_INSERT/DELETE/UPDATE redo are pure functions |
| `apply_btree_modify_record` | `pgxn/orioledb/src/recovery/recovery.c:1858-1931` | I2 violation — CONTAINER replay calls `o_btree_modify` |
| `OrioleDBPageHeader` / `OrioleDBOndiskPageHeader` | `pgxn/orioledb/include/orioledb.h:333-367` | Basis for header-related claims across all invariants |
| `write_page_to_disk` | `pgxn/orioledb/src/btree/io.c:1595-1640` | Plan E writes on-disk header checkpointNum, not in-memory |
| `page_walrecord.h` event enum | `pgxn/orioledb/include/btree/page_walrecord.h:65-75` | Basis for I2 / Q1 event inventory |
| `compute.rs` OrioleDB recovery path | `compute_tools/src/compute.rs:1772-1835` | I4 violation — current signal + WAL-copy path |

Any claim here that can't be traced to a verified anchor is marked
"open audit" in §8, not "verified".

---

## 8 — Open audits (claims not yet verified)

These remain uncommitted. Each requires a targeted code read before a
design decision depends on it.

1. ~~**I5-write compliance of M1.2 / M1.3.**~~ **Closed 2026-04-21 —
   violation confirmed** (`docs/P1_6_I5_WRITE_AUDIT.md`). M1.2/M1.3
   emit WAL records *after* `RecordTransactionCommit` has already
   flushed `XACT_COMMIT` to SafeKeeper, and those records are not
   themselves force-flushed. Window between commit return and next
   WAL-writer cycle can lose Oriole-side commit state (xidmap CSN +
   undo FPIs) while PG-side commit is durable. Resolution: single
   `XLogFlush(GetXLogInsertRecPtr())` at end of `current_oxid_commit`
   under `smgr_hook != NULL` (tracked as Track A.6 in
   `docs/EXECUTION_PLAN.md`). Firm Phase 2 prerequisite: A.6 must
   land before `orioledb_recovery.signal` retirement.
2. **Completeness of the OrioleDB shmem startup inventory.** Q5 in
   `MVP_FIRST_PRINCIPLES.md` lists the candidate items. Each one must
   be traced: does its value live in a sys-tree key or a
   walingest-maintainable summary field? If some don't, I4 needs a new
   mechanism.
3. **PageServer layer compaction on OrioleDB keys.** Works unchanged
   if I2 holds for all events on the chain. Not yet empirically
   verified on an OrioleDB workload.
4. **Chain-length upper bound under realistic load.** I3 requires a
   finite bound; "finite" is met by any Plan-E-only strategy, but the
   practical bound under hot-page workloads is unmeasured.
5. **I5-read at arbitrary branch LSN.** No test currently covers
   branching mid-transaction and observing the snapshot. Needs a
   dedicated scenario in the N8 matrix.

---

## 9 — How to use this document

- **Every design proposal** must state, per invariant, whether it
  preserves / newly satisfies / or relies on an open audit.
- **Every PR** that changes WAL emit/apply, basebackup, or startup
  must cite the relevant invariant in the commit message.
- **An invariant change** requires an RFC-style discussion + a
  bumped version on this doc + a §9 change-log entry. Never a silent
  revision.

---

## 10 — Change log

- **v1.0 (2026-04-20)** — initial definitive set. Prior draft
  iterations in `MVP_FIRST_PRINCIPLES.md v0.1` distilled into five
  invariants after a session-long walk-through with external
  review.
