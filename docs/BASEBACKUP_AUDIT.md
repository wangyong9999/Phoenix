# Basebackup Semantics Audit (Phase 6.6.0)

**Audit date:** 2026-04-18
**Audit scope:** Read-only. No code changed.

## Question

In the Neon stateless restart flow, does the tar returned by
`get_basebackup(LSN)` contain any files from `orioledb_data/`?

## Finding

**No.** `get_basebackup()` tars only the 23 standard PostgreSQL
subdirectories from `PGDATA_SUBDIRS`. `orioledb_data/` is never
created, populated, or touched by the basebackup code path. On
stateless restart, `orioledb_data/` is absent from the extracted
tree and must be reconstructed entirely from WAL replay +
PageServer `GetPage` fallback.

This is **Possibility B** from the Phase 6.6 design discussion:
Plan B WAL mirroring is the **sole** durability layer for OrioleDB
undo/xidmap. It is not belt-and-suspenders.

## Evidence

### 1. PageServer tars only `PGDATA_SUBDIRS`

`pageserver/src/basebackup.rs:366-375` — the only place directories
are added to the archive:

```rust
let subdirs = dispatch_pgversion!(pgversion, &pgv::bindings::PGDATA_SUBDIRS[..]);

// Create pgdata subdirs structure
for dir in subdirs.iter() {
    let header = new_tar_header_dir(dir)?;
    self.ar
        .append(&header, io::empty())
        .await
        .map_err(|e| BasebackupError::Client(e, "send_tarball"))?;
}
```

`libs/postgres_ffi/src/pg_constants_v17.rs:20-44` lists all 23
entries — none of them is `orioledb_data`:

```rust
pub const PGDATA_SUBDIRS: [&str; 23] = [
    "global", "pg_wal/archive_status", "pg_wal/summaries",
    "pg_commit_ts", "pg_dynshmem", "pg_notify", "pg_serial",
    "pg_snapshots", "pg_subtrans", "pg_twophase", "pg_multixact",
    "pg_multixact/members", "pg_multixact/offsets",
    "base", "base/1", "pg_replslot", "pg_tblspc",
    "pg_stat", "pg_stat_tmp", "pg_xact",
    "pg_logical", "pg_logical/snapshots", "pg_logical/mappings",
];
```

### 2. basebackup.rs has zero OrioleDB references

A repository-wide grep for `orioledb` inside `pageserver/src/basebackup.rs`
returns no matches. There is no OrioleDB-specific directory enumeration,
no OrioleDB tablespace handling, no special file injection. The tar
producer is architecturally unaware of OrioleDB.

### 3. Compute extracts the tar into pgdata as-is

`compute_tools/src/compute.rs:1303-1305`:

```rust
let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(&mut reader));
ar.set_ignore_zeros(true);
ar.unpack(&self.params.pgdata)?;
```

No post-processing, no `orioledb_data/` creation. If the tar does
not contain it, it does not exist on disk after extraction.

### 4. Compute restart explicitly relies on PageServer, not local files

`compute_tools/src/compute.rs:1701-1709`:

```rust
// Architecture:
//   * B-tree data pages: page-level delta WAL + checkpoint FPIs
//   * Control file / map files: Plan E FPIs → PageServer GetPage
//   * Undo / xidmap: Plan B OBuffersDesc mirroring → PageServer
//   * In-memory xid/CSN state: XACT WAL replay (still required)
//
// On restart, PG reads orioledb_recovery.signal, enters crash
// recovery mode, replays only OrioleDB (rmid=129) + XACT records,
// then runs end-of-recovery checkpoint which emits fresh FPIs.
```

This is the architectural commitment: every OrioleDB-persistent
artifact comes from PageServer, not from local FS, not from the
basebackup tar.

### 5. Plan B writes are keyed into standard PageServer storage

`libs/wal_decoder/src/serialized_batch.rs:211-241` — rmid=129 FPI
records from Plan B are stored as `Value::Image` under the normal
`(spc, db, rel, blk)` key, the same place PageServer serves PG
heap pages from. That is how the read-side fallback at
`pgxn/orioledb/src/utils/o_buffers.c:287-298` can succeed via
`smgropen(rlocator, …) → smgrread(reln, MAIN_FORKNUM, blkno, page)`
— these calls route to Neon's smgr, which queries PageServer by
that same key.

