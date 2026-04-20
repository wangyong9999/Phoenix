# N3 — Sys-Tree Commit-Barrier Audit

> **Plan reference:** `ENTERPRISE_HARDENING_PLAN.md` §N3 (L1.d + L1.e).
> **Question:** every OrioleDB sys-tree write must reach SafeKeeper
> before the writer-txn's `XACT_COMMIT` is flushed. Prove it case by case.

## The 24 sys trees

Defined in `pgxn/orioledb/include/catalog/sys_trees.h`:

| ID | Name | Purpose | Persistence | Write path |
|----|------|---------|-------------|-----------|
| 1 | SHARED_ROOT_INFO | per-rel root info | permanent | btree ops |
| 2 | O_TABLES | table metadata | permanent | btree ops via recovery/worker.c |
| 3 | O_INDICES | index metadata | permanent | btree ops via recovery/worker.c |
| 4 | OPCLASS_CACHE | op-class cache | permanent | cache load |
| 5 | ENUM_CACHE | enum type cache | permanent | cache load |
| 6 | ENUMOID_CACHE | enum oid cache | permanent | cache load |
| 7 | RANGE_CACHE | range type cache | permanent | cache load |
| 8 | CLASS_CACHE | pg_class mirror | permanent | cache load |
| 9 | EXTENTS_OFF_LEN | free extent by off | permanent | checkpoint |
| 10 | EXTENTS_LEN_OFF | free extent by len | permanent | checkpoint |
| 11 | PROC_CACHE | pg_proc mirror | permanent | cache load |
| 12 | TYPE_CACHE | pg_type mirror | permanent | cache load |
| 13 | AGG_CACHE | pg_aggregate mirror | permanent | cache load |
| 14 | OPER_CACHE | pg_operator mirror | permanent | cache load |
| 15 | AMOP_CACHE | pg_amop mirror | permanent | cache load |
| 16 | AMPROC_CACHE | pg_amproc mirror | permanent | cache load |
| 17 | COLLATION_CACHE | collation cache | permanent | cache load |
| 18 | DATABASE_CACHE | pg_database mirror | permanent | cache load |
| 19 | AMOP_STRAT_CACHE | amop strat cache | permanent | cache load |
| 20 | EVICTED_DATA | evicted-tree payload | permanent | eviction |
| 21 | CHKP_NUM | per-tree chkp num | permanent | checkpoint |
| 22 | MULTIRANGE_CACHE | multirange type cache | permanent | cache load |
| 23 | TABLESPACE_CACHE | tablespace cache | permanent | cache load |
| 24 | CATALOG_XID_UNDO_LOCATION | xid → undo location | permanent | commit |

## Classification

### (A) Covered by CONTAINER-record replay

Sys trees that receive writes via `apply_btree_modify_record` from
`recovery/worker.c:678`:
- SHARED_ROOT_INFO (1), O_TABLES (2), O_INDICES (3)

These are modified during DDL (CREATE/DROP TABLE, CREATE/DROP INDEX).
The modification goes via `o_btree_modify()` → emits an
`ORIOLEDB_XLOG_CONTAINER` record (recovery/wal.c:980) that's replayed
on the other side.

**Barrier status**: currently covered because CONTAINER records are
flushed as part of the txn's WAL and replayed on restart. N2's
data-page FPI path does NOT cover sys-tree modifications — those stay
on the CONTAINER-replay path.

