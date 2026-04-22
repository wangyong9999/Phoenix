# G7 — SPLIT + parent downlink atomicity

Plan for closing G7 (documented in `docs/GAPS.md`). Single-session
continuity: whoever picks this up next can `cat` this file and
start implementing without re-deriving the bug or the design.

## Problem (frozen reference)

Mid-CHECKPOINT SIGKILL reproduction: `bash scripts/test_e2e_crash_mid_ckpt.sh`
→ count survives, md5 diverges from pre-crash, and any index-range
scan (`WHERE id BETWEEN x AND y`) PANICs in the post-restart
compute with:

```
PANIC:  error reading downlink 80010000/0 in relfile (5, 16476)
DETAIL: Hikeys don't match.
```

PANIC site: `pgxn/orioledb/src/btree/io.c:1936` (the
post-downlink-load hikey reconciliation).

## Root cause (from session 2026-04-22)

SPLIT in OrioleDB emits **two independent WAL records**:

1. `orioledb_page_wal_split` at `pgxn/orioledb/src/btree/page_wal.c:564`
   → `ORIOLEDB_XLOG_SPLIT` (0x70) with two block FPIs: left (existing
   page being split, post-split content) + right (newly allocated
   page with the right-half items). Emitted from:
   - `split.c:459` (regular non-root split)
   - `insert.c:252` (root split — promotes tree level, old root
     becomes `left_blkno`, the shmem slot at `rootPageBlkno` now
     holds the new-root internal page)
2. `orioledb_page_wal_emit_fpi(..., ORIOLEDB_XLOG_PAGE_IMAGE)` at
   `insert.c:1198` (the "R22 fix") — a separate FPI record for
   the **parent internal page** with the new downlink inserted.

Between record 1 and record 2 there is a crash-exposure window.
Under SIGKILL, PageServer may hold the FPIs of the split (1) but
not the parent's updated downlink (2). On post-restart tree
descent the parent points at the pre-split blkno with the old
hikey expectation, but that blkno now materialises the LEFT half
only → hikey mismatch → PANIC.

Clean-shutdown paths (`cargo neon endpoint stop`) are unaffected:
PG flushes WAL in order before exit, so both records reach
SafeKeeper atomically from the LSN-ordering consumer's view.

## Target design — direction 1: single atomic WAL record

`ORIOLEDB_XLOG_SPLIT` already documents itself as "2-3 FPIs"
(`pgxn/orioledb/include/btree/page_walrecord.h:71`). The wire
format therefore anticipates this; the code path just never emits
the third FPI.

Make the SPLIT WAL record carry **three** block refs for non-root
splits:
- blkref 0: left page (post-split)
- blkref 1: right page (newly allocated, right-half items)
- blkref 2: parent page (with the new downlink inserted)

Root-split keeps the existing two-FPI shape: the "new root" lives
at the rootPageBlkno shmem slot and has no parent to update.

Redo / walingest read up to 3 FPIs per SPLIT record and apply
them in order — the atomic write is the WAL record itself, so any
SIGKILL either sees all three or none.

## Implementation plan

### Part A — OrioleDB C side, emit

File: `pgxn/orioledb/src/btree/page_wal.c`

1. Extend `orioledb_page_wal_split` to accept an optional
   `OInMemoryBlkno parent_blkno` parameter (InvalidBlockNumber
   for root split). When valid, append a third
   `XLogRegisterBlock(2, &rlocator, MAIN_FORKNUM, parent_disk, …)`
   before `XLogInsert(ORIOLEDB_RMGR_ID, ORIOLEDB_XLOG_SPLIT)`.
2. Caller at `split.c:459`: pass the parent blkno from the
   insert machinery's context. Need to trace through
   `perform_page_split` / `o_btree_insert_split_internal` to
   confirm the parent blkno is available at the call site.
3. Caller at `insert.c:252` (root split): pass
   `OInvalidInMemoryBlkno`. The function must skip blkref 2 when
   parent is invalid.
4. **Remove the separate R22 FPI emit at `insert.c:1198`** once
   the parent's FPI rides with SPLIT. Leave the COMPACT emit at
   `insert.c:1255` alone — that's a different path.

### Part B — OrioleDB C side, redo

File: `pgxn/orioledb/src/btree/page_redo.c`

