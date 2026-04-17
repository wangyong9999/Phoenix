# OrioleDB on Neon: Enterprise Readiness Roadmap

> **Status:** POC validated — stateless restart works (100 rows recovered)
> **Created:** 2026-04-16
> **Baseline:** INSERT → CHECKPOINT → STOP → RESTART → SELECT = 100 rows ✅

## Phase 6: Delta WAL Recovery (CRITICAL)

**Goal:** Verify data recovery for non-checkpointed modifications

**Test:** INSERT → CHECKPOINT → INSERT more → STOP (no 2nd CHECKPOINT) → RESTART → SELECT

This exercises the complete page-level delta WAL → wal-redo pipeline:
  PageServer finds: base image (checkpoint FPI) + delta (LEAF_INSERT WAL)
  → sends both to wal-redo process
  → orioledb_page_redo applies delta to base image
  → returns correct page with both checkpoint and post-checkpoint data

**If fails:** Fall back to FPI-only mode (change delta to FPI for all ops)

## Phase 7: Full CRUD + Structural Operations

**Tests:**
- UPDATE 50 rows → CHECKPOINT → STOP → RESTART → verify updates present
- DELETE 50 rows → CHECKPOINT → STOP → RESTART → verify deletions
- INSERT 5000 rows (trigger multiple SPLITs) → CHECKPOINT → RESTART → verify all
- Mixed operations → RESTART → verify

## Phase 8: Branching

**Test:** INSERT data → create timeline branch → SELECT on branch
- Verifies OrioleDB page-level WAL supports Neon's branching
- Tests ancestor timeline page resolution for OrioleDB synthetic relations

## Phase 9: Stress & Reliability

**Tests:**
- pgbench with OrioleDB tables (1000 TPS for 60s → restart → verify)
- WAL volume measurement (delta vs FPI overhead)
- Concurrent INSERT from multiple backends → crash → restart

## Phase 10: CSN Improvement

- Verify xidBuffer checkpoint flush covers all committed CSN
- Test transaction visibility after restart (CSN-based MVCC)
- Evaluate: commit-time flush vs checkpoint-only flush

## Priority Order

1. Phase 6 (delta WAL) — blocks everything else
2. Phase 7 (CRUD) — confidence in basic operations
3. Phase 8 (branching) — Neon's value proposition
4. Phase 9 (stress) — production readiness
5. Phase 10 (CSN) — completeness
