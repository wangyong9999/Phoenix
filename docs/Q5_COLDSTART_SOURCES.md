# Q5 — Compute Cold-start State Sources

> **Status:** v0.1 — 2026-04-21. Draft.
>
> **Scope:** MVP Q5 per `docs/MVP_FIRST_PRINCIPLES.md §3.5`. Enumerate
> every OrioleDB state item compute must populate at cold-start, map
> each to its source (basebackup / walingest summary / sys-tree GetPage
> / lazy rebuild), produce the walingest summary schema shape required
> by `docs/EXECUTION_PLAN.md` Phase 2.1 tracks B.3 / B.4.
>
> **Depends on:** `docs/INVARIANTS.md` I4 + I5-read,
> `docs/Q1_EVENT_CLOSURE_AUDIT.md` v1.0 §6.2 (shmem scalar list from
> N2/N3 audit), `docs/P1_6_I5_WRITE_AUDIT.md` (A.6 dependency).
>
> **Does not:** prescribe Rust struct bit-layout or serialization
> encoding — those belong to Phase 2.1 B.3 implementation.

---

## 0 — Why Q5 matters

Per I4, compute at cold-start is **stateless**: every byte of state
comes from one of these four sources and **none** from WAL replay:

| Source | Mechanism | Example items |
|---|---|---|
| **basebackup** | `get_basebackup(lsn)` — PageServer bulk snapshot at `sync_lsn` | PG catalog pages, pg_control, OrioleDB shmem summary |
| **PageServer GetPage** | on-demand per-page read with `wait_lsn` | OrioleDB data pages, sys-tree pages |
| **walingest summary** | PageServer-side structure maintained by `walingest.rs` per rmid=129 record | shmem scalars, meta-page counters |
| **deterministic local rebuild** | compute-side cold init from runtime defaults | LRU caches, page pool, backend-local cursors |

Q5's job: for every OrioleDB shmem item, name exactly **one** source.
Items with "unclear source" under current code are the real I4 gaps
and must be resolved in Phase 2.1 design before B.3/B.4 implementation.

---

## 1 — Inventory

### 1.1 Category A — Shmem scalars (walingest summary)

These never had a page representation. They live only in shmem and
cannot be produced by GetPage. walingest maintains them by consuming
every rmid=129 record.

| # | Item | Shmem location | Walingest reconstruction rule | Evidence |
|---|---|---|---|---|
| A.1 | `nextOXID` | `xid_meta->nextXid` (pg_atomic_uint64) | `nextOXID = max(OXID referenced by any ingested record) + 1` | Q1 N2 — `oxid.c:1262`, no XLogInsert; Q1 V3 closed |
| A.2 | `nextCSN` | `TRANSAM_VARIABLES->nextCommitSeqNo` (pg_atomic_uint64) | `nextCSN = max(CSN in any ingested WAL_REC_COMMIT / xidmap LEAF_\*) + 1` | `oxid.c:2217` atomic fetch-add at commit |
| A.3 | `undoLocationMax[UndoLogsCount]` | per-type undo meta (`writtenLocation`, `writeInProgressLocation`) | `undoLocationMax[t] = max(UndoLocation referenced in any ingested record for type t)`, updated on each Plan B PAGE_IMAGE emit and each CONTAINER-carried undo-location advance | `undo.c` writtenLocation writes; driven by Plan B flush |
| A.4 | `runningOXids` (MVCC snapshot basis) | `xid_meta->runXmin` + per-procdata vxids | `runningOXids = {oxid in A.1 range : mapCSN(oxid) in [COMMITTING, IN_PROGRESS_CSN]}`. walingest tracks a compact bitmap / range list. | `oxid.c:1423 advance_run_xmin` |
| A.5 | `CHKP_NUM` runtime cursor | `checkpoint_state->lastCheckpointNumber` (shmem) | Last `CONTAINER` record with info=CHKP_NUM write targeting sys-tree CHKP_NUM; also durably written via Plan B when checkpoint runs | `checkpoint.c:1170` CONTAINER emit |

