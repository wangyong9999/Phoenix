# Q1 — OrioleDB WAL Event Closure Audit

> **Status:** v1.0 — 2026-04-21. N1–N5 audits complete; definitive answer in §6.
> **Covers:** MVP Q1 per `docs/MVP_FIRST_PRINCIPLES.md`: is the set of
> OrioleDB WAL event types closed over every state transition that has
> persistent effect?
> **Does not cover:** purity of each event's redo (that is Q2). This
> doc surfaces coverage gaps, not I2 compliance.
>
> **v1.0 major findings (vs v0.1/v0.2):**
> - Sys-tree writes emit both CONTAINER and LEAF_\* (v0.2 correction
>   stands). Sys-tree pages are pure-function materializable.
> - G3 (root split) is closed: `orioledb_page_wal_split` at
>   `btree/insert.c:252` covers it.
> - G1 (meta-page counters) is confirmed present but reclassified as
>   walingest-summary scope (Q4/E2), not event-schema gap.
> - G2 reclassified to G2': I4 residue, same root cause as G5'.
> - V1–V5 all closed with code evidence.
> - B33 corrected: abort has a dedicated WAL_REC_ROLLBACK record in
>   addition to UNDO_APPLY chain.

---

## 1 — Part A: What's being emitted today (code-verified inventory)

Every direct `XLogInsert(ORIOLEDB_RMGR_ID, ...)` site in the tree:

| # | File:line | Info byte | Emitter trigger |
|---|---|---|---|
| E1 | `recovery/wal.c:980` | `CONTAINER` (0x00) | Catch-all for row-level + sys-tree mutations. See §4. |
| E2 | `utils/o_buffers.c:318` | `PAGE_IMAGE` (0x81) | Plan B mirroring: each undo / xidmap buffer page flush → FPI |
| E3 | `btree/page_wal.c:230` | Variable (`info` arg) | Generic page-FPI helper `orioledb_page_wal_emit_fpi`; see callers in E4 |
| E4a | `btree/page_wal.c:295` | `LEAF_INSERT` (0x20) | Page-level delta for tuple insert |
| E4b | `btree/page_wal.c:337` | `LEAF_DELETE` (0x30) | Page-level delta for tuple mark-deleted |
| E4c | `btree/page_wal.c:381` | `LEAF_UPDATE` (0x40) | Page-level delta for tuple replace |
| E4d | `btree/page_wal.c:473` | `SPLIT` (0x70) | 2× FPI (left, right) for page split |
| E4e | `btree/page_wal.c:509` | `MERGE` (0x80) | FPI for page merge |
| E5 | `checkpoint/checkpoint.c:3034` | `PAGE_IMAGE` (0x81) | Plan E: per-dirty-page FPI during checkpoint |
| E6 | `btree/io.c:1675` | `PAGE_IMAGE` (0x81) | Post-split parent downlink emit (R22 fix) |
| E7 | `checkpoint/control.c:230` | `PAGE_IMAGE` (0x81) | Control file FPI at checkpoint |

Call sites of the generic helper `orioledb_page_wal_emit_fpi(desc, blkno, info)`:

| # | File:line | Info passed | Purpose |
|---|---|---|---|
| E3a | `insert.c:1019` | `COMPACT` | Compaction after finding no room |
| E3b | `insert.c:1255` | `COMPACT` | Compaction in a different insert branch |
| E3c | `insert.c:1198` | `PAGE_IMAGE` | Parent downlink insert after child split |
| E3d | `merge.c:114` | `MERGE` | Parent page (downlink removed) |
| E3e | `merge.c:165` | `MERGE` | Left page (absorbed right's data) |
| E3f | `undo.c:564` | `UNDO_APPLY` | `apply_undo_callback` — undo rollback applied to page |
| E3g | `undo.c:694` | `UNDO_APPLY` | `lock_undo_callback` — undo cleanup for lock |
| E3h | `modify.c:518` | `UNDO_APPLY` | Undo rollback during insert/update |
| E3i | `modify.c:683` | `UNDO_APPLY` | Undo rollback, distinct callsite |

**Events declared but never emitted** (`page_walrecord.h:65-75`):
- `PAGE_INIT` (0x10) — reserved
- `LEAF_LOCK` (0x50) — reserved; row-lock tuphdr mutations currently ride on `LEAF_DELETE` redo
- `ROOT_SPLIT` (0x90) — reserved; current root-split goes through `SPLIT` + meta-page writes which flow through Plan B / Plan E

---

## 2 — Part A': Dual-WAL-path structure (critical discovery)

Tracing the user-INSERT path end to end:

1. `tableam/operations.c:351` (and peers at 234/270/273/941/1294/1321/1398)
   → `o_wal_insert(desc, tuple, ...)` in `recovery/wal.c:987`
   → builds an opaque `OTuple` via `recovery_rec_insert`
   → emits **`ORIOLEDB_XLOG_CONTAINER`** (E1) carrying the row bytes + xid + CSN.
2. Simultaneously, inside the B-tree insert path:
   `btree/insert.c:945` (and peers at 1161/1170)
   → `orioledb_page_wal_leaf_insert(...)` in `page_wal.c:246`
   → emits **`ORIOLEDB_XLOG_LEAF_INSERT`** (E4a) with block ref + tuphdr + tuple bytes.

**Every user row mutation emits two WAL records** (one CONTAINER, one LEAF_*). Today this is:

- CONTAINER: feeds logical-decoding and the compute-side selective replay path (`apply_btree_modify_record`). Opaque to PageServer in the page-keyspace sense.
- LEAF_*: feeds PageServer's per-page materialization (block-keyed in the A-phase routing now in effect).

Same duality exists for LEAF_DELETE (`modify.c:879` + `operations.c` `o_wal_delete_key`) and LEAF_UPDATE (`insert.c:1170` + `operations.c:1294` `o_wal_update`).

**Sys-tree mutations emit the same dual pair, not CONTAINER alone** (v0.2 correction).
Verified via code:

- `btree/page_wal.c:44` `orioledb_page_wal_enabled()` is a global toggle
  (`smgr_hook != NULL && XLogInsertAllowed()`); no sys-tree branch.
- `btree/page_wal.c:185-190` `orioledb_page_wal_rlocator(desc)` maps
  `(desc->oids.datoid, desc->oids.relnode)` uniformly — sys-trees reuse
  the `SYS_TREES_DATOID` bucket but otherwise travel the same helper.
- `catalog/o_sys_cache.c:820-843` writes in this order:
  (1) `o_btree_modify(sys_tree_desc, BTreeOperation*, tuple, ...)` —
  internally hits `btree/insert.c:945` (or `btree/modify.c:879` for
  delete) and calls `orioledb_page_wal_leaf_insert/delete/update`,
  emitting a **LEAF_\*** record against the sys-tree page;
  **then**
  (2) `o_wal_update(sys_tree_desc, ...)` at line 842 — emits a
  **CONTAINER** record with the row payload + sys-tree OIDs.

So sys-tree pages *do* have a pure-function redo path (LEAF_* via
`page_redo.c`), identical in kind to user-tree leaf pages. What
CONTAINER uniquely carries for sys-trees is **the row-level batch that
logical decoding and (currently) the compute-side selective-replay
consumer both read**. Neither of those is on the PageServer
materialization path.

This matters for Q2 (redo purity). v0.1 concluded CONTAINER's
non-purity was a standalone I2 breach for sys-trees. It is not:
sys-tree pages are materializable from LEAF_*. CONTAINER's non-purity
only bites if something on the materialization chain actually replays
CONTAINER — which today is only the `apply_btree_modify_record` path
reachable through `orioledb_recovery.signal`, a known I4 violation
already slated for retirement (see repo-root `CLAUDE.md`). Once that
consumer is gone, CONTAINER has two remaining readers — R14 logical
decoding and whatever walingest-side summary we build for Q4 — neither
requires purity.

---

## 3 — Part B: State transition inventory (what SHOULD be covered)

Persistent OrioleDB state changes that must, per I1, be carried by WAL. Grouped by category:

### 3.1 User data (IOT leaves)

| # | State transition | Currently covered by |
|---|---|---|
| B1 | Insert tuple into primary index leaf | CONTAINER (E1) + LEAF_INSERT (E4a) — dual path |
| B2 | Insert tuple into secondary / bridge / toast index leaf | CONTAINER (E1) + LEAF_INSERT (E4a) — dual path |
| B3 | Mark tuple deleted on leaf (tuphdr update) | CONTAINER (E1) + LEAF_DELETE (E4b) — dual path |
| B4 | Replace tuple on leaf | CONTAINER (E1) + LEAF_UPDATE (E4c) — dual path |
| B5 | Row lock acquired (tuphdr mutation only) | LEAF_DELETE (E4b) reused; LEAF_LOCK reserved not emitted |

### 3.2 IOT structural (B-tree shape)

| # | State transition | Currently covered by |
|---|---|---|
| B6 | Leaf page compaction (reclaim deleted tuples) | COMPACT (E3a/E3b) — FPI |
| B7 | Leaf page split (left + right result) | SPLIT (E4d) — 2× FPI |
| B8 | Internal page split | Same SPLIT event (E4d) |
| B9 | Page merge | MERGE (E4e or E3d/E3e) — FPI |
| B10 | Root split / new level | **No dedicated event** — `ROOT_SPLIT` reserved; currently rides on SPLIT + meta-page writes |
| B11 | Parent downlink insert after child split | PAGE_IMAGE via E3c (R22 fix) |
| B12 | Page init (brand new empty leaf/internal) | **No dedicated event** — `PAGE_INIT` reserved; page creation currently implicit via SPLIT right-page payload |
| B13 | Meta-page atomic counter update (`ctid`, `bridge_ctid`, `numFreeBlocks`, `leafPagesNum`) | **Not separately WAL'd**; captured only at next checkpoint via Plan E FPI of meta page |

### 3.3 Undo log

| # | State transition | Currently covered by |
|---|---|---|
| B14 | New undo record written to undo buffer | Plan B PAGE_IMAGE (E2) — emitted at buffer flush, not per-record |
| B15 | Undo rollback applied to page | UNDO_APPLY (E3f–E3i) — FPI |
| B16 | Undo-location advance per xid (which undo location each xid owns) | CONTAINER via CATALOG_XID_UNDO_LOCATION sys-tree write |
| B17 | Undo chain trim / release post-commit | **Unclear** — need audit of undo.c commit path |

### 3.4 Sys-trees

v0.2 correction: every row below emits LEAF_* **alongside** CONTAINER,
same dual-WAL shape as user-tree rows (§2). LEAF_* is the
materialization source; CONTAINER feeds logical decoding and the
retiring compute-side replay.

| # | State transition | Currently covered by |
|---|---|---|
| B18 | O_TABLES insert/update/delete (DDL) | CONTAINER (E1) + LEAF_* (E4a/b/c) — dual path |
| B19 | O_INDICES insert/update/delete (DDL) | CONTAINER (E1) + LEAF_* (E4a/b/c) — dual path |
| B20 | SHARED_ROOT_INFO update | CONTAINER (E1) + LEAF_* (E4a/b/c) — dual path |
| B21 | CHKP_NUM update | CONTAINER (E1) at `checkpoint.c:1170` + LEAF_* when the sys-tree page is written |
| B22 | CATALOG_XID_UNDO_LOCATION insert/update | CONTAINER (E1) + LEAF_* (E4a/b/c) — dual path |
| B23 | Cache trees (OPCLASS/ENUM/RANGE/CLASS/…) writes | Not persistent — rebuilt from pg_catalog (see N3 audit) |
| B24 | EXTENTS_OFF_LEN / EXTENTS_LEN_OFF updates | Checkpoint-only (N3 audit); page-level via Plan E FPI |

### 3.5 Checkpoint / COW

| # | State transition | Currently covered by |
|---|---|---|
| B25 | Dirty data page persisted at checkpoint | PAGE_IMAGE via Plan E (E5) |
| B26 | Control file update at checkpoint | PAGE_IMAGE (E7) |
| B27 | Map file update at checkpoint | PAGE_IMAGE — need to confirm which emit site |
| B28 | Free-extent recycling (EXTENTS_*) | Checkpoint-only |

### 3.6 Transaction / commit

| # | State transition | Currently covered by |
|---|---|---|
| B29 | OXID allocation (new top-level OrioleDB transaction) | **Not WAL'd by OrioleDB directly** — OXID allocator is a shmem counter; value is implied by subsequent records that reference the OXID |
| B30 | CSN assignment at commit | CONTAINER (E1) — via `report_commit_oxid` path writing to xidmap sys-tree |
| B31 | xidmap update (xid → CSN entry) | CONTAINER (E1) via sys-tree write |
| B32 | Commit barrier flushes (M1.2 undo / M1.3 xidmap) | Flush of records already in 3.3/3.4/3.6; no new event type |
| B33 | Abort / rollback initiation | **WAL_REC_ROLLBACK** finish record (`recovery/wal.c:559-587`, emitted by `wal_rollback` from `undo_xact_callback`) **+** UNDO_APPLY chain (E3f–E3i) from `apply_undo_stack` |

---

## 4 — Part C: CONTAINER deep-dive

`ORIOLEDB_XLOG_CONTAINER` is the catch-all. Its payload (in `recovery/wal.c:909-980`):

```
flags (1 byte)
[if WAL_CONTAINER_HAS_XACT_INFO]  xactTime + xid
[if WAL_CONTAINER_HAS_ORIGIN_INFO] origin_id + origin_lsn
body: serialized OTuple batch from recovery_rec_{insert,update,delete,delete_key}
```

The body is an opaque batch of row records. Each batch may contain mutations targeting multiple sys-trees or user trees. The batch consumer on replay is `apply_btree_modify_record` (`recovery/recovery.c:1858`), which for each entry:

1. Looks up the target `BTreeDescr` by OIDs (requires sys-tree access).
2. Calls `o_btree_modify(tree, BTreeOperationInsert/Update/Delete, tuple, ...)` — the full B-tree mutation path.
3. `o_btree_modify` uses: comparator (tuple descriptor), page pool, undo manager, oxid state.

**I2 assessment (v0.2).** The redo body of CONTAINER — `apply_btree_modify_record → o_btree_modify` — is non-pure: it depends on `BTreeDescr` lookups, comparator, page pool, undo manager, oxid state. This cannot run in walredo light mode.

However, **that non-purity is not an I3 materialization breach**, because CONTAINER is not on the materialization chain:

- User-tree rows materialize via LEAF_* (pure, `page_redo.c`).
- Sys-tree rows *also* materialize via LEAF_* (v0.2 correction, §2).
- Shmem-only scalars (nextOXID / nextCSN / undoLocationMax / running-OXids snapshot / …) materialize via walingest-side summary, not via walredo at all (this is the Q4 mechanism).

The only consumer that actually hits the non-pure `o_btree_modify` path is compute-side selective replay driven by `orioledb_recovery.signal` — flagged in `CLAUDE.md` as an I4 violation scheduled for removal. Once retired, CONTAINER's remaining readers are R14 logical decoding (row-level consumer, not page materialization) and any future walingest-side summary; neither requires purity.

Net: CONTAINER's impurity is an **I4 residue**, not an independent I2 breach. Fixing it is "stop replaying CONTAINER on compute," not "redesign CONTAINER's schema."

**Usage scope.** CONTAINER is emitted from:

- `tableam/operations.c` — every user row mutation (dual with LEAF_*).
- `catalog/o_sys_cache.c:842` — sys cache updates (dual with LEAF_*, §2).
- `btree/modify.c:1542/1593/1595` — secondary index operations that go through the generic `o_wal_{insert,delete,delete_key}` helpers.
- `checkpoint/checkpoint.c:1137/1170` — CHKP_NUM + xid undo-location sys-tree writes during checkpoint.
- Any other sys-tree mutation path reaching `o_btree_modify` on a sys-tree descriptor.

CONTAINER cannot be retired blindly — logical decoding still reads it — but retirement of *its non-pure replay* is free once the signal-based recovery path is removed.

---

## 5 — Part D: Gap register (v1.0 post-audit)

### Gap status after N1–N5

| Ref | Gap | v1.0 status | Resolution path |
|---|---|---|---|
| G1 | Meta-page atomic counters (B13) — `ctid`, `bridge_ctid`, `numFreeBlocks`, `leafPagesNum`, `datafileLength[chkp%2]` — live-updated in shmem without per-increment WAL; captured only via Plan E checkpoint FPI. | **Confirmed** (N3). Five counter families verified shmem-only: `ctid` (`btree.c:269`), `bridge_ctid` (`btree.c:306`), `numFreeBlocks` (`free_extents.c:193, 607`, `descr.c:545`), `leafPagesNum` (`insert.c:264, 754`, `merge.c:193`), `datafileLength` (`free_extents.c:101, 185`, `page_wal.c:427`, `io.c:1275, 1296, 2301, 2309`). | **Not an event-schema gap.** Each counter is walingest-derivable from WAL'd events: `ctid` = `max(LEAF_INSERT.ctid) + 1`; `leafPagesNum` = net SPLIT − MERGE delta over base; `numFreeBlocks` from EXTENTS_\* sys-tree LEAF_\* writes + datafileLength; `datafileLength` = max referenced offset in any emit; `bridge_ctid` mirrors `ctid`. Moves to **Q5/E2 walingest-summary scope** (EXECUTION_PLAN Phase 2.1 B.3/B.4). |
| G2 → **G2'** | Undo record writes — Plan B emits per-page PAGE_IMAGE, not per-undo-record. | **Confirmed per-page** (N4). `write_buffer_data` (`o_buffers.c:253-341`) emits exactly one PAGE_IMAGE per `ORIOLEDB_BLCKSZ` page flush, via `write_undo_range` → `o_buffers_write` at `undo.c:1450`. **BUT** the in-code comment at `o_buffers.c:269-295` is explicit: **per-record truth is in CONTAINER** (`recovery/wal.c:980`), Plan B is a recovery accelerator. | **I4 residue, same as G5'.** Between two Plan B FPIs, per-record undo bytes live in CONTAINER; once compute-side selective replay retires, undo inter-FPI materialization needs a different path (walingest-side summary of undo cursor / or Plan B per-tx flush). **Phase 2.1 B.3/B.4 + Phase 3** scope, not an independent Q1 event gap. |
| ~~G3~~ | Root split has no dedicated event. | **Closed** (N1). `o_btree_finish_root_split_internal` (`btree/insert.c:188-267`) line 252 calls `orioledb_page_wal_split(rootPageBlkno, leftBlkno)` emitting SPLIT (2× FPI: left + new root). ROOT_SPLIT enum (0x90) is declared-but-never-emitted; `page_redo.c:78` handler is dead code. | Nothing to add for correctness. Reserved ROOT_SPLIT slot is Phase 2.3 semantic-granularity improvement candidate. The meta counter increment at `insert.c:264` (leafPagesNum++) folds into G1. |
| G4 | Page init has no dedicated event. | **Partial** (N1). Runtime page births verified covered: `init_new_btree_page` inside `o_btree_finish_root_split_internal` (`insert.c:204, 212`) is inside the SPLIT emit window; non-root leaf split's right-page init happens inside `perform_page_split` under the same SPLIT FPI. Reserved PAGE_INIT enum (0x10) never emitted. | **Residual**: `btree/build.c` initial-load path **not traced**. Bounded impact — CREATE TABLE emits O_TABLES sys-tree write (CONTAINER + LEAF_\*), and the initial empty data tree's root page would be captured by Plan E at worst. **Does not block Phase 2.1 I4-critical path.** Phase 2.2 cleanup audit. |
| G5' | CONTAINER non-pure redo is an I4 leftover, not an independent I2 breach. | Unchanged from v0.2. Sys-tree pages materialize via pure LEAF_\* (`page_redo.c`). CONTAINER's non-pure `apply_btree_modify_record` is reachable only from the `orioledb_recovery.signal` compute-side selective-replay path. | **Phase 3** — retire the signal-based consumer. No CONTAINER schema change. |
| G6 | Dual-WAL redundancy (B1–B4): CONTAINER + LEAF_\* both emit per row mutation. | Unchanged. Design question, not violation. CONTAINER needed for R14 logical decoding; LEAF_\* for PageServer materialization. | Post-MVP; gated on R14 switching source. |

### V1–V5 verification closure

| Ref | Claim | v1.0 status |
|---|---|---|
| V1 | "Meta-page counters captured only at checkpoint" | **✓ Confirmed** (N3). Five counter families shmem-only; captured by Plan E FPI of meta page. |
| V2 | "Root split relies on SPLIT + meta-page" | **✓ Confirmed** (N1). `orioledb_page_wal_split` at `insert.c:252` emits SPLIT; meta counter update at `insert.c:264` is shmem. |
| V3 | "OXID allocation is purely shmem" | **✓ Confirmed** (N2). `get_current_oxid` at `oxid.c:1262` does `pg_atomic_fetch_add_u64(&xid_meta->nextXid, 1)`; `advance_oxids` at `oxid.c:1210-1248` writes `nextXid` and `xidBuffer[].csn/commitPtr` — all shmem, no XLogInsert. |
| V4 | "Abort path covered by UNDO_APPLY chain" | **✓ Confirmed, with correction** (N5). Abort has both: `wal_rollback` emits WAL_REC_ROLLBACK finish record (`wal.c:559-587`, via `add_finish_wal_record`), **and** `apply_undo_stack` walks undo stack emitting UNDO_APPLY FPI chain (E3f–E3i). B33 row updated. |
| V5 | "Plan B buffer flush is per-page, not per-record" | **✓ Confirmed** (N4). `write_buffer_data` (`o_buffers.c:253-341`) emits one PAGE_IMAGE per `ORIOLEDB_BLCKSZ`. |

---

## 6 — Q1 answer (v1.0)

**Is the event set closed over every state transition with persistent effect?**

### 6.1 Page-resident state: YES

Every I3-materializable transition has at least one pure-function WAL carrier:

| Transition class | Carrier |
|---|---|
| User-tree row mutations (B1–B4) | LEAF_INSERT / LEAF_DELETE / LEAF_UPDATE — pure via `page_redo.c` |
| Sys-tree row mutations (B18–B22) | Same LEAF_\* path (§2 correction — v0.2 / v1.0 both confirm) |
| Leaf compaction (B6) | COMPACT (currently FPI, trivially pure) |
| Page split incl. root (B7/B8/B10) | SPLIT via `orioledb_page_wal_split` (2× FPI); root split verified at `insert.c:252` |
| Page merge (B9) | MERGE (FPI) |
| Parent downlink post-split (B11) | PAGE_IMAGE via E3c |
| Page init runtime (B12) | Subsumed in SPLIT right-page payload (initial-build path in G4 residual) |
| Undo rollback apply (B15) | UNDO_APPLY (FPI) |
| Undo-location advance per xid (B16) | CONTAINER + LEAF_\* via CATALOG_XID_UNDO_LOCATION sys-tree write |
| CSN assignment / xidmap update (B30/B31) | CONTAINER + LEAF_\* via xidmap sys-tree write |
| Checkpoint dirty pages (B25) | PAGE_IMAGE via Plan E |
| Abort initiation (B33) | WAL_REC_ROLLBACK (`wal.c:559`) + UNDO_APPLY chain |
| Plan B buffer flush (B14) | PAGE_IMAGE per-page (accelerator; truth in CONTAINER) |

Reserved-but-never-emitted slots (PAGE_INIT 0x10 / LEAF_LOCK 0x50 / ROOT_SPLIT 0x90) are currently subsumed by SPLIT (root and page-init) and LEAF_DELETE (row lock). Activating them is **Phase 2.3 semantic-granularity improvement, not a Phase 2.1 correctness blocker**.

### 6.2 Shmem-only scalar state: not event-schema, by design

The following never had a page representation and **cannot be WAL'd as page events**:

- `nextOXID` (`oxid.c:1262` `xid_meta->nextXid`)
- `nextCSN` (`TRANSAM_VARIABLES->nextCommitSeqNo`)
- `undoLocationMax` (undo stack cursor per `UndoLogType`)
- running-OXids snapshot (MVCC visibility, shmem only)
- `CHKP_NUM` runtime value (also written into sys-tree but shmem cursor is separate)
- Meta-page atomic counters (N3 list, five families)

**Reconstruction mechanism.** All are walingest-derivable from already-WAL'd state. Walingest maintains an OrioleDB-state summary analogous to PG's CheckPoint struct; basebackup delivers it at cold-start. **This is Q5 / E2 summary / E3 delivery scope — not a Q1 event-schema gap.**

### 6.3 CONTAINER's role

Row-level batch source for R14 logical decoding. Emitted dual with LEAF_\* on user-tree and sys-tree row mutations. Its non-pure redo path (`apply_btree_modify_record` → `o_btree_modify`) is **unreachable from the I3 materialization chain** (all materialization goes through pure LEAF_\* / FPI events). The only remaining caller of the non-pure path is compute-side selective replay, already scheduled for retirement in Phase 3. After retirement, CONTAINER has two consumers:

- R14 logical decoding (row-level; no purity requirement).
- Walingest summary ingest for Q5 scalars (non-replay; operates on record payload, not B-tree state).

### 6.4 Residual audits (do not block Phase 2.1)

- **G4 — `btree/build.c` initial-load path.** Bounded impact (new table creation DDL is covered by O_TABLES sys-tree CONTAINER + LEAF_\*). Phase 2.2 cleanup.
- **B27 — map file update emit site** not pinpointed. Phase 2.2 code-read.

### 6.5 Implications for execution plan

- **I4-retirement critical path** (walingest summary + basebackup delivery + retire `orioledb_recovery.signal`) is **not blocked** by any Q1 finding. Required inputs — the Q5 shmem inventory — are enumerated in §6.2.
- **Semantic granularity improvements on E1** (DELTA encoding, Q4 emit-decision, activating reserved PAGE_INIT / ROOT_SPLIT / LEAF_LOCK slots) are additive and orthogonal; no Q1 gap blocks them either.
- **CONTAINER is not retired** in the current plan scope — only its non-pure replay consumer (compute-side selective replay via `orioledb_recovery.signal`) is.
- `docs/EXECUTION_PLAN.md` maps these conclusions onto specific Phase 2 tracks and DoD.

---

## 7 — N1–N5 audit log (completed)

| # | Question | Outcome |
|---|---|---|
| N1 ✓ | Does the root-split path emit any WAL besides SPLIT and meta-page Plan E? | **SPLIT only.** `o_btree_finish_root_split_internal` (`btree/insert.c:188-267`) line 252 calls `orioledb_page_wal_split(rootPageBlkno, leftBlkno)` → 2× FPI. ROOT_SPLIT enum reserved-not-emitted. Meta counter update (`leafPagesNum++` at `insert.c:264`) is shmem-only, folds into G1. → G3 closed. |
| N2 ✓ | Is OXID allocation purely shmem, or does it emit WAL? | **Purely shmem.** `get_current_oxid` at `oxid.c:1262` does `pg_atomic_fetch_add_u64(&xid_meta->nextXid, 1)`; `advance_oxids` at `oxid.c:1210-1248` writes `nextXid` + `xidBuffer[].csn/commitPtr` — all shmem, zero `XLogInsert`. → Walingest-summarizable as `max(OXID in any WAL record) + 1`. V3 closed. |
| N3 ✓ | What meta-page counter updates happen between checkpoints without WAL? | **Five counter families**, all shmem-only: `ctid` (`btree.c:269`), `bridge_ctid` (`btree.c:306`), `numFreeBlocks` (`free_extents.c:193, 607`, `descr.c:545`), `leafPagesNum` (`insert.c:264, 754`, `merge.c:193`), `datafileLength[chkp%2]` (`free_extents.c:101, 185`, `page_wal.c:427`, `io.c:1275, 1296, 2301, 2309`). Captured only via Plan E checkpoint FPI of meta page. → All walingest-derivable (see §6.2). V1 closed; G1 reclassified as Q4/E2 scope. |
| N4 ✓ | Is the Plan B buffer flush truly per-page or per-record? | **Per-page.** `write_buffer_data` (`o_buffers.c:253-341`) emits exactly one PAGE_IMAGE per `ORIOLEDB_BLCKSZ`, called from `write_undo_range` → `o_buffers_write` at `undo.c:1450`. In-code comment (`o_buffers.c:269-295`) explicit: per-record truth is in CONTAINER, Plan B is a recovery accelerator. → V5 closed; G2 reclassified to G2' (I4 residue). |
| N5 ✓ | Does `AbortTransaction` produce any OrioleDB-side WAL beyond UNDO_APPLY? | **Yes.** `undo_xact_callback` (`undo.c:2038-2318`), case `XACT_EVENT_ABORT` (`:2250-2293`) emits: (1) `wal_rollback` → WAL_REC_ROLLBACK finish record (`wal.c:559-587`, `add_finish_wal_record`); (2) `apply_undo_stack` → UNDO_APPLY FPI chain (E3f–E3i); (3) `current_oxid_abort` writes xidmap CSN=aborted (shmem cursor; sys-tree write happens at M1.3 barrier or checkpoint). → V4 closed; B33 row in §3.6 corrected. |

All N1–N5 audits complete; all V1–V5 claims closed with code evidence. See §5 for resulting gap status and §6 for definitive Q1 answer.

---

## 8 — Change log

- **v0.1 (2026-04-20)** — initial draft. Part A inventory verified from code.
  Part B sketched from architectural knowledge. G1–G6 flagged. V1–V5
  marked as unverified. N1–N5 queued as next-step reads.
- **v0.2 (2026-04-21)** — corrected §2: sys-tree writes emit LEAF_*
  **alongside** CONTAINER (evidence: `page_wal.c:44,185-190`,
  `o_sys_cache.c:820-843`, uniform path through `btree/insert.c:945` /
  `btree/modify.c:879`). Updated §3.4 table to reflect dual path for
  B18–B22. Rewrote §4 I2 assessment: CONTAINER non-purity is off the
  materialization chain, i.e. an I4 residue, not an independent I2
  breach. Downgraded G5 → G5' in §5. Rewrote §6: α/β bifurcation
  collapses — α already live for sys-tree pages, β narrowed to
  shmem-only scalars. §7 N1–N5 audits preserved unchanged (they are
  orthogonal to the §2 correction).
- **v1.0 (2026-04-21)** — N1–N5 code audits completed with direct
  file:line evidence (see §7).
  - G3 closed: root split covered by SPLIT via `orioledb_page_wal_split`
    at `btree/insert.c:252`.
  - G1 confirmed present but reclassified as Q4 / E2 walingest-summary
    scope, not event-schema gap. Five counter families enumerated with
    reconstruction rules.
  - G2 reclassified to G2' (I4 residue, same root cause as G5').
  - G4 partial closure: runtime page births covered; `btree/build.c`
    initial-load path remains an audit residual (does not block Phase
    2.1 I4-critical path).
  - V1–V5 all closed with code evidence (§5 table).
  - B33 corrected: abort path emits WAL_REC_ROLLBACK finish record
    (`wal.c:559`) **in addition to** UNDO_APPLY chain.
  - §4 CONTAINER deep-dive updated to reflect v1.0 consumer list
    (R14 logical decoding + walingest summary ingest).
  - §6 rewritten as definitive Q1 answer; §6.5 maps conclusions onto
    `EXECUTION_PLAN.md` Phase 2.1 / 2.3 scope.
  - §7 N1–N5 tables updated with outcomes (was: queued; now: done).