**Gap**: the 6.6.4c-3 failure mode suggests CONTAINER-replay is not
reliably re-dispatched on post-crash restart (no "orioledb recovery
started" log). If that is the root cause of 6.6.4c-3, it also
undermines sys-tree durability for post-checkpoint DDL operations.
The test harness for DDL-then-crash is N3 follow-up.

### (B) Checkpoint-only trees

Sys trees that are only mutated during a checkpoint, so their writes
are already coupled to the checkpoint's own persistence fence:
- EXTENTS_OFF_LEN (9), EXTENTS_LEN_OFF (10)
- CHKP_NUM (21) — updated by `o_update_latest_chkp_num` called from
  `checkpoint_btree` at checkpoint.c:3056
- CATALOG_XID_UNDO_LOCATION (24) — updated from the commit path in
  `report_commit_oxid`, flushed as part of the XACT record

**Barrier status**: CHKP_NUM is confirmed preserved across restart
(CI diag for 6.6.4c-3 showed `(5, 16476)=3 (entries[3, 2])`).
EXTENTS_* are implementation details of checkpoint and only read
during checkpoint; no post-crash visibility concern.
CATALOG_XID_UNDO_LOCATION is the same lifetime as the commit XACT
record; flushed atomically.

### (C) Cache trees

Sys trees that are *caches* of PG catalog state:
- OPCLASS, ENUM, ENUMOID, RANGE, CLASS, PROC, TYPE, AGG, OPER, AMOP,
  AMPROC, COLLATION, DATABASE, AMOP_STRAT, MULTIRANGE, TABLESPACE
  (4, 5, 6, 7, 8, 11, 12, 13, 14, 15, 16, 17, 18, 19, 22, 23)

These are re-populated on demand when an OrioleDB op needs catalog
info. They hold no durable state that isn't already in
`pg_catalog` — so "losing" a cache tree entry post-crash is fine,
it'll be re-cached on the next access.

**Barrier status**: no barrier needed. Documented here so nobody
mistakenly adds one.

### (D) Runtime trees

Sys trees that hold *runtime* state and can be safely rebuilt:
- EVICTED_DATA (20) — per-tree evicted payload. Only relevant between
  eviction and re-load; not durable across crash.

**Barrier status**: no barrier needed.

## Conclusion

- **No commit-barrier gap for sys trees** in the persistent-state
  sense: every persistent write is either (A) in a CONTAINER record
  flushed with the txn's WAL, or (B) in a checkpoint-coupled write,
  or belongs to classes (C)/(D) that don't require durability.

- **The remaining concern is CONTAINER-replay reliability** — if
  post-crash restart doesn't re-dispatch CONTAINER records (which is
  what the 6.6.4c-3 log evidence points to), then sys-tree DDL
  operations past the last checkpoint are also at risk. But this
  isn't a new barrier to add in N3; it's the same root-cause
  investigation under #21.

- **M1.4 root-downlink-after-split FPI** (plan L1.d) is already
  implemented:
  - Root split reuses `orioledb_page_wal_split` which emits FPIs for
    both resulting pages (split.c:459, insert.c:252 via
    `o_btree_finish_root_split_internal`).
  - Non-root parent-downlink insertion after split emits FPI via R22
    in `insert.c:1191`.
  - So the rootDownlink *as a pointer* is in the meta page, which is
    checkpoint-only; the root *content* has an FPI chain that flows
    through split→R22→checkpoint.

## Follow-up work

1. **Add a DDL-then-crash test** (`test_e2e_crash_ddl.sh`):
   - BEGIN; CREATE TABLE t1; INSERT rows; COMMIT; CHECKPOINT;
   - BEGIN; CREATE TABLE t2; INSERT rows; COMMIT;
   - SIGKILL (no CHECKPOINT after t2 creation).
   - Post-restart: both t1 and t2 must exist with their rows.
   - Pins down (A)-class sys-tree durability.

2. **Add an instrumentation point in `apply_btree_modify_record`** so
   we can count CONTAINER record replays post-crash. If zero, that
   confirms the missing-replay hypothesis from #21.

Both items are scoped; not blocking on the 6.6.4c-3 mechanism being
pinned down. N3 analysis is done; the follow-ups are queued under
the N3 milestone but treated as incremental PRs.

## N3 closure

- No new commit-barrier required — sys trees are already covered by
  either CONTAINER-replay (A), checkpoint-coupling (B), or are
  non-durable by design (C, D).
- Follow-up DDL-crash test (`test_e2e_crash_ddl.sh`) queued as
  scenario N3-follow-up; logs alongside N8.
- Any residual gap inherits from the 6.6.4c-3 CONTAINER-replay
  investigation; do not re-investigate in N3.