**All five are Category A** — they cannot be sourced from GetPage
(no page representation) and must be in the walingest summary blob.

### 1.2 Category B — Meta-page atomic counters (walingest summary OR sys-tree page, with caveat)

Per-tree counters that live on each tree's meta page. Between Plan E
checkpoints they are shmem-only (Q1 N3); at checkpoint they are
written into the meta page via FPI.

| # | Item | Per-tree | Walingest reconstruction rule | Evidence |
|---|---|---|---|---|
| B.1 | `metaPage->ctid` | yes | `ctid[tree] = max(LEAF_INSERT tuple.ctid for this tree) + 1` | Q1 N3 — `btree.c:269` |
| B.2 | `metaPage->bridge_ctid` | yes (bridge index only) | `bridge_ctid[tree] = max(bridge INSERT tuple ctid) + 1` | `btree.c:306` |
| B.3 | `metaPage->numFreeBlocks` | yes | `numFreeBlocks[tree] = base@lastCheckpoint - (extents allocated in WAL) + (EXTENTS_\* sys-tree write deltas)` | `free_extents.c:193/607`, `descr.c:545` |
| B.4 | `metaPage->leafPagesNum` | yes | `leafPagesNum[tree] = base@lastCheckpoint + count(SPLIT for tree) - count(MERGE for tree)` | `insert.c:264/754`, `merge.c:193` |
| B.5 | `metaPage->datafileLength[chkp%2]` | yes, per checkpoint slot | `datafileLength[tree][slot] = max(fileExtent.off + len referenced in tree emits during slot window)` | `free_extents.c:101/185`, `page_wal.c:427`, `io.c:1275/1296/2301/2309` |

**Design decision (Q5 resolves).** There are two viable sources for
B.1–B.5:

- **(Source B-A)** walingest summary maintains a per-tree counter
  table, updated per ingested record. Cold-start reads the summary.
- **(Source B-B)** compute reads the meta page via GetPage at
  cold-start, extracts the counter values from the materialized meta
  page. This works **only** if the meta page materialization is kept
  current by Plan E-style FPI emission on each mutation — which it
  currently is NOT (N3 finding: shmem atomic updates do not emit WAL).

**Conclusion: B.1–B.5 go via walingest summary (Source B-A)**. Per-tree
overhead: 5 × u64 ≈ 40 bytes per tree. With typical ~10 trees per
tenant, summary per tenant ~400 bytes for Category B. Negligible.

### 1.3 Category C — Sys-tree content (PageServer GetPage)

Sys-tree pages are materialized by PageServer the same way user-tree
pages are (Q1 v0.2 correction — sys-tree emits LEAF_\*). Cold-start
accesses sys-tree root via the tree's known meta-page-blkno; subsequent
access walks the tree.

| # | Item | Sys-tree | Cold-start access path | I4 |
|---|---|---|---|---|
| C.1 | O_TABLES entries | SYS_TREES_TABLES | via tree descriptor init → first sys-tree read → GetPage | ✅ |
| C.2 | O_INDICES entries | SYS_TREES_INDICES | same | ✅ |
| C.3 | SHARED_ROOT_INFO | SYS_TREES_SHARED_ROOT_INFO | same | ✅ |
| C.4 | `CHKP_NUM` durable value | SYS_TREES_CHKP_NUM | sys-tree GetPage on first access | ✅ (redundant with A.5, cross-check) |
| C.5 | CATALOG_XID_UNDO_LOCATION entries | SYS_TREES_CATALOG_XID_UNDO_LOCATION | GetPage on first xid → undo-loc lookup | ✅ |
| C.6 | xidmap (OXID → CSN) | SYS_TREES_OXIDMAP | GetPage on first visibility check | ✅ |
| C.7 | EXTENTS_OFF_LEN / EXTENTS_LEN_OFF | per-tree sys-trees | GetPage on free-extent allocation | ✅ |