Plan B therefore persists undo/xidmap into the ordinary rel/blk
keyspace, **not** into basebackup-packaged files.

### 6. ORIOLEDB_WAL_KEY_PREFIX is an orthogonal stream (not basebackup-packaged)

`libs/pageserver_api/src/key.rs:52-55`:

```rust
/// Key prefix for OrioleDB WAL stream.
/// OrioleDB WAL records have no block references — they're stored
/// as a relation-level stream and replayed as a whole by wal-redo.
pub const ORIOLEDB_WAL_KEY_PREFIX: u8 = 0x70;
```

This prefix is for OrioleDB delta WAL (row-level LEAF_INSERT etc.)
that wal-redo replays inside compute. It is **neither** in
`metadata_aux_key_range()` nor surfaced through basebackup. It is
orthogonal to Plan B persistence — Plan B uses normal rel/blk keys
(§5). Confirms basebackup is OrioleDB-blind.

## Implications for Phase 6.6.2

Confirmed: **WAL-then-FS write-path inversion is mandatory**, not
reducible to "confirm + assert".

With basebackup unaware of OrioleDB, the local `orioledb_data/`
directory is strictly ephemeral compute-local cache. Two concrete
risks in the current code that 6.6.2 must close:

1. **Write order is WAL-*after*-data.** `o_buffers.c:214-254` does
   `OFileWrite(...)` (local FS) **before** `XLogInsert(...)` (Plan B
   mirror). A crash after the local write and before the XLogInsert
   (or before the commit's XLogFlush sees the Plan B LSN) loses the
   write on the next stateless restart: local FS is gone, WAL has
   no record. Today this is mitigated because transactions don't
   ack until XLogFlush on the commit record, so clients see
   rollback — but the ordering is a code smell and prevents any
   future design that writes outside a transaction boundary (e.g.,
   autonomous maintenance writes).

2. **`checkpoint_is_shutdown` carve-out skips Plan B mirror**
   (`o_buffers.c:233`). Shutdown checkpoints emit undo/xidmap to
   local FS but **not** to WAL. On stateless restart after a
   "clean shutdown" the compute boots with an empty
   `orioledb_data/`, and WAL has no Plan B record for the pages
   written during the last shutdown checkpoint. Today this works
   only because stateless restart always re-runs end-of-recovery
   checkpoint and re-emits FPIs — but that is an implicit
   dependency; the guard has no reason to exist under
   WAL-then-FS discipline.

6.6.2 therefore must:

- Invert the order in `write_buffer_data`: `XLogInsert` first, local
  FS write demoted to cache-only (no fsync on the OrioleDB buffer
  boundary).
- Remove the `checkpoint_is_shutdown` skip on Plan B mirror emit.
- Add a write-path assertion that every non-empty local write is
  preceded by a successful `XLogInsert` returning a valid LSN.

Weight guard remains in force: after the inversion, measure INSERT
latency + WAL bytes/sec against the pre-6.6.2 baseline. Inversion
is expected to be **neutral or faster** (the local FS write no
longer blocks on disk IO in the critical path), but this must be
proven, not assumed.

## Open questions

- **The `.orioledb_initialized` marker lifecycle under crash-restart
  within the same endpoint instance** (compute.rs:1711). If compute
  crashes and is restarted by the orchestrator with the same
  `endpoint_dir`, the marker persists, so `was_initialized=true`
  triggers WAL replay. But the local `orioledb_data/` may or may
  not still exist depending on whether the endpoint_dir is volume-
  backed. Worth a follow-up audit in Phase 6.7 (branching/PITR
  uses the same marker logic at branch boundaries).

- **Plan B mirror write-error handling.** If `OFileWrite` succeeds
  but `XLogInsert` returns an error (currently unhandled — code
  falls through), the transaction is in a "data on local FS but not
  in WAL" state. Under 6.6.2's inverted order this becomes "WAL but
  not on local FS" which is correct (local FS is cache, WAL is
  authoritative). So 6.6.2 implicitly fixes this.
