# OrioleDB-on-Neon — Project Instructions

This repo is a fork of Neon adapted to run OrioleDB as a fully
serverless, Log-is-Data storage engine. The work is mid-flight; the
architectural contract is documented and stable, but many pieces of
the current codebase are interim solutions that will be replaced as
the contract is fully realized.

## Must-read before proposing any design change

**`docs/INVARIANTS.md`** — five invariants (I1–I5) that every design
and implementation change in this repo must satisfy. This document is
the result of several iteration rounds where prior proposals were
discovered to violate one or more invariants after landing. A proposal
that contradicts an invariant is invalid until either the proposal is
corrected or the invariant is explicitly updated with recorded
rationale.

Do not make substantive architecture / WAL / recovery / basebackup
proposals without stating how they interact with each invariant.

## Supporting documents

- **`docs/LOG_IS_DATA_ARCHITECTURE.md`** — North Star. The end-state
  architecture that the invariants are designed to preserve.
- **`docs/MVP_FIRST_PRINCIPLES.md`** — exploratory draft of the MVP
  question set that follows from the invariants (Q1–Q5 style). Still
  being iterated; the invariants doc is authoritative.
- **`docs/ENTERPRISE_HARDENING_PLAN.md`** — work queue, risk register,
  phase plan. Implementation-level.
- **`docs/ORIOLEDB_SERVERLESS.md`** — describes the current (interim)
  implementation. Parts of it — notably the `orioledb_recovery.signal`
  + selective-replay path — are known to violate I4 and are slated
  for replacement.

## Key facts that have been verified, do not re-derive

These were re-established by direct code reading in the session
ending 2026-04-20. Cite the file anchors rather than re-inferring:

- **Compute on Neon does not replay WAL at cold start.** Not PG-layer,
  not rmid=129. `libs/postgres_ffi/src/xlog_utils.rs:138-179` sets
  pg_control to `DB_SHUTDOWNED` + `redo = lsn`; Neon patches PG
  startup to honor this and skip crash recovery. The WAL segment in
  basebackup is a header-only empty segment
  (`xlog_utils.rs:432-506`). PG-layer shmem comes from the CheckPoint
  struct that `walingest` maintains continuously.
- **PageServer's `wait_lsn` blocks basebackup and GetPage until
  walingest has caught up.** See `pageserver/src/page_service.rs:3761`
  (basebackup) and `:2222` (GetPage). 60s timeout
  (`pageserver/src/config.rs:647`). Compute never needs to wait for
  PageServer manually.
- **`orioledb_page_redo` (`pgxn/orioledb/src/btree/page_redo.c`) is a
  pure-function redo** for LEAF_INSERT/DELETE/UPDATE. No B-tree
  context, no shmem access. Walredo light-mode can run it.
- **`apply_btree_modify_record`
  (`pgxn/orioledb/src/recovery/recovery.c:1858`) is NOT pure** — it
  calls `o_btree_modify` which needs BTreeDescr + comparator + page
  pool. CONTAINER-type (info=0x00) records go through this path and
  violate I2.
- **OrioleDB page header layout**
  (`pgxn/orioledb/include/orioledb.h:333-367`):
  `OrioleDBPageHeader = { pg_atomic_uint64 state; uint32
  pageChangeCount; uint32 checkpointNum }`. The first 8 bytes are
  `state`, not an LSN — PG's `PageGetLSN(page)` on OrioleDB in-memory
  pages is meaningless.
- **`write_page_to_disk` (`pgxn/orioledb/src/btree/io.c:1595-1640`)**
  writes `curChkpNum` into the on-disk header, does **not** update the
  in-memory page header — any mechanism reading `hdr->checkpointNum`
  at emit time must account for this staleness.

## What we're NOT doing

- **Don't revive the `orioledb_recovery.signal` / `pg_wal/` copy path
  as a long-term solution.** It is a known I4 violation, kept only as
  a crutch until the `walingest`-maintained OrioleDB-state summary is
  implemented.
- **Don't design around "compute will selectively replay rmid=129".**
  The architecture requires zero rmid=129 replay on compute.
- **Don't put FPI as a default answer.** FPI is one of two encodings
  for an event (the other is DELTA). Defaulting everything to FPI
  compromises the semantic-event granularity that Git-for-Data depends
  on. See `docs/INVARIANTS.md §1 I2` context.
- **Don't put a `pd_lsn`-equivalent field into `OrioleDBPageHeader`.**
  Prior sessions concluded OrioleDB's versioning (CSN +
  pageChangeCount + checkpointNum, plus PageServer's external
  `(key, LSN)` index) already covers what PG does with pd_lsn. No
  layout change is needed for MVP.

## Build and test

Standard Neon workflow. A few common entry points for OrioleDB work:

```bash
# Rust side
cargo build -p wal_decoder
cargo build -p pageserver
cargo test -p wal_decoder --lib --features testing

# Full build
make -j$(nproc)

# End-to-end crash scenarios
bash scripts/test_e2e_crud.sh
bash scripts/test_e2e_crash_data.sh
bash scripts/test_e2e_crash_ddl.sh
# More under scripts/test_e2e_*.sh
```

Session-local state for active work lives under `.neon/` (gitignored).
`cargo neon {init,start,stop,tenant create,endpoint create,endpoint
start}` drives the test harness.

## Commit conventions

- No AI / assistant / tool attribution in commit messages, code
  comments, or docs. Ever.
- Commit bodies are short (1-3 lines typical); split unrelated
  changes into separate commits instead of long bulleted bodies.
- Don't push to remote unless explicitly asked.