**A.6 dependency:** C.5 and C.6 are the ones whose latest entries are
written via M1.2/M1.3 at commit time. **Without A.6**, the tail xidmap
/ undo-location entries may not reach SafeKeeper → PageServer state
is stale → GetPage returns a stale sys-tree page → visibility bug
(6.6.4c-3 mechanism). **With A.6**, all Category C items are durably
reachable at `sync_lsn`.

C.1–C.4 / C.7 have infrequent writes (DDL, checkpoint) that almost
always predate the most recent commit by a wide margin; they are not
I5-write sensitive.

### 1.4 Category D — PG-side state (basebackup)

Standard Neon mechanism, already working.

| # | Item | Source |
|---|---|---|
| D.1 | PG system catalog pages | basebackup per `pageserver/src/basebackup.rs` |
| D.2 | pg_control | basebackup, synthesized via `generate_pg_control` (`xlog_utils.rs:138-179`) |
| D.3 | neon.signal metadata | basebackup write |
| D.4 | PG shmem counters (nextXid, nextOid, etc.) | from pg_control's CheckPoint struct (walingest-maintained) |
| D.5 | empty WAL segment | basebackup, `generate_wal_segment` (`xlog_utils.rs:432-506`) |

**Note.** The OrioleDB summary blob (Categories A + B) is delivered
**as part of basebackup** — most natural carrier is a separate stream
alongside pg_control. See §2 for the exact shape; details of the
basebackup protocol extension belong to Track C.1 (Phase 2.1).

### 1.5 Category E — Not persistent (lazy rebuild)

Transient runtime state. Cold-start initializes these to defaults.

| # | Item | Cold-start init |
|---|---|---|
| E.1 | Page pool LRU | empty / reset |
| E.2 | Comparator / tuple descriptor caches | empty |
| E.3 | Backend-local `curOxid` | `InvalidOXid` |
| E.4 | Backend-local `logicalXidContext` | empty |
| E.5 | `retained_undo_location` heaps | empty |
| E.6 | `prevLogicalXids` list | empty |
| E.7 | OBuffers in-memory pages | empty (fallback to PageServer Plan B mirror per o_buffers.c `read_buffer_planb_fallback`) |

These satisfy I4 trivially — no persistence needed.

### 1.6 Residual items (unresolved — open audits)

| # | Item | Issue | Next step |
|---|---|---|---|
| R.1 | 2PC prepared transactions | Oriole interaction with PG's `PrepareTransaction` not traced. Prepared xid's OXID, CSN assignment, and undo retention need a separate source decision. | Phase 2.2 audit (post-Phase 2.1 critical path). |
| R.2 | Autonomous transactions (`autonomousNestingLevel`) | Per-procdata shmem; on cold-start the leader backend restarts, autonomous txns were already aborted before crash. | Confirm reset behavior at startup; expect "lazy rebuild" (Category E). |
| R.3 | `set_switch_logical_xid` pairings for SWITCH_LOGICAL_XID records | Logical xid ↔ OXID mapping, WAL-carried. Unclear if walingest must preserve during summary maintenance. | Small audit during B.3 walingest ingest implementation. |
| R.4 | Vacuum / bridge-index runtime state | `max_bridge_ctid_blkno` GUC-configured; at cold-start re-reads GUC. Not persistent. | Category E; no action. |

These do not block Phase 2.1 critical path (B.3/B.4 minimum viable
summary can proceed with Categories A + B). Each will be addressed
before full Track C cutover.

---

## 2 — Walingest summary schema (conceptual)

Categories A and B combined produce the summary's Rust-level shape
(Phase 2.1 B.3 will turn this into concrete struct + serialization).
Conceptually:

```
OrioleDBColdStartSummary {
  // Category A (global)
  next_oxid               : u64
  next_csn                : u64
  undo_location_max       : [u64; N_UNDO_LOG_TYPES]    // N_UNDO_LOG_TYPES = UndoLogsCount
  running_oxids           : CompactSet<u64>             // xmin..nextXid bitmap / range list
  chkp_num                : u32

  // Category B (per-tree)
  per_tree_counters       : Vec<PerTreeCounters>      // keyed by (datoid, relnode)
}

PerTreeCounters {
  tree_id                 : (u32 datoid, u32 relnode)
  ctid                    : u64
  bridge_ctid             : u64                        // Option? or 0 for non-bridge
  num_free_blocks         : u64
  leaf_pages_num          : u32
  datafile_length         : [u64; 2]                   // even/odd checkpoint slots
}
```

