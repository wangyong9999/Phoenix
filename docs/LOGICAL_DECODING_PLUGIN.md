# OrioleDB Logical Decoding Output Plugin — Specification

**Phase:** 6.8.2 (independent, lower priority)
**Status:** Specification only; no implementation in this repository yet.

## Problem statement

Consumers that subscribe to Postgres logical replication (Debezium, custom
CDC, audit pipelines) expect a per-row event stream with `INSERT` /
`UPDATE` / `DELETE` semantics and a transactional boundary. Neon's
built-in heap tables support this via `pgoutput` out of the box.

OrioleDB tables do not, today. All OrioleDB WAL is emitted under
`rmid = 129` (`ORIOLEDB_RMGR_ID`) as `ORIOLEDB_XLOG_CONTAINER` records
or Plan E / Plan B FPIs. `pgoutput` does not recognise rmid=129 — it
has no decoder for OrioleDB's record layout — so a `CREATE PUBLICATION
… FOR TABLE <orioledb_table>` is silently skipped by logical decoding.

## Non-goal: double-emit

A naive "solution" would be to have OrioleDB also emit heap-style WAL
for every row change, so `pgoutput` picks it up. **This is rejected**
as a violation of the Phase 6.6 weight invariant (WAL byte count
would roughly double for every OrioleDB row modification). The plugin
must be a **pure consumer** of what OrioleDB already writes.

## What OrioleDB already puts in WAL

`pgxn/orioledb/include/recovery/wal.h:34-52` defines the full set of
CONTAINER record subtypes. The row-level subset relevant to a logical
consumer:

| Subtype (decimal) | Name | Meaning |
|---|---|---|
| 1 | `WAL_REC_XID` | Assigns an OrioleDB-side xid to the current transaction |
| 2 | `WAL_REC_COMMIT` | Transaction commit boundary |
| 3 | `WAL_REC_ROLLBACK` | Transaction rollback boundary |
| 4 | `WAL_REC_RELATION` | Maps `(datoid, reloid, relnode)` to OrioleDB internal identifiers |
| 5 | `WAL_REC_INSERT` | Row insert — carries tuple payload |
| 6 | `WAL_REC_UPDATE` | Row update — carries old+new tuple when `REPLICA IDENTITY FULL`, else just new |
| 7 | `WAL_REC_DELETE` | Row delete — carries old tuple key (REPLICA IDENTITY shape) |
| 12 | `WAL_REC_JOINT_COMMIT` | Transaction touches both heap and OrioleDB |
| 15 | `WAL_REC_REINSERT` | Update that was implemented as delete+insert |
| 17 | `WAL_REC_SWITCH_LOGICAL_XID` | Ties an OrioleDB sub-xid to the heap top-xid (critical for correct CDC visibility) |
| 13 | `WAL_REC_TRUNCATE` | Table truncate |
| 10/11 | `WAL_REC_SAVEPOINT` / `WAL_REC_ROLLBACK_TO_SAVEPOINT` | Nested transaction boundaries |
| 18 | `WAL_REC_RELREPLIDENT` | REPLICA IDENTITY changes — changes subsequent row event shape |

Everything else (Plan E / Plan B FPIs, bridge records, replay feedback,
meta locks) is internal-use and not surfaced to the CDC consumer.

## Plugin contract

### Name
`orioledboutput` — a PG logical decoding output plugin, loaded the
same way `pgoutput` and `test_decoding` are.

### Hookable entry points

Standard logical decoding callbacks (see
`pgxn/orioledb/src/recovery/logical.c` for the existing parsing
harness this plugin will build on):

```
startup_cb    — verify OrioleDB extension is installed, read
                publication list, set REPLICA IDENTITY expectations.
begin_cb      — on WAL_REC_XID / WAL_REC_JOINT_COMMIT begin: emit
                BEGIN with the OrioleDB xid mapped to the PG top-xid
                via WAL_REC_SWITCH_LOGICAL_XID seen earlier.
change_cb     — on WAL_REC_INSERT/UPDATE/DELETE/REINSERT/TRUNCATE:
                emit the corresponding row event.
