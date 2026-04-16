# OrioleDB on Neon: Serverless Log-is-Data Architecture

A research integration of [OrioleDB](https://github.com/orioledb/orioledb) (B-tree Index-Organized Table engine) on [Neon](https://github.com/neondatabase/neon) (serverless PostgreSQL with storage-compute separation). This project achieves **fully stateless compute** for OrioleDB tables — all data is recoverable from PageServer without persistent local state.

## What This Project Does

Standard Neon only supports PostgreSQL's native heap tables for serverless operation. OrioleDB uses a completely different storage engine (B-tree IOT with CSN-based MVCC and undo logs) that bypasses PG's buffer manager and smgr — so none of Neon's storage hooks work out of the box.

This project bridges that gap with two mechanisms:

- **Plan E (Checkpoint FPI):** During checkpoint, OrioleDB B-tree pages, control files, and map files are emitted as Full Page Images via `XLogRegisterBlock(REGBUF_FORCE_IMAGE)`. PageServer stores them as `Value::Image` and serves them via `GetPage` on stateless restart.

- **Plan B (Undo/Xidmap Mirroring):** Every undo log and xidmap write is mirrored as an FPI to a synthetic PageServer relation. On local file miss, reads transparently fall back to PageServer.

## Architecture

```
                    ┌─────────────────────────────────────────────┐
                    │           Stateless Compute Node            │
                    │                                             │
                    │  ┌──────────┐  ┌──────────┐  ┌──────────┐  │
                    │  │ OrioleDB │  │ PG Heap  │  │  Neon    │  │
                    │  │ B-tree   │  │ (native) │  │  smgr    │  │
                    │  └────┬─────┘  └────┬─────┘  └────┬─────┘  │
                    │  Plan E/B FPI  Std smgr      GetPage/WAL   │
                    └───────┼─────────────┼─────────────┼────────┘
                            │             │             │
                    ════════╪═════════════╪═════════════╪════ WAL
                            │             │             │
                    ┌───────▼─────────────▼─────────────▼────────┐
                    │              SafeKeeper                     │
                    └────────────────────┬───────────────────────-┘
                                        │
                    ┌────────────────────▼───────────────────────-┐
                    │              PageServer                     │
                    │  WAL Decoder (rmid=129 → FPI / WAL key)    │
                    │  Page Store  (Value::Image for all state)  │
                    └────────────────────┬───────────────────────-┘
                                        │
                              Cloud Storage (layer files)
```

### Data Flow Summary

| OrioleDB State | Mechanism | Write Path | Read Path |
|---------------|-----------|------------|-----------|
| B-tree data pages | Plan E FPI | Checkpoint → WAL FPI | `btree_smgr_read()` → PageServer |
| Control file | Plan E FPI | `write_checkpoint_control()` → WAL FPI | `get_checkpoint_control_data()` → PageServer fallback |
| Map files | Plan E FPI | `checkpoint_map_write_header()` → WAL FPI | `evictable_tree_init_meta()` → PageServer fallback |
| Undo logs | Plan B mirror | `write_buffer_data()` → WAL FPI | `read_buffer_planb_fallback()` → PageServer |
| Xidmap | Plan B mirror | `write_buffer_data()` → WAL FPI | `read_buffer_planb_fallback()` → PageServer |
| In-memory xid/CSN | WAL replay | OrioleDB row-level WAL | Selective crash recovery (rmid=129) |

### Stateless Restart Sequence

1. `sync_safekeepers` → get LSN
2. `get_basebackup(LSN)` → PG catalog at latest state
3. Inject `orioledb.skip_unmodified_trees = false`
4. Copy SafeKeeper WAL to `pg_wal/`, write `orioledb_recovery.signal`
5. PG starts → selective crash recovery (only rmid=129 + XACT records)
6. End-of-recovery checkpoint → emits fresh FPIs to PageServer
7. Normal operation → backends read via PageServer `GetPage`

## Modified Components

### OrioleDB Extension (`pgxn/orioledb/`) — 9 files, +536 lines

| File | Purpose |
|------|---------|
| `src/btree/io.c` | Plan E FPI emit + PageServer read for B-tree pages |
| `src/checkpoint/control.c` | Control file FPI + PageServer fallback |
| `src/checkpoint/checkpoint.c` | Map file FPI + PageServer fallback |
| `src/utils/o_buffers.c` | Plan B undo/xidmap mirroring |
| `include/utils/o_buffers.h` | `planBLogId` field |
| `src/transam/undo.c` | planBLogId = 1 (undo) |
| `src/transam/oxid.c` | planBLogId = 2 (xidmap) |
| `src/recovery/recovery.c` | Selective WAL replay |
| `src/orioledb.c` | `skip_unmodified_trees` GUC |

### Vendor PostgreSQL (`vendor/postgres-v17/`) — 5 files, +165 lines

| File | Purpose |
|------|---------|
| `xlogrecovery.c` | `orioledb_recovery.signal` + selective WAL replay |
| `xlog.c` | End-of-recovery checkpoint hook, assert relaxations |
| `xlogprefetcher.c` | Skip errors during OrioleDB recovery |
| `xlogrecovery.h` | `IsOrioleDbRecoveryRequested()` |
| `relcache.c` | OrioleDB table AM compatibility |

### Neon Stack (Rust)

| File | Purpose |
|------|---------|
| `libs/wal_decoder/src/serialized_batch.rs` | rmid=129 FPI → Value::Image |
| `libs/pageserver_api/src/key.rs` | ORIOLEDB_WAL_KEY_PREFIX |
| `compute_tools/src/compute.rs` | Recovery orchestration |
| `pgxn/neon_walredo/` | OrioleDB wal-redo fallback |

## Building

### Prerequisites

Same as upstream Neon — see [Neon build instructions](https://github.com/neondatabase/neon#running-local-installation).

Additionally: OrioleDB build requirements (ICU, lz4).

### Build

```bash
# Build everything (Neon + PG + OrioleDB extension)
make -j$(nproc) -s

# Build OrioleDB extension only
cd pgxn/orioledb && make install

# Check Rust components
cargo check -p compute_tools -p pageserver
```

### Quick Start

```bash
# Initialize Neon environment
cargo neon init
cargo neon start

# Create tenant and endpoint with OrioleDB
cargo neon tenant create --set-default
cargo neon endpoint create main
cargo neon endpoint start main

# Connect and use OrioleDB tables
psql -p 55432 -h 127.0.0.1 -U cloud_admin postgres
postgres=# CREATE EXTENSION orioledb;
postgres=# CREATE TABLE t(id int PRIMARY KEY, val text) USING orioledb;
postgres=# INSERT INTO t VALUES (1, 'serverless');
postgres=# SELECT * FROM t;
```

## Documentation

- [Architecture Details](docs/ORIOLEDB_SERVERLESS.md) — Full technical design
- [PG Patches Inventory](docs/PG_PATCHES_INVENTORY.md) — All PostgreSQL modifications
- [Neon Developer Docs](docs/SUMMARY.md) — Upstream Neon documentation

## Based On

- [Neon](https://github.com/neondatabase/neon) — Serverless PostgreSQL
- [OrioleDB](https://github.com/orioledb/orioledb) — B-tree IOT storage engine for PostgreSQL
- PostgreSQL 17.7

## License

Same as upstream projects. See individual LICENSE files.