**Size estimate.** Global section ~64 bytes. Per-tree ~48 bytes. With
10 sys-trees + e.g. 50 user trees per tenant: ~3 KB summary per
tenant. Negligible vs basebackup total.

**Serialization:** TBD in B.3 — Neon already has infrastructure for
pg_control-like payloads; extending the basebackup stream with a
named blob is the cheapest option.

---

## 3 — Walingest update rules per WAL record type

For each record type in Q1's catalog, what summary fields update:

| rmid=129 record | next_oxid | next_csn | undo_loc_max | running_oxids | chkp_num | per-tree counters |
|---|---|---|---|---|---|---|
| CONTAINER (row-level) | `max(oxid in payload)` | – | (via in-body CATALOG_XID_UNDO_LOCATION) | xid enters running | – | – |
| CONTAINER (CHKP_NUM write) | – | – | – | – | set new value | – |
| CONTAINER (xidmap LEAF_\*) | – | `max(CSN in payload)` | – | remove xid (→ committed) | – | – |
| LEAF_INSERT | – | – | – | – | – | `ctid[tree] = max(old, tuple.ctid+1)` |
| LEAF_DELETE | – | – | – | – | – | – (no counter change) |
| LEAF_UPDATE | – | – | – | – | – | – |
| SPLIT | – | – | – | – | – | `leaf_pages_num[tree] += 1` |
| MERGE | – | – | – | – | – | `leaf_pages_num[tree] -= 1` |
| COMPACT | – | – | – | – | – | – |
| PAGE_IMAGE (Plan B — undo tag) | – | – | `undo_loc_max[type] = max(old, block_num × ORIOLEDB_BLCKSZ + size)` | – | – | – |
| PAGE_IMAGE (Plan B — xidmap tag) | – | – | – | – | – | – (row-level CSN extraction via body) |
| PAGE_IMAGE (Plan E — meta page) | – | – | – | – | – | **re-anchor** per-tree counters to page contents (checkpoint baseline) |
| PAGE_IMAGE (Plan E — data page) | – | – | – | – | – | `datafile_length[tree][slot] = max(old, page.extent.off + len)` |
| PAGE_IMAGE (R22 — parent downlink) | – | – | – | – | – | – |
| UNDO_APPLY | – | – | – | – | – | – |
| WAL_REC_COMMIT (in CONTAINER finish) | – | `CSN in record` | – | `remove(xid)` | – | – |
| WAL_REC_ROLLBACK | – | – | – | `remove(xid)` (aborted path) | – | – |

**Base anchor at Plan E meta-page FPI**: when walingest ingests a
PAGE_IMAGE on a tree's meta page, it replaces the per-tree counters
with values decoded from the meta page contents. This gives a
periodic re-anchor that lets summary skip all prior deltas — the
"base" of the counter chain.

**Open detail (for B.3 implementation).** Exact byte offsets within
PAGE_IMAGE records to extract tuple.ctid, CSN, meta-page counters are
payload-format-dependent. This doc gives the semantic rules; the
implementation reads `libs/wal_decoder` existing payload parsers.

---

## 4 — Cold-start protocol (compute side)

Sequence from `kill -9 compute && restart`:

```
1. Compute starts, runs standard PG startup flow.
2. PG reads pg_control (from basebackup) → sees state=DB_SHUTDOWNED,
   redo_lsn=sync_lsn → skips crash recovery (already verified behavior,
   xlog_utils.rs:138-179).
3. Neon-specific init loads OrioleDBColdStartSummary from basebackup.
4. OrioleDB shmem initialized from summary:
     - xid_meta->nextXid = summary.next_oxid
     - TRANSAM_VARIABLES->nextCommitSeqNo = summary.next_csn
     - undoMeta[t]->writtenLocation / writeInProgressLocation = from summary.undo_location_max[t]
     - runningOXids rebuilt from summary.running_oxids
     - checkpoint_state->lastCheckpointNumber = summary.chkp_num
     - for each tree in summary.per_tree_counters:
         - BTree metaPage->ctid = counters.ctid
         - ... (etc.)
5. OrioleDB ready to accept queries.
6. First access to a sys-tree page triggers GetPage → PageServer
   walredo delta chain → returns current page.
7. First access to a user data page similar.
8. Plan B OBuffers read path uses read_buffer_planb_fallback on
   local-cache miss.

NO rmid=129 replay. NO orioledb_recovery.signal. NO pg_wal copy.
```

This is the I4-compliant cold-start. Track C (Phase 2.1) implements
steps 3–5 on the compute side; Track B (Phase 2.1) implements
walingest summary maintenance + basebackup delivery.

---

## 5 — Dependencies

### 5.1 A.6 dependency (Phase 2.1 prerequisite)

Summary completeness at `sync_lsn` requires M1.2/M1.3 records to have
reached SafeKeeper. Without A.6 fix, tail Category C items (C.5, C.6)
may be stale in PageServer, and Category A.2 (next_csn) / A.3
(undo_location_max) may trail reality. See `docs/P1_6_I5_WRITE_AUDIT.md`.

**Q5 v0.1 is written assuming A.6 is in place.** Without A.6, the
summary is usable but readers must accept bounded staleness tolerance
(tail commits invisible — the very bug we're fixing).

### 5.2 Q2 / Q3 / Q4 dependencies (Phase 2.3 prerequisites)

Q5 does NOT require Q2/Q3/Q4 to be answered. Category C items are
materialized via existing event types (LEAF_*, SPLIT, etc.) whose
current redo works. Phase 2.3 improvements (DELTA encoding, emit
decision S1) refine Q3 chain length but do not affect Q5's source
inventory.

### 5.3 Layer compaction (Phase 2.2 verification)

For Category C sys-tree pages to be efficient at first access, the
delta chain between a base image and `sync_lsn` must be bounded. This
is Q3 territory. Empirical verification is Track B.5 / F.1 (Phase
2.2). Q5 assumes they resolve OK; if they don't, cold-start latency
on first-sys-tree-access may exceed `wait_lsn` 60s ceiling (Phase 2.2
risk R5 in `EXECUTION_PLAN.md`).

---

## 6 — How Q5 outputs feed Phase 2.1

- **B.3 (walingest summary structure)** — Rust struct mirrors §2
  schema; update logic implements §3 rules.
- **B.4 (summary field coverage)** — DoD = every Category A + B item
  in §1.1 / §1.2 is updated by the walingest ingest loop.
- **C.1 (basebackup summary carrier)** — basebackup stream gets a
  named blob `orioledb.state` or `neon.orioledb_cold_summary`;
  serialization format TBD.
- **C.2 (basebackup generation)** — `pageserver/src/basebackup*`
  reads the walingest-maintained summary, serializes, ships.
- **C.3 (compute init)** — cold-start per §4; shmem init branch
  gated on presence of the summary blob.
- **Feature flag** (C.4) — until summary blob is reliably present,
  fall back to `orioledb_recovery.signal` path (do not remove until
  Phase 3).

---

## 7 — Residuals and open items

Copied forward from §1.6 for visibility:

- R.1 2PC prepared transactions — Phase 2.2 audit.
- R.2 Autonomous transactions — confirm lazy rebuild is enough.
- R.3 SWITCH_LOGICAL_XID mapping — small audit at B.3 implementation.
- R.4 Vacuum/bridge runtime state — Category E, no action.

None of these block Phase 2.1 B.3/B.4 minimum viable.

---

## 8 — Change log

- **v0.1 (2026-04-21)** — initial inventory. Categories A–E + residual
  R.1–R.4. Walingest summary schema sketch. Dependency on A.6 (P1.6
  audit) and on Phase 2.3 Qs (independence). Feeds Phase 2.1 B.3/B.4/C
  tracks in EXECUTION_PLAN.md.