commit_cb     — on WAL_REC_COMMIT: emit COMMIT with the mapped xid.
```

### Record-to-event mapping

| WAL subtype | Event emitted |
|---|---|
| `WAL_REC_XID` | internal — feeds the xid→logical-xid map, no event |
| `WAL_REC_SWITCH_LOGICAL_XID` | internal — records the heap↔oriole xid bridge for the current tx |
| `WAL_REC_RELATION` | internal — refreshes the `(datoid, reloid, relnode)` lookup cache |
| `WAL_REC_INSERT` | `INSERT` with full tuple |
| `WAL_REC_UPDATE` | `UPDATE` with new tuple, plus old tuple if `REPLICA IDENTITY FULL` (else just the identity columns) |
| `WAL_REC_REINSERT` | `UPDATE` (exposed to consumer as a logical update; the delete+insert internal representation is flattened) |
| `WAL_REC_DELETE` | `DELETE` with identity columns |
| `WAL_REC_TRUNCATE` | `TRUNCATE` |
| `WAL_REC_COMMIT` / `WAL_REC_JOINT_COMMIT` | `COMMIT` (after buffering the above row events for the xid) |
| `WAL_REC_ROLLBACK` | discard all buffered events for that xid |
| `WAL_REC_SAVEPOINT` / `WAL_REC_ROLLBACK_TO_SAVEPOINT` | respect the subtxn boundary; rollback-to-sp trims the buffered events for the sub-xid |
| Plan E / Plan B FPIs | **ignored** — these are storage-level, not row-level |

### Transaction atomicity invariant

A consumer must see events from a single logical transaction in a
contiguous block bounded by one `BEGIN` and one `COMMIT`. When both
heap and OrioleDB changes happen in the same transaction, two xids
are active (PG top-xid for heap, OrioleDB-assigned xid for oriole
changes). `WAL_REC_SWITCH_LOGICAL_XID` is the bridge. The plugin
MUST emit a single `BEGIN top-xid` / `COMMIT top-xid` pair covering
the union of heap-emitted (via `pgoutput`) and oriole-emitted (via
`orioledboutput`) events — which means the two plugins need to
cooperate, or the output plugin slot needs to register interest in
both rmids.

**Recommended shape:** `orioledboutput` registers with logical
decoding for rmid=129 **only**, and assumes the subscriber will also
attach a `pgoutput` (or equivalent) slot for rmid=0 on the same
publication. Each plugin emits its own BEGIN/COMMIT pair scoped to
the top-xid; the downstream consumer merges by xid. This is the
same shape Debezium uses today for `heap` + `pgoutput`.

### Wire format

Option A: reuse `pgoutput`'s protocol verbatim, tagging the
relation id with the OrioleDB `reloid`. Pros: any existing
Debezium connector will pick up rows without changes. Cons: some
OrioleDB-specific concepts (REPLICA IDENTITY FULL nuances,
WAL_REC_REINSERT flattening) have to round-trip through pgoutput's
message vocabulary.

Option B: native JSON-like shape (`test_decoding`-style). Pros:
simpler implementation, no pgoutput wire-format reverse-engineering.
Cons: consumers need bespoke adapters.

**Recommendation: Option A.** Reuse pgoutput's wire format. The
plugin's job is to translate rmid=129 CONTAINER records into the
same LogicalDecodingMessage shapes `pgoutput` produces, so the
Debezium connector stays OrioleDB-agnostic.

## Dependencies on Phase 6.6

None. Logical decoding is orthogonal to the Log-is-Data invariants
6.6 closes. `orioledboutput` can be developed and shipped before or
after 6.6 completes.

## Implementation scope (out-of-session)

- New file `pgxn/orioledb/src/recovery/output_plugin.c` implementing
  the four callbacks above, reusing the existing
  `src/recovery/logical.c` parse machinery.
- `Makefile` target that produces `orioledboutput.so`.
- Regression test: `test_e2e_logical_decoding.sh` that creates a
  publication, attaches a subscription using
  `output_plugin = 'orioledboutput'`, issues
  `INSERT/UPDATE/DELETE` against OrioleDB tables, and asserts
  every row event reaches the subscriber in order.
- Documentation addendum: which OrioleDB record subtypes are
  internal (never surfaced), so downstream tools don't misinterpret
  Plan E FPIs as row events.

## Out of scope (explicitly)

- Changes to OrioleDB's WAL format. The plugin is a pure consumer.
- Changes to PageServer / wal_decoder. Logical decoding happens
  inside the compute process; PageServer is uninvolved.
- Multi-master / active-active replication.
- Schema evolution handling beyond what `WAL_REC_RELATION` and
  `WAL_REC_RELREPLIDENT` already encode — major DDL (ADD COLUMN,
  DROP COLUMN) reuses PG's heap-side catalog machinery and is
  implicitly handled.