`ORIOLEDB_XLOG_SPLIT` is currently listed as a no-op in the
dispatcher (line 76-88): "FPI-based records ... handled by
XLog machinery before we're called". That remains correct —
PG's `XLogReadBufferForRedo` restores each blkref's FPI
automatically. We just need to ensure the dispatcher doesn't
reject SPLIT records with 3 blkrefs.

No changes expected here, but verify with a walredo-light-mode
test.

### Part C — walingest (Rust)

Find the ORIOLEDB_XLOG_SPLIT (info=0x70) handling in
`libs/wal_decoder/src/` and confirm it iterates all block refs
in the record as Value::Image entries (standard PG FPI handling).
Should not need Rust-side changes if the decoder is generic over
block refs.

Watch for: summary tracking (`orioledb_state.rs`) — SPLIT records
have no CSN body, so next_csn tracking is unaffected. Nothing
to change.

### Part D — Tests

1. Local: `bash scripts/test_e2e_crash_mid_ckpt.sh` must PASS
   (count + md5 match, no PANIC) on ≥10 consecutive runs — the
   race was probabilistic on SIGKILL timing.
2. Local: `test_e2e_crud` 500/5000, `test_e2e_crash_savepoint`,
   `test_e2e_crash_ddl`, `test_e2e_crash_concurrent` must still
   PASS (no regression).
3. CI: flip `test_e2e_crash_mid_ckpt` step from
   `continue-on-error: true` to hard-required in
   `.github/workflows/phoenix-ci.yml`.

### Part E — rollout / forward-compat

The wire format comment already says "2-3 FPIs". Adding a third
blkref to emitted records is **backward compatible** — any
decoder that already iterates blkrefs handles it transparently.
No version bump needed.

**Backward incompat risk**: if a mixed-version fleet exists
where some computes emit 3-FPI SPLIT and others emit 2-FPI
SPLIT, the 2-FPI emitters would still leak the pre-fix race
window on SIGKILL. Mitigation: ensure all computes in a tenant
run the fixed binary before flipping the CI gate.

## Known open questions (resolve while implementing)

1. **Root-split right-half blkno**: `orioledb_page_wal_split(desc,
   rootPageBlkno, left_blkno)` only names two blknos. Where does
   the right half of the promoted root land? Need to read
   `split.c` + `o_btree_finish_root_split_internal` to confirm
   whether a third blkno is already involved and whether it needs
   its own FPI in the SPLIT record.
2. **Parent-blkno availability at `split.c:459`**: the caller's
   insert stack has the parent on its `context->items[]` path
   stack — need to pass it through. Verify locking: is the
   parent still pinned/locked when SPLIT emits? (It must be, for
   us to safely emit its FPI.)
3. **`END_CRIT_SECTION()` ordering at insert.c:1204**: the R22
   FPI removal at :1198 moves responsibility to split-emit. The
   new downlink insert on the parent still happens here — make
   sure it completes *before* the SPLIT XLogInsert, so the FPI
   we emit reflects the post-insert parent state.

## Success criteria

- [ ] `test_e2e_crash_mid_ckpt` passes 10/10 consecutive runs.
- [ ] No regression across the rest of `scripts/test_e2e_*.sh`.
- [ ] Phoenix CI `crash_mid_ckpt` step flipped to hard-required,
      green on the first green Phoenix run after the flip.
- [ ] `docs/GAPS.md` G7 marked CLOSED with the commit hash.

## Code anchors quick reference

| Location | What |
|---|---|
| `page_wal.c:564` | `orioledb_page_wal_split` definition |
| `page_wal.c:630` | `orioledb_page_wal_merge` (for pattern reference) |
| `split.c:459` | Regular SPLIT call site — needs parent_blkno param |
| `insert.c:252` | Root SPLIT call site — passes invalid parent |
| `insert.c:1198` | R22 separate FPI — **DELETE** after fix |
| `io.c:1936` | PANIC site (hikey mismatch reconciliation) |
| `page_walrecord.h:71` | Wire format comment: "2-3 FPIs" |
| `page_redo.c:76-88` | SPLIT redo (expected unchanged) |

## Not in scope for this fix

- MERGE atomicity: similar structural op; out of scope here but
  likely needs parallel treatment. File separate tracking if
  mid-ckpt MERGE crash ever reproduces.
- COMPACT atomicity: emitted at `insert.c:1019` and `1255`.
  Compaction only modifies one page → no parent update race.
- G4 (compressed tables) — orthogonal.
- Phase 4 cleanup — orthogonal.
