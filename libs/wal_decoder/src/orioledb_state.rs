//! OrioleDB cold-start summary (walingest-maintained).
//!
//! Phase 2.1 B.3 — infrastructure for the walingest-side state blob
//! that compute reads at cold-start to initialize OrioleDB shmem
//! without replaying WAL. Alone this module does not achieve I4
//! (`docs/INVARIANTS.md §4`) — basebackup delivery (C.1/C.2) and
//! compute init codepath (C.3) close the loop.
//!
//! # Invariant linkage
//!
//! - **I4** enabler: summary records state that compute otherwise
//!   would rebuild via WAL replay. Walingest derives this summary
//!   from the already-ingested rmid=129 stream.
//! - **I1**: summary is derivable, not a new persistence source.
//!   Pageserver restart re-derives by re-ingesting from the most
//!   recent checkpoint; no separate persistence needed at this
//!   layer.
//! - **I2/I3/I5**: orthogonal — summary does not emit records, does
//!   not enter `(rel, blkno)` keyspace, does not participate in
//!   per-record transaction atomicity.
//!
//! # Scope
//!
//! **v0.2** (current): CONTAINER header decode + first-sub-record
//! body decode extracting `OXid` from `WAL_REC_XID`. Summary tracks
//! `next_oxid` (monotonic max of seen `oxid + 1`). Still does not
//! decode `WAL_REC_COMMIT` / `WAL_REC_ROLLBACK` / `WAL_REC_JOINT_COMMIT`
//! for CSN, nor deeper sub-record traversal — those are v0.3+/B.4.
//!
//! # References
//!
//! - `docs/Q5_COLDSTART_SOURCES.md §2` — summary schema sketch.
//! - `docs/Q5_COLDSTART_SOURCES.md §3` — per-record update rules.
//! - `pgxn/orioledb/include/recovery/wal.h` — CONTAINER header +
//!   sub-record layouts.
//! - `pgxn/orioledb/src/recovery/wal.c:936-979` — header emit code.

use serde::{Deserialize, Serialize};

// --- CONTAINER header wire format -------------------------------------------

/// `WAL_CONTAINER_HAS_XACT_INFO` flag bit.
/// See `pgxn/orioledb/include/recovery/wal.h:239`.
pub const WAL_CONTAINER_HAS_XACT_INFO: u8 = 1 << 0;

/// `WAL_CONTAINER_HAS_ORIGIN_INFO` flag bit.
/// See `pgxn/orioledb/include/recovery/wal.h:240`.
pub const WAL_CONTAINER_HAS_ORIGIN_INFO: u8 = 1 << 1;

/// Size of the CONTAINER header prefix that is always present:
/// `uint16 wal_version + uint8 flags = 3 bytes`.
const CONTAINER_HEADER_MIN_LEN: usize = 3;

/// Size of `WALRecXactInfo` — 8 bytes xactTime + 4 bytes xid.
/// See `pgxn/orioledb/include/recovery/wal.h:253-257`.
const WAL_REC_XACT_INFO_LEN: usize = 12;

/// Size of `WALRecOriginInfo` — 2 bytes origin_id + 8 bytes origin_lsn.
/// See `pgxn/orioledb/include/recovery/wal.h:259-263`.
const WAL_REC_ORIGIN_INFO_LEN: usize = 10;

// --- Sub-record wire format -------------------------------------------------

/// `OrioleWalRecType` enumeration from
/// `pgxn/orioledb/include/recovery/wal.h:34-52`.
pub mod wal_rec_type {
    pub const NONE: u8 = 0;
    pub const XID: u8 = 1;
    pub const COMMIT: u8 = 2;
    pub const ROLLBACK: u8 = 3;
    pub const RELATION: u8 = 4;
    pub const INSERT: u8 = 5;
    pub const UPDATE: u8 = 6;
    pub const DELETE: u8 = 7;
    pub const O_TABLES_META_LOCK: u8 = 8;
    pub const O_TABLES_META_UNLOCK: u8 = 9;
    pub const SAVEPOINT: u8 = 10;
    pub const ROLLBACK_TO_SAVEPOINT: u8 = 11;
    pub const JOINT_COMMIT: u8 = 12;
    pub const TRUNCATE: u8 = 13;
    pub const BRIDGE_ERASE: u8 = 14;
    pub const REINSERT: u8 = 15;
    pub const REPLAY_FEEDBACK: u8 = 16;
    pub const SWITCH_LOGICAL_XID: u8 = 17;
    pub const RELREPLIDENT: u8 = 18;
}

/// Sub-record sizes — see `pgxn/orioledb/include/recovery/wal.h`.
///
/// `WALRecXid`        : 1 recType + 8 oxid + 4 logicalXid + 4 heapXid = 17
/// `WALRecFinish`     : 1 recType + 8 xmin + 8 csn = 17 (used for COMMIT / ROLLBACK)
/// `WALRecJointCommit`: 1 recType + 4 xid + 8 xmin + 8 csn = 21
const WAL_REC_XID_LEN: usize = 17;
const WAL_REC_FINISH_LEN: usize = 17;
const WAL_REC_JOINT_COMMIT_LEN: usize = 21;

/// Offset of the `oxid` field inside `WALRecXid` — immediately after
/// `recType` (1 byte).
const WAL_REC_XID_OXID_OFFSET: usize = 1;

/// Offset of the `csn` field inside `WALRecFinish` — after recType (1)
/// + xmin (8) = 9.
const WAL_REC_FINISH_CSN_OFFSET: usize = 9;

/// Offset of the `csn` field inside `WALRecJointCommit` — after
/// recType (1) + xid (4) + xmin (8) = 13.
const WAL_REC_JOINT_COMMIT_CSN_OFFSET: usize = 13;

// --- Types ------------------------------------------------------------------

/// Parsed header of an `ORIOLEDB_XLOG_CONTAINER` (info=0x00) record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContainerHeader {
    pub wal_version: u16,
    pub flags: u8,
    /// PG-layer TransactionId from the `xact_info` sub-structure, if
    /// the `HAS_XACT_INFO` flag is set. `None` otherwise.
    pub pg_xid: Option<u32>,
    /// Offset into the original payload where the OrioleDB body
    /// sub-records begin.
    pub body_offset: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerParseError {
    TooShort,
}

/// Per-record extraction result produced by the decoder and consumed
/// by the walingest-side summary updater. This is what flows through
/// `MetadataRecord::OrioleDb(…)` from `wal_decoder::decoder` to
/// `pageserver::walingest`.
///
/// `None` fields indicate "this record did not carry that piece of
/// information" — the summary leaves the corresponding field
/// unchanged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrioleDbRecordDelta {
    /// PG TransactionId from the CONTAINER `xact_info` header.
    pub pg_xid: Option<u32>,
    /// Largest OrioleDB OXid seen in a `WAL_REC_XID` sub-record while
    /// scanning the container body.
    pub max_oxid_in_body: Option<u64>,
    /// Largest `CommitSeqNo` seen in a `WAL_REC_COMMIT` or
    /// `WAL_REC_JOINT_COMMIT` sub-record. `WAL_REC_ROLLBACK` does not
    /// bump next_csn (its CSN is `COMMITSEQNO_ABORTED`).
    pub max_csn_in_body: Option<u64>,
}

/// Per-tree atomic-counter snapshot reconstructed by walingest from
/// the rmid=129 stream. Compute reads these at cold-start to seed
/// `metaPage` fields for each tree (B.1–B.5 + T7 in Q5 vocabulary).
///
/// `tree_id` is the 64-bit hash of `(datoid, relnode, tree_type)`.
/// Collision probability per tenant is negligible (< 2^-50 with
/// ~10 trees per tenant).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerTreeCounters {
    pub tree_id: u64,
    /// `metaPage->ctid` — next ctid to allocate. T9a B.1.
    pub ctid: u64,
    /// `metaPage->bridge_ctid` — bridge index ctid. T9a B.2.
    pub bridge_ctid: u64,
    /// `metaPage->numFreeBlocks` — free-extent counter. T9a B.3.
    pub num_free_blocks: u64,
    /// `metaPage->leafPagesNum` — count of leaf pages. T9a B.4.
    pub leaf_pages_num: u64,
    /// `metaPage->datafileLength[chkp%2]` — per-checkpoint slot
    /// extent watermark. T9a B.5.
    pub datafile_length: [u64; 2],
    /// Per-tree max `undoLocation` seen across UNDO_APPLY and
    /// LEAF_INSERT/UPDATE/DELETE records that emit undo. T7.
    pub undo_location: u64,
}

/// SPLIT record awaiting a matching `SPLIT_FINALIZE`. If walingest
/// finishes ingesting up to the cold-start LSN with this entry still
/// present, compute synthesizes the parent downlink update at apply
/// time. Closes G7 without lock-holding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSplit {
    pub tree_id: u64,
    pub left_blkno: u32,
    pub right_blkno: u32,
    /// LSN of the originating `ORIOLEDB_XLOG_SPLIT` record so the
    /// pair can be matched and the child hikey re-fetched.
    pub child_hikey_lsn: u64,
    pub child_hikey_offset: u32,
    pub _reserved: u32,
}

/// MERGE record awaiting a matching `MERGE_FINALIZE`. Same shape /
/// purpose as `PendingSplit`, applied to the delete path. Closes G8.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingMerge {
    pub tree_id: u64,
    pub left_blkno: u32,
    pub parent_blkno: u32,
    pub merge_lsn: u64,
}

/// Per-timeline OrioleDB cold-start summary.
///
/// Fields are intentionally minimal; additions require a
/// walingest-derivation proof per Q5 §3.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrioleDBColdStartSummary {
    /// Number of rmid=129 records ingested into this summary.
    /// Monotonic; reset only when the summary is discarded.
    pub ingested_count: u64,

    /// Raw XLogRecPtr of the most recently ingested record. Stored
    /// as u64; conversion to `utils::lsn::Lsn` happens at callsite
    /// boundaries.
    pub last_ingested_lsn_raw: u64,

    /// Last PG TransactionId seen in a CONTAINER `xact_info` header.
    /// `0` means none seen yet.
    pub last_pg_xid_seen: u32,

    /// Next OrioleDB OXid to allocate — one past the maximum OXid
    /// observed in any ingested record. `0` until the first
    /// `WAL_REC_XID` sub-record is seen. Compute reads this on
    /// cold-start to seed `xid_meta->nextXid`; see Q5 §2 and
    /// `pgxn/orioledb/src/transam/oxid.c:1262`.
    pub next_oxid: u64,

    /// Next `CommitSeqNo` to allocate — one past the maximum CSN
    /// observed in any `WAL_REC_COMMIT` / `WAL_REC_JOINT_COMMIT`
    /// sub-record. `0` until the first commit record is seen.
    /// Compute reads this to bump `startupCommitSeqNo` before PG's
    /// StartupXLOG writes it into `TransamVariables->nextCommitSeqNo`
    /// (see `vendor/postgres-v17/src/backend/access/transam/xlog.c:5669`).
    /// Added in packed format version 2.
    pub next_csn: u64,

    /// Per-tree atomic counters (T7 + T9a B.1–B.5). Indexed by
    /// `tree_id`; sparsely populated (only trees seen since the
    /// most recent checkpoint appear here). Added in packed format
    /// version 3.
    pub per_tree: Vec<PerTreeCounters>,

    /// SPLIT records awaiting matching `SPLIT_FINALIZE`. Compute
    /// synthesizes parent state from these at apply time. G7 closure.
    /// Added in packed format version 3.
    pub pending_splits: Vec<PendingSplit>,

    /// MERGE records awaiting matching `MERGE_FINALIZE`. G8 closure.
    /// Added in packed format version 3.
    pub pending_merges: Vec<PendingMerge>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestError {
    Parse(ContainerParseError),
    NonMonotonicLsn {
        previous: u64,
        attempted: u64,
    },
}

// --- Packed wire format (C-readable) ----------------------------------------

/// Magic identifying the OrioleDB cold-start summary blob on disk.
/// ASCII "OROS" (Oriole State) in little-endian.
pub const ORIOLEDB_STATE_MAGIC: u32 = 0x534F524F;

/// Wire-format version. Bump when fields or layout change; the C-side
/// reader must be updated accordingly.
///
/// History:
/// - v1 (40 bytes): next_oxid, last_pg_xid_seen, last_ingested_lsn_raw,
///   ingested_count. (Superseded — never shipped to production.)
/// - v2 (48 bytes): v1 + next_csn. Superseded.
/// - v3 (variable): v2 + per-tree counters (ctid / bridge_ctid /
///   numFreeBlocks / leafPagesNum / datafileLength[2] / undo_location)
///   + pending_splits[] + pending_merges[] for SPLIT/MERGE
///   reconciliation. See `docs/WALINGEST_SUMMARY_V3.md`. Current.
pub const ORIOLEDB_STATE_VERSION: u32 = 3;

/// Size of the v2 fixed prefix (kept for back-compat decoding of
/// older summary blobs). `ORIOLEDB_STATE_V3_HEADER_SIZE` adds a
/// 16-byte counts header on top of this.
pub const ORIOLEDB_STATE_V2_FIXED_SIZE: usize = 48;

/// Size of v3's fixed header — v2 fixed prefix (48) + 16-byte counts
/// header (`tree_count`, `pending_split_count`, `pending_merge_count`,
/// `_reserved`). Variable-length per-tree / pending-pool sections
/// follow this header.
pub const ORIOLEDB_STATE_V3_HEADER_SIZE: usize = 64;

/// Size of one `PerTreeCounters` slot in the variable section.
/// 8 (tree_id) + 8 (ctid) + 8 (bridge_ctid) + 8 (num_free_blocks)
/// + 8 (leaf_pages_num) + 16 (datafile_length[2]) + 8 (undo_location).
pub const PER_TREE_COUNTERS_SIZE: usize = 64;

/// Size of one `PendingSplit` slot. 8 (tree_id) + 4 (left_blkno)
/// + 4 (right_blkno) + 8 (child_hikey_lsn) + 4 (child_hikey_offset)
/// + 4 (_reserved).
pub const PENDING_SPLIT_SIZE: usize = 32;

/// Size of one `PendingMerge` slot. 8 (tree_id) + 4 (left_blkno)
/// + 4 (parent_blkno) + 8 (merge_lsn).
pub const PENDING_MERGE_SIZE: usize = 24;

/// Sanity bound on dynamic-section element counts (per-tree,
/// pending_splits, pending_merges). Prevents pathological summary
/// blobs from exhausting memory at decode time.
pub const ORIOLEDB_STATE_V3_MAX_ELEMENTS: u32 = 1 << 20;

/// Legacy alias retained for any external callers; v3 encoding is
/// variable-length so this constant only meaningfully describes v2.
#[deprecated(note = "use ORIOLEDB_STATE_V2_FIXED_SIZE; v3 is variable-length")]
pub const ORIOLEDB_STATE_ENCODED_SIZE: usize = ORIOLEDB_STATE_V2_FIXED_SIZE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Byte slice too short for the declared version.
    TooShort,
    /// Magic prefix did not match `ORIOLEDB_STATE_MAGIC`.
    BadMagic(u32),
    /// Version is beyond what this reader understands.
    UnsupportedVersion(u32),
    /// v3 dynamic-section element count exceeds the safety bound.
    /// Indicates a corrupted or malicious summary blob.
    TooManyElements(u32),
}

impl From<ContainerParseError> for IngestError {
    fn from(err: ContainerParseError) -> Self {
        Self::Parse(err)
    }
}

// --- Header parser ----------------------------------------------------------

/// Parse the header of a CONTAINER record payload.
///
/// Wire format (see `pgxn/orioledb/src/recovery/wal.c:939-977`):
///
/// ```text
/// uint16 wal_version          (little endian)
/// uint8  flags
/// if (flags & HAS_XACT_INFO)   WALRecXactInfo (12 bytes)
/// if (flags & HAS_ORIGIN_INFO) WALRecOriginInfo (10 bytes)
/// body ...                     (OrioleDB sub-record batch)
/// ```
pub fn parse_container_header(
    payload: &[u8],
) -> Result<ContainerHeader, ContainerParseError> {
    if payload.len() < CONTAINER_HEADER_MIN_LEN {
        return Err(ContainerParseError::TooShort);
    }

    let wal_version = u16::from_le_bytes([payload[0], payload[1]]);
    let flags = payload[2];
    let mut cursor = CONTAINER_HEADER_MIN_LEN;

    let pg_xid = if flags & WAL_CONTAINER_HAS_XACT_INFO != 0 {
        if payload.len() < cursor + WAL_REC_XACT_INFO_LEN {
            return Err(ContainerParseError::TooShort);
        }
        let xid = u32::from_le_bytes([
            payload[cursor + 8],
            payload[cursor + 9],
            payload[cursor + 10],
            payload[cursor + 11],
        ]);
        cursor += WAL_REC_XACT_INFO_LEN;
        Some(xid)
    } else {
        None
    };

    if flags & WAL_CONTAINER_HAS_ORIGIN_INFO != 0 {
        if payload.len() < cursor + WAL_REC_ORIGIN_INFO_LEN {
            return Err(ContainerParseError::TooShort);
        }
        cursor += WAL_REC_ORIGIN_INFO_LEN;
    }

    Ok(ContainerHeader {
        wal_version,
        flags,
        pg_xid,
        body_offset: cursor,
    })
}

// --- Body parser (v0.2: first sub-record only) ------------------------------

/// Linearly scan a CONTAINER body, decoding the sub-record types we
/// care about (XID, COMMIT, ROLLBACK, JOINT_COMMIT). Unknown types
/// abort the scan — we stop at the first byte we cannot interpret
/// rather than guess at record lengths.
///
/// Returns `(max_oxid, max_csn)` where either may be `None`.
///
/// COMMIT and JOINT_COMMIT bump `max_csn`; ROLLBACK only advances the
/// cursor (its `csn` field is `COMMITSEQNO_ABORTED` and must not bump
/// next_csn). XID bumps `max_oxid`.
///
/// Full sub-record inventory lives at
/// `pgxn/orioledb/include/recovery/wal.h`; B.4 handles the common
/// commit-path subset. Non-commit sub-records (INSERT / UPDATE /
/// DELETE / REINSERT / RELATION / …) terminate the scan — B.5/later
/// can extend to those if per-tree counter tracking is needed.
fn scan_container_body(body: &[u8]) -> (Option<u64>, Option<u64>) {
    let mut cursor = 0usize;
    let mut max_oxid: Option<u64> = None;
    let mut max_csn: Option<u64> = None;

    while cursor < body.len() {
        let rec_type = body[cursor];
        let consumed = match rec_type {
            wal_rec_type::XID => {
                if cursor + WAL_REC_XID_LEN > body.len() {
                    break;
                }
                let oxid = u64::from_le_bytes(
                    body[cursor + WAL_REC_XID_OXID_OFFSET
                        ..cursor + WAL_REC_XID_OXID_OFFSET + 8]
                        .try_into()
                        .unwrap(),
                );
                max_oxid = Some(match max_oxid {
                    Some(prev) => prev.max(oxid),
                    None => oxid,
                });
                WAL_REC_XID_LEN
            }
            wal_rec_type::COMMIT => {
                if cursor + WAL_REC_FINISH_LEN > body.len() {
                    break;
                }
                let csn = u64::from_le_bytes(
                    body[cursor + WAL_REC_FINISH_CSN_OFFSET
                        ..cursor + WAL_REC_FINISH_CSN_OFFSET + 8]
                        .try_into()
                        .unwrap(),
                );
                max_csn = Some(match max_csn {
                    Some(prev) => prev.max(csn),
                    None => csn,
                });
                WAL_REC_FINISH_LEN
            }
            wal_rec_type::ROLLBACK => {
                // ROLLBACK shares WALRecFinish layout but carries
                // COMMITSEQNO_ABORTED — we must not bump next_csn.
                if cursor + WAL_REC_FINISH_LEN > body.len() {
                    break;
                }
                WAL_REC_FINISH_LEN
            }
            wal_rec_type::JOINT_COMMIT => {
                if cursor + WAL_REC_JOINT_COMMIT_LEN > body.len() {
                    break;
                }
                let csn = u64::from_le_bytes(
                    body[cursor + WAL_REC_JOINT_COMMIT_CSN_OFFSET
                        ..cursor + WAL_REC_JOINT_COMMIT_CSN_OFFSET + 8]
                        .try_into()
                        .unwrap(),
                );
                max_csn = Some(match max_csn {
                    Some(prev) => prev.max(csn),
                    None => csn,
                });
                WAL_REC_JOINT_COMMIT_LEN
            }
            _ => {
                // Unknown sub-record: stop scanning. A future B.4+
                // expansion can add more types (INSERT / UPDATE / …)
                // once per-tree counter tracking is needed.
                break;
            }
        };
        cursor += consumed;
    }

    (max_oxid, max_csn)
}

/// Decode a CONTAINER record payload into the summary-relevant delta.
///
/// A malformed payload yields `OrioleDbRecordDelta::default()` — a
/// no-op from the summary's perspective. This design keeps the decoder
/// infallible from the walingest caller's perspective: malformed
/// records are logged by the caller but do not halt ingest.
pub fn decode_container_for_summary(payload: &[u8]) -> OrioleDbRecordDelta {
    let Ok(header) = parse_container_header(payload) else {
        return OrioleDbRecordDelta::default();
    };
    let body = &payload[header.body_offset..];
    let (max_oxid, max_csn) = scan_container_body(body);
    OrioleDbRecordDelta {
        pg_xid: header.pg_xid,
        max_oxid_in_body: max_oxid,
        max_csn_in_body: max_csn,
    }
}

// --- Summary updater --------------------------------------------------------

impl OrioleDBColdStartSummary {
    /// Apply one decoded record delta at `next_record_lsn_raw`.
    ///
    /// Enforces WAL-monotonic ordering: the LSN of every new record
    /// must exceed the previously ingested LSN (WAL stream invariant;
    /// a non-monotonic call indicates a caller bug).
    pub fn ingest_delta(
        &mut self,
        delta: &OrioleDbRecordDelta,
        next_record_lsn_raw: u64,
    ) -> Result<(), IngestError> {
        if self.ingested_count > 0 && next_record_lsn_raw <= self.last_ingested_lsn_raw {
            return Err(IngestError::NonMonotonicLsn {
                previous: self.last_ingested_lsn_raw,
                attempted: next_record_lsn_raw,
            });
        }

        if let Some(xid) = delta.pg_xid {
            self.last_pg_xid_seen = xid;
        }
        if let Some(oxid) = delta.max_oxid_in_body {
            let candidate = oxid.saturating_add(1);
            if candidate > self.next_oxid {
                self.next_oxid = candidate;
            }
        }
        if let Some(csn) = delta.max_csn_in_body {
            let candidate = csn.saturating_add(1);
            if candidate > self.next_csn {
                self.next_csn = candidate;
            }
        }

        self.ingested_count += 1;
        self.last_ingested_lsn_raw = next_record_lsn_raw;
        Ok(())
    }

    /// Convenience wrapper: decode the raw CONTAINER payload, then
    /// apply the resulting delta.
    pub fn ingest_container_record(
        &mut self,
        payload: &[u8],
        next_record_lsn_raw: u64,
    ) -> Result<(), IngestError> {
        // Monotonicity check happens first so a malformed payload
        // cannot quietly bump the LSN.
        if self.ingested_count > 0 && next_record_lsn_raw <= self.last_ingested_lsn_raw {
            return Err(IngestError::NonMonotonicLsn {
                previous: self.last_ingested_lsn_raw,
                attempted: next_record_lsn_raw,
            });
        }
        // Surface a truly malformed payload as a parse error.
        let _ = parse_container_header(payload)?;
        let delta = decode_container_for_summary(payload);
        self.ingest_delta(&delta, next_record_lsn_raw)
    }

    /// Encode the summary into a packed byte blob.
    ///
    /// Wire format v3 (all little-endian; v2 fixed prefix is a
    /// strict subset so v2 decoders can read the first 48 bytes
    /// transparently if they ignore the trailing dynamic section):
    ///
    /// ```text
    /// offset  size  field
    /// ── v2 fixed prefix (48 bytes) ──
    /// 0       4     magic                  (= ORIOLEDB_STATE_MAGIC)
    /// 4       4     version                (= ORIOLEDB_STATE_VERSION = 3)
    /// 8       8     next_oxid              (u64)
    /// 16      4     last_pg_xid_seen       (u32)
    /// 20      4     _reserved (0)          (alignment pad for C)
    /// 24      8     last_ingested_lsn_raw  (u64)
    /// 32      8     ingested_count         (u64)
    /// 40      8     next_csn               (u64)
    /// ── v3 counts header (16 bytes) ──
    /// 48      4     tree_count             (u32)
    /// 52      4     pending_split_count    (u32)
    /// 56      4     pending_merge_count    (u32)
    /// 60      4     _reserved (0)
    /// ── v3 dynamic section ──
    /// 64                              tree_count × PER_TREE_COUNTERS_SIZE
    /// 64 + tree_count*64              pending_split_count × PENDING_SPLIT_SIZE
    /// 64 + …                          pending_merge_count × PENDING_MERGE_SIZE
    /// ```
    ///
    /// Consumers: pageserver basebackup ships these bytes as
    /// `global/orioledb.state`; the C-side reader in
    /// `pgxn/orioledb/src/checkpoint/control.c` reads them at shmem
    /// startup.
    pub fn encode_packed(&self) -> Vec<u8> {
        let tree_count = self.per_tree.len();
        let pending_split_count = self.pending_splits.len();
        let pending_merge_count = self.pending_merges.len();
        let total_size = ORIOLEDB_STATE_V3_HEADER_SIZE
            + tree_count * PER_TREE_COUNTERS_SIZE
            + pending_split_count * PENDING_SPLIT_SIZE
            + pending_merge_count * PENDING_MERGE_SIZE;

        let mut buf = vec![0u8; total_size];

        // v2 fixed prefix
        buf[0..4].copy_from_slice(&ORIOLEDB_STATE_MAGIC.to_le_bytes());
        buf[4..8].copy_from_slice(&ORIOLEDB_STATE_VERSION.to_le_bytes());
        buf[8..16].copy_from_slice(&self.next_oxid.to_le_bytes());
        buf[16..20].copy_from_slice(&self.last_pg_xid_seen.to_le_bytes());
        // bytes 20..24 reserved/zero
        buf[24..32].copy_from_slice(&self.last_ingested_lsn_raw.to_le_bytes());
        buf[32..40].copy_from_slice(&self.ingested_count.to_le_bytes());
        buf[40..48].copy_from_slice(&self.next_csn.to_le_bytes());

        // v3 counts header
        buf[48..52].copy_from_slice(&(tree_count as u32).to_le_bytes());
        buf[52..56].copy_from_slice(&(pending_split_count as u32).to_le_bytes());
        buf[56..60].copy_from_slice(&(pending_merge_count as u32).to_le_bytes());
        // bytes 60..64 reserved/zero

        // per_tree[]
        let mut cursor = ORIOLEDB_STATE_V3_HEADER_SIZE;
        for entry in &self.per_tree {
            buf[cursor..cursor + 8].copy_from_slice(&entry.tree_id.to_le_bytes());
            buf[cursor + 8..cursor + 16].copy_from_slice(&entry.ctid.to_le_bytes());
            buf[cursor + 16..cursor + 24].copy_from_slice(&entry.bridge_ctid.to_le_bytes());
            buf[cursor + 24..cursor + 32].copy_from_slice(&entry.num_free_blocks.to_le_bytes());
            buf[cursor + 32..cursor + 40].copy_from_slice(&entry.leaf_pages_num.to_le_bytes());
            buf[cursor + 40..cursor + 48].copy_from_slice(&entry.datafile_length[0].to_le_bytes());
            buf[cursor + 48..cursor + 56].copy_from_slice(&entry.datafile_length[1].to_le_bytes());
            buf[cursor + 56..cursor + 64].copy_from_slice(&entry.undo_location.to_le_bytes());
            cursor += PER_TREE_COUNTERS_SIZE;
        }

        // pending_splits[]
        for entry in &self.pending_splits {
            buf[cursor..cursor + 8].copy_from_slice(&entry.tree_id.to_le_bytes());
            buf[cursor + 8..cursor + 12].copy_from_slice(&entry.left_blkno.to_le_bytes());
            buf[cursor + 12..cursor + 16].copy_from_slice(&entry.right_blkno.to_le_bytes());
            buf[cursor + 16..cursor + 24].copy_from_slice(&entry.child_hikey_lsn.to_le_bytes());
            buf[cursor + 24..cursor + 28]
                .copy_from_slice(&entry.child_hikey_offset.to_le_bytes());
            buf[cursor + 28..cursor + 32].copy_from_slice(&entry._reserved.to_le_bytes());
            cursor += PENDING_SPLIT_SIZE;
        }

        // pending_merges[]
        for entry in &self.pending_merges {
            buf[cursor..cursor + 8].copy_from_slice(&entry.tree_id.to_le_bytes());
            buf[cursor + 8..cursor + 12].copy_from_slice(&entry.left_blkno.to_le_bytes());
            buf[cursor + 12..cursor + 16].copy_from_slice(&entry.parent_blkno.to_le_bytes());
            buf[cursor + 16..cursor + 24].copy_from_slice(&entry.merge_lsn.to_le_bytes());
            cursor += PENDING_MERGE_SIZE;
        }

        debug_assert_eq!(cursor, total_size);
        buf
    }

    /// Decode a packed blob produced by `encode_packed`. Accepts both
    /// v2 (legacy fixed-size) and v3 (variable-length) blobs; v2
    /// decodes as v3 with empty `per_tree` / `pending_splits` /
    /// `pending_merges`. This back-compat is intentional so a
    /// PageServer running v3 code can read a v2 blob written by an
    /// older walingest during rolling upgrade.
    pub fn decode_packed(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() < ORIOLEDB_STATE_V2_FIXED_SIZE {
            return Err(DecodeError::TooShort);
        }
        let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        if magic != ORIOLEDB_STATE_MAGIC {
            return Err(DecodeError::BadMagic(magic));
        }
        let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        match version {
            2 => {
                // Legacy: just the v2 fixed prefix; dynamic section absent.
                Ok(Self {
                    next_oxid: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
                    last_pg_xid_seen: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
                    last_ingested_lsn_raw: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
                    ingested_count: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
                    next_csn: u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
                    per_tree: Vec::new(),
                    pending_splits: Vec::new(),
                    pending_merges: Vec::new(),
                })
            }
            3 => Self::decode_v3(bytes),
            _ => Err(DecodeError::UnsupportedVersion(version)),
        }
    }

    fn decode_v3(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() < ORIOLEDB_STATE_V3_HEADER_SIZE {
            return Err(DecodeError::TooShort);
        }
        let tree_count = u32::from_le_bytes(bytes[48..52].try_into().unwrap());
        let pending_split_count = u32::from_le_bytes(bytes[52..56].try_into().unwrap());
        let pending_merge_count = u32::from_le_bytes(bytes[56..60].try_into().unwrap());

        for &count in &[tree_count, pending_split_count, pending_merge_count] {
            if count > ORIOLEDB_STATE_V3_MAX_ELEMENTS {
                return Err(DecodeError::TooManyElements(count));
            }
        }

        let expected_size = ORIOLEDB_STATE_V3_HEADER_SIZE
            + (tree_count as usize) * PER_TREE_COUNTERS_SIZE
            + (pending_split_count as usize) * PENDING_SPLIT_SIZE
            + (pending_merge_count as usize) * PENDING_MERGE_SIZE;
        if bytes.len() < expected_size {
            return Err(DecodeError::TooShort);
        }

        let mut cursor = ORIOLEDB_STATE_V3_HEADER_SIZE;
        let mut per_tree = Vec::with_capacity(tree_count as usize);
        for _ in 0..tree_count {
            per_tree.push(PerTreeCounters {
                tree_id: u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap()),
                ctid: u64::from_le_bytes(bytes[cursor + 8..cursor + 16].try_into().unwrap()),
                bridge_ctid: u64::from_le_bytes(
                    bytes[cursor + 16..cursor + 24].try_into().unwrap(),
                ),
                num_free_blocks: u64::from_le_bytes(
                    bytes[cursor + 24..cursor + 32].try_into().unwrap(),
                ),
                leaf_pages_num: u64::from_le_bytes(
                    bytes[cursor + 32..cursor + 40].try_into().unwrap(),
                ),
                datafile_length: [
                    u64::from_le_bytes(bytes[cursor + 40..cursor + 48].try_into().unwrap()),
                    u64::from_le_bytes(bytes[cursor + 48..cursor + 56].try_into().unwrap()),
                ],
                undo_location: u64::from_le_bytes(
                    bytes[cursor + 56..cursor + 64].try_into().unwrap(),
                ),
            });
            cursor += PER_TREE_COUNTERS_SIZE;
        }

        let mut pending_splits = Vec::with_capacity(pending_split_count as usize);
        for _ in 0..pending_split_count {
            pending_splits.push(PendingSplit {
                tree_id: u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap()),
                left_blkno: u32::from_le_bytes(
                    bytes[cursor + 8..cursor + 12].try_into().unwrap(),
                ),
                right_blkno: u32::from_le_bytes(
                    bytes[cursor + 12..cursor + 16].try_into().unwrap(),
                ),
                child_hikey_lsn: u64::from_le_bytes(
                    bytes[cursor + 16..cursor + 24].try_into().unwrap(),
                ),
                child_hikey_offset: u32::from_le_bytes(
                    bytes[cursor + 24..cursor + 28].try_into().unwrap(),
                ),
                _reserved: u32::from_le_bytes(
                    bytes[cursor + 28..cursor + 32].try_into().unwrap(),
                ),
            });
            cursor += PENDING_SPLIT_SIZE;
        }

        let mut pending_merges = Vec::with_capacity(pending_merge_count as usize);
        for _ in 0..pending_merge_count {
            pending_merges.push(PendingMerge {
                tree_id: u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap()),
                left_blkno: u32::from_le_bytes(
                    bytes[cursor + 8..cursor + 12].try_into().unwrap(),
                ),
                parent_blkno: u32::from_le_bytes(
                    bytes[cursor + 12..cursor + 16].try_into().unwrap(),
                ),
                merge_lsn: u64::from_le_bytes(
                    bytes[cursor + 16..cursor + 24].try_into().unwrap(),
                ),
            });
            cursor += PENDING_MERGE_SIZE;
        }

        Ok(Self {
            next_oxid: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            last_pg_xid_seen: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            last_ingested_lsn_raw: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
            ingested_count: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
            next_csn: u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
            per_tree,
            pending_splits,
            pending_merges,
        })
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn build_container_payload(
        flags: u8,
        xact_xid: Option<u32>,
        origin: Option<(u16, u64)>,
        body: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + body.len());
        out.extend_from_slice(&42u16.to_le_bytes());
        out.push(flags);
        if flags & WAL_CONTAINER_HAS_XACT_INFO != 0 {
            let xid = xact_xid.expect("flag set but no xid provided");
            out.extend_from_slice(&0x1122334455667788u64.to_le_bytes());
            out.extend_from_slice(&xid.to_le_bytes());
        }
        if flags & WAL_CONTAINER_HAS_ORIGIN_INFO != 0 {
            let (origin_id, origin_lsn) = origin.expect("flag set but no origin provided");
            out.extend_from_slice(&origin_id.to_le_bytes());
            out.extend_from_slice(&origin_lsn.to_le_bytes());
        }
        out.extend_from_slice(body);
        out
    }

    /// Build a `WAL_REC_XID` sub-record body: recType + OXid(8) +
    /// logicalXid(4) + heapXid(4) = 17 bytes.
    fn wal_rec_xid_bytes(oxid: u64) -> Vec<u8> {
        let mut v = Vec::with_capacity(WAL_REC_XID_LEN);
        v.push(wal_rec_type::XID);
        v.extend_from_slice(&oxid.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // logicalXid
        v.extend_from_slice(&0u32.to_le_bytes()); // heapXid
        v
    }

    /// Build a `WALRecFinish` sub-record: recType + xmin(8) + csn(8)
    /// = 17 bytes. `rec_type` is either COMMIT or ROLLBACK.
    fn wal_rec_finish_bytes(rec_type: u8, xmin: u64, csn: u64) -> Vec<u8> {
        let mut v = Vec::with_capacity(WAL_REC_FINISH_LEN);
        v.push(rec_type);
        v.extend_from_slice(&xmin.to_le_bytes());
        v.extend_from_slice(&csn.to_le_bytes());
        v
    }

    /// Build a `WALRecJointCommit`: recType + xid(4) + xmin(8) +
    /// csn(8) = 21 bytes.
    fn wal_rec_joint_commit_bytes(xid: u32, xmin: u64, csn: u64) -> Vec<u8> {
        let mut v = Vec::with_capacity(WAL_REC_JOINT_COMMIT_LEN);
        v.push(wal_rec_type::JOINT_COMMIT);
        v.extend_from_slice(&xid.to_le_bytes());
        v.extend_from_slice(&xmin.to_le_bytes());
        v.extend_from_slice(&csn.to_le_bytes());
        v
    }

    // --- header parser tests ------------------------------------------------

    #[test]
    fn parse_bare_header() {
        let payload = build_container_payload(0, None, None, &[]);
        let header = parse_container_header(&payload).unwrap();
        assert_eq!(header.wal_version, 42);
        assert_eq!(header.flags, 0);
        assert_eq!(header.pg_xid, None);
        assert_eq!(header.body_offset, 3);
    }

    #[test]
    fn parse_header_with_xact_info_extracts_xid() {
        let payload = build_container_payload(
            WAL_CONTAINER_HAS_XACT_INFO,
            Some(0xDEADBEEF),
            None,
            b"body-bytes",
        );
        let header = parse_container_header(&payload).unwrap();
        assert_eq!(header.pg_xid, Some(0xDEADBEEF));
        assert_eq!(header.body_offset, 3 + WAL_REC_XACT_INFO_LEN);
    }

    #[test]
    fn parse_header_with_xact_and_origin_info() {
        let payload = build_container_payload(
            WAL_CONTAINER_HAS_XACT_INFO | WAL_CONTAINER_HAS_ORIGIN_INFO,
            Some(7),
            Some((3, 0xCAFEBABE_0001_u64)),
            b"body",
        );
        let header = parse_container_header(&payload).unwrap();
        assert_eq!(header.pg_xid, Some(7));
        assert_eq!(
            header.body_offset,
            3 + WAL_REC_XACT_INFO_LEN + WAL_REC_ORIGIN_INFO_LEN
        );
    }

    #[test]
    fn parse_header_truncated_returns_too_short() {
        assert!(matches!(
            parse_container_header(&[0u8, 0u8]),
            Err(ContainerParseError::TooShort)
        ));
        let mut truncated = vec![1u8, 0, WAL_CONTAINER_HAS_XACT_INFO];
        truncated.extend_from_slice(&[0u8; 4]);
        assert!(matches!(
            parse_container_header(&truncated),
            Err(ContainerParseError::TooShort)
        ));
    }

    // --- body scanner tests -------------------------------------------------

    #[test]
    fn scan_body_single_xid() {
        let body = wal_rec_xid_bytes(0x1234);
        let (oxid, csn) = scan_container_body(&body);
        assert_eq!(oxid, Some(0x1234));
        assert_eq!(csn, None);
    }

    #[test]
    fn scan_body_commit_extracts_csn() {
        let body = wal_rec_finish_bytes(wal_rec_type::COMMIT, /* xmin */ 1, /* csn */ 0xCC);
        let (oxid, csn) = scan_container_body(&body);
        assert_eq!(oxid, None);
        assert_eq!(csn, Some(0xCC));
    }

    #[test]
    fn scan_body_rollback_does_not_bump_csn() {
        // ROLLBACK carries COMMITSEQNO_ABORTED — must not advance
        // next_csn. Scanner still consumes the record and continues.
        let mut body = wal_rec_finish_bytes(wal_rec_type::ROLLBACK, 1, 0xFFFF_FFFF_FFFF_FFFF);
        body.extend_from_slice(&wal_rec_xid_bytes(7));
        let (oxid, csn) = scan_container_body(&body);
        assert_eq!(csn, None, "ROLLBACK must not contribute to next_csn");
        assert_eq!(oxid, Some(7), "scan continues past ROLLBACK to XID");
    }

    #[test]
    fn scan_body_joint_commit_extracts_csn() {
        let body = wal_rec_joint_commit_bytes(/* xid */ 5, /* xmin */ 2, /* csn */ 0xABCD);
        let (oxid, csn) = scan_container_body(&body);
        assert_eq!(oxid, None);
        assert_eq!(csn, Some(0xABCD));
    }

    #[test]
    fn scan_body_mixed_xid_then_commit() {
        let mut body = wal_rec_xid_bytes(100);
        body.extend_from_slice(&wal_rec_finish_bytes(wal_rec_type::COMMIT, 100, 555));
        let (oxid, csn) = scan_container_body(&body);
        assert_eq!(oxid, Some(100));
        assert_eq!(csn, Some(555));
    }

    #[test]
    fn scan_body_tracks_max_across_multiple_records() {
        let mut body = wal_rec_xid_bytes(10);
        body.extend_from_slice(&wal_rec_finish_bytes(wal_rec_type::COMMIT, 10, 100));
        body.extend_from_slice(&wal_rec_xid_bytes(50));
        body.extend_from_slice(&wal_rec_finish_bytes(wal_rec_type::COMMIT, 50, 500));
        body.extend_from_slice(&wal_rec_xid_bytes(30));
        body.extend_from_slice(&wal_rec_finish_bytes(wal_rec_type::COMMIT, 30, 300));
        let (oxid, csn) = scan_container_body(&body);
        assert_eq!(oxid, Some(50));
        assert_eq!(csn, Some(500));
    }

    #[test]
    fn scan_body_stops_at_unknown_record() {
        // INSERT (type 5) is not decoded yet — scan stops and returns
        // whatever was extracted up to that point.
        let mut body = wal_rec_xid_bytes(42);
        body.push(wal_rec_type::INSERT);
        body.extend_from_slice(&[0u8; 20]); // garbage — should not be read
        let (oxid, csn) = scan_container_body(&body);
        assert_eq!(oxid, Some(42));
        assert_eq!(csn, None);
    }

    #[test]
    fn scan_body_stops_at_truncation() {
        // XID needs 17 bytes but body gives only recType + 4.
        let body = vec![wal_rec_type::XID, 1, 2, 3, 4];
        let (oxid, csn) = scan_container_body(&body);
        assert_eq!(oxid, None);
        assert_eq!(csn, None);
    }

    // --- decode_container_for_summary ---------------------------------------

    #[test]
    fn decode_extracts_pg_xid_oxid_and_csn() {
        let mut body = wal_rec_xid_bytes(0x5555_6666);
        body.extend_from_slice(&wal_rec_finish_bytes(
            wal_rec_type::COMMIT,
            0x5555_6666,
            0xAAAA_BBBB,
        ));
        let payload = build_container_payload(
            WAL_CONTAINER_HAS_XACT_INFO,
            Some(0xABCD),
            None,
            &body,
        );
        let delta = decode_container_for_summary(&payload);
        assert_eq!(delta.pg_xid, Some(0xABCD));
        assert_eq!(delta.max_oxid_in_body, Some(0x5555_6666));
        assert_eq!(delta.max_csn_in_body, Some(0xAAAA_BBBB));
    }

    #[test]
    fn decode_malformed_payload_yields_empty_delta() {
        let delta = decode_container_for_summary(&[0u8]);
        assert_eq!(delta, OrioleDbRecordDelta::default());
    }

    // --- summary ingest tests -----------------------------------------------

    #[test]
    fn ingest_delta_updates_next_oxid_monotonically() {
        let mut sum = OrioleDBColdStartSummary::default();
        sum.ingest_delta(
            &OrioleDbRecordDelta {
                pg_xid: None,
                max_oxid_in_body: Some(10),
                max_csn_in_body: None,
            },
            100,
        )
        .unwrap();
        assert_eq!(sum.next_oxid, 11);

        sum.ingest_delta(
            &OrioleDbRecordDelta {
                pg_xid: None,
                max_oxid_in_body: Some(25),
                max_csn_in_body: None,
            },
            200,
        )
        .unwrap();
        assert_eq!(sum.next_oxid, 26);

        // Smaller OXid does NOT regress next_oxid.
        sum.ingest_delta(
            &OrioleDbRecordDelta {
                pg_xid: None,
                max_oxid_in_body: Some(15),
                max_csn_in_body: None,
            },
            300,
        )
        .unwrap();
        assert_eq!(sum.next_oxid, 26);
    }

    #[test]
    fn ingest_delta_updates_next_csn_monotonically() {
        let mut sum = OrioleDBColdStartSummary::default();
        sum.ingest_delta(
            &OrioleDbRecordDelta {
                pg_xid: None,
                max_oxid_in_body: None,
                max_csn_in_body: Some(1000),
            },
            100,
        )
        .unwrap();
        assert_eq!(sum.next_csn, 1001);

        // Larger CSN bumps.
        sum.ingest_delta(
            &OrioleDbRecordDelta {
                pg_xid: None,
                max_oxid_in_body: None,
                max_csn_in_body: Some(5000),
            },
            200,
        )
        .unwrap();
        assert_eq!(sum.next_csn, 5001);

        // Smaller does not regress.
        sum.ingest_delta(
            &OrioleDbRecordDelta {
                pg_xid: None,
                max_oxid_in_body: None,
                max_csn_in_body: Some(2500),
            },
            300,
        )
        .unwrap();
        assert_eq!(sum.next_csn, 5001);
    }

    #[test]
    fn ingest_delta_without_oxid_leaves_next_oxid_unchanged() {
        let mut sum = OrioleDBColdStartSummary::default();
        sum.ingest_delta(
            &OrioleDbRecordDelta {
                pg_xid: Some(77),
                max_oxid_in_body: None,
                max_csn_in_body: None,
            },
            500,
        )
        .unwrap();
        assert_eq!(sum.next_oxid, 0);
        assert_eq!(sum.next_csn, 0);
        assert_eq!(sum.last_pg_xid_seen, 77);
        assert_eq!(sum.ingested_count, 1);
    }

    #[test]
    fn ingest_container_record_round_trip() {
        let mut sum = OrioleDBColdStartSummary::default();
        let mut body = wal_rec_xid_bytes(99);
        body.extend_from_slice(&wal_rec_finish_bytes(wal_rec_type::COMMIT, 99, 7777));
        let payload = build_container_payload(
            WAL_CONTAINER_HAS_XACT_INFO,
            Some(0x1111),
            None,
            &body,
        );
        sum.ingest_container_record(&payload, 1000).unwrap();
        assert_eq!(sum.ingested_count, 1);
        assert_eq!(sum.last_ingested_lsn_raw, 1000);
        assert_eq!(sum.last_pg_xid_seen, 0x1111);
        assert_eq!(sum.next_oxid, 100);
        assert_eq!(sum.next_csn, 7778);
    }

    #[test]
    fn ingest_rejects_non_monotonic_lsn() {
        let mut sum = OrioleDBColdStartSummary::default();
        let payload = build_container_payload(0, None, None, b"");
        sum.ingest_container_record(&payload, 100).unwrap();
        let err = sum
            .ingest_container_record(&payload, 100)
            .expect_err("equal LSN must be rejected");
        assert!(matches!(err, IngestError::NonMonotonicLsn { .. }));
        let err = sum
            .ingest_container_record(&payload, 50)
            .expect_err("earlier LSN must be rejected");
        assert!(matches!(err, IngestError::NonMonotonicLsn { .. }));
        assert_eq!(sum.ingested_count, 1);
        assert_eq!(sum.last_ingested_lsn_raw, 100);
    }

    #[test]
    fn ingest_surfaces_parse_error() {
        let mut sum = OrioleDBColdStartSummary::default();
        let err = sum
            .ingest_container_record(&[0u8], 10)
            .expect_err("too-short payload must error");
        assert!(matches!(err, IngestError::Parse(ContainerParseError::TooShort)));
    }

    // --- packed wire format -------------------------------------------------

    #[test]
    fn packed_default_matches_expected_bytes() {
        let sum = OrioleDBColdStartSummary::default();
        let bytes = sum.encode_packed();
        // Default summary has empty dynamic section, so total size is
        // exactly the v3 fixed header.
        assert_eq!(bytes.len(), ORIOLEDB_STATE_V3_HEADER_SIZE);
        assert_eq!(
            u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            ORIOLEDB_STATE_MAGIC
        );
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            ORIOLEDB_STATE_VERSION
        );
        // All data fields are zero in a default summary.
        assert_eq!(&bytes[8..40], &[0u8; 32]);
        // v3 counts header — all zero since dynamic section is empty.
        assert_eq!(&bytes[48..64], &[0u8; 16]);
    }

    #[test]
    fn packed_roundtrip_preserves_fields() {
        let mut sum = OrioleDBColdStartSummary::default();
        sum.next_oxid = 0x0102_0304_0506_0708;
        sum.last_pg_xid_seen = 0xDEADBEEF;
        sum.last_ingested_lsn_raw = 0x1122_3344_5566_7788;
        sum.ingested_count = 42;
        sum.next_csn = 0x9999_AAAA_BBBB_CCCC;

        let encoded = sum.encode_packed();
        assert_eq!(encoded.len(), ORIOLEDB_STATE_V3_HEADER_SIZE);
        let decoded = OrioleDBColdStartSummary::decode_packed(&encoded).unwrap();
        assert_eq!(decoded, sum);
    }

    #[test]
    fn packed_v3_has_next_csn_at_offset_40() {
        let mut sum = OrioleDBColdStartSummary::default();
        sum.next_csn = 0x0123_4567_89AB_CDEF;
        let bytes = sum.encode_packed();
        assert_eq!(
            u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
            0x0123_4567_89AB_CDEF
        );
    }

    #[test]
    fn packed_decode_rejects_bad_magic() {
        let mut bytes = [0u8; ORIOLEDB_STATE_V3_HEADER_SIZE];
        bytes[0..4].copy_from_slice(&0xBAD0_BAD0_u32.to_le_bytes());
        assert!(matches!(
            OrioleDBColdStartSummary::decode_packed(&bytes),
            Err(DecodeError::BadMagic(_))
        ));
    }

    #[test]
    fn packed_decode_rejects_unsupported_version() {
        let mut sum = OrioleDBColdStartSummary::default();
        sum.next_oxid = 7;
        let mut bytes = sum.encode_packed();
        // Write a future version.
        bytes[4..8].copy_from_slice(&99u32.to_le_bytes());
        assert!(matches!(
            OrioleDBColdStartSummary::decode_packed(&bytes),
            Err(DecodeError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn packed_decode_rejects_truncated_input() {
        assert!(matches!(
            OrioleDBColdStartSummary::decode_packed(&[]),
            Err(DecodeError::TooShort)
        ));
        assert!(matches!(
            OrioleDBColdStartSummary::decode_packed(&[0u8; 10]),
            Err(DecodeError::TooShort)
        ));
    }

    // --- v3 dynamic-section round-trip --------------------------------------

    #[test]
    fn packed_v3_with_per_tree_round_trips() {
        let mut sum = OrioleDBColdStartSummary::default();
        sum.next_oxid = 100;
        sum.next_csn = 200;
        sum.per_tree.push(PerTreeCounters {
            tree_id: 0xAAAA_BBBB_CCCC_DDDD,
            ctid: 0x1111_2222_3333_4444,
            bridge_ctid: 0x0000_0000_0001_0001,
            num_free_blocks: 500,
            leaf_pages_num: 7,
            datafile_length: [4096, 8192],
            undo_location: 0x9999_8888_7777_6666,
        });
        sum.per_tree.push(PerTreeCounters {
            tree_id: 0xFFFF_EEEE_DDDD_CCCC,
            ctid: 9999,
            ..Default::default()
        });

        let encoded = sum.encode_packed();
        let expected_size =
            ORIOLEDB_STATE_V3_HEADER_SIZE + 2 * PER_TREE_COUNTERS_SIZE;
        assert_eq!(encoded.len(), expected_size);

        let decoded = OrioleDBColdStartSummary::decode_packed(&encoded).unwrap();
        assert_eq!(decoded, sum);
    }

    #[test]
    fn packed_v3_with_pending_splits_round_trips() {
        let mut sum = OrioleDBColdStartSummary::default();
        sum.pending_splits.push(PendingSplit {
            tree_id: 0x1234_5678_9ABC_DEF0,
            left_blkno: 100,
            right_blkno: 101,
            child_hikey_lsn: 0xDEAD_BEEF_CAFE_BABE,
            child_hikey_offset: 42,
            _reserved: 0,
        });
        sum.pending_splits.push(PendingSplit {
            tree_id: 0x1234_5678_9ABC_DEF0,
            left_blkno: 102,
            right_blkno: 103,
            child_hikey_lsn: 0xDEAD_BEEF_CAFE_BABF,
            child_hikey_offset: 0,
            _reserved: 0,
        });

        let encoded = sum.encode_packed();
        let expected_size =
            ORIOLEDB_STATE_V3_HEADER_SIZE + 2 * PENDING_SPLIT_SIZE;
        assert_eq!(encoded.len(), expected_size);

        let decoded = OrioleDBColdStartSummary::decode_packed(&encoded).unwrap();
        assert_eq!(decoded, sum);
    }

    #[test]
    fn packed_v3_with_pending_merges_round_trips() {
        let mut sum = OrioleDBColdStartSummary::default();
        sum.pending_merges.push(PendingMerge {
            tree_id: 0xCAFE_BABE_DEAD_BEEF,
            left_blkno: 50,
            parent_blkno: 1,
            merge_lsn: 0x1234_5678_9ABC_DEF0,
        });

        let encoded = sum.encode_packed();
        let expected_size = ORIOLEDB_STATE_V3_HEADER_SIZE + PENDING_MERGE_SIZE;
        assert_eq!(encoded.len(), expected_size);

        let decoded = OrioleDBColdStartSummary::decode_packed(&encoded).unwrap();
        assert_eq!(decoded, sum);
    }

    #[test]
    fn packed_v3_with_all_sections_round_trips() {
        let mut sum = OrioleDBColdStartSummary::default();
        sum.next_oxid = 1_000_000;
        sum.next_csn = 2_000_000;
        sum.last_pg_xid_seen = 0x12345;
        sum.last_ingested_lsn_raw = 0xABCD_EF01_2345_6789;
        sum.ingested_count = 1024;

        for i in 0..3 {
            sum.per_tree.push(PerTreeCounters {
                tree_id: 0x1000 + i as u64,
                ctid: 100 * (i + 1) as u64,
                bridge_ctid: 50 * (i + 1) as u64,
                num_free_blocks: 200,
                leaf_pages_num: 5 * (i + 1) as u64,
                datafile_length: [1024 * (i + 1) as u64, 2048 * (i + 1) as u64],
                undo_location: 0xFFFF + i as u64,
            });
        }
        sum.pending_splits.push(PendingSplit {
            tree_id: 0x1000,
            left_blkno: 1,
            right_blkno: 2,
            child_hikey_lsn: 100,
            child_hikey_offset: 0,
            _reserved: 0,
        });
        sum.pending_merges.push(PendingMerge {
            tree_id: 0x1001,
            left_blkno: 3,
            parent_blkno: 4,
            merge_lsn: 200,
        });

        let encoded = sum.encode_packed();
        let expected_size = ORIOLEDB_STATE_V3_HEADER_SIZE
            + 3 * PER_TREE_COUNTERS_SIZE
            + PENDING_SPLIT_SIZE
            + PENDING_MERGE_SIZE;
        assert_eq!(encoded.len(), expected_size);

        let decoded = OrioleDBColdStartSummary::decode_packed(&encoded).unwrap();
        assert_eq!(decoded, sum);
    }

    #[test]
    fn packed_v3_decodes_legacy_v2_blob() {
        // Hand-craft a v2 blob (48 bytes, version=2) and decode it with v3
        // reader. Result should be a v3 summary with empty dynamic sections.
        let mut bytes = vec![0u8; ORIOLEDB_STATE_V2_FIXED_SIZE];
        bytes[0..4].copy_from_slice(&ORIOLEDB_STATE_MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&2u32.to_le_bytes());
        bytes[8..16].copy_from_slice(&0xAAAA_u64.to_le_bytes());
        bytes[16..20].copy_from_slice(&0xBBBB_u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&0xCCCC_u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&0xDDDD_u64.to_le_bytes());
        bytes[40..48].copy_from_slice(&0xEEEE_u64.to_le_bytes());

        let decoded = OrioleDBColdStartSummary::decode_packed(&bytes).unwrap();
        assert_eq!(decoded.next_oxid, 0xAAAA);
        assert_eq!(decoded.last_pg_xid_seen, 0xBBBB);
        assert_eq!(decoded.last_ingested_lsn_raw, 0xCCCC);
        assert_eq!(decoded.ingested_count, 0xDDDD);
        assert_eq!(decoded.next_csn, 0xEEEE);
        assert!(decoded.per_tree.is_empty());
        assert!(decoded.pending_splits.is_empty());
        assert!(decoded.pending_merges.is_empty());
    }

    #[test]
    fn packed_v3_rejects_too_many_elements() {
        let mut bytes = vec![0u8; ORIOLEDB_STATE_V3_HEADER_SIZE];
        bytes[0..4].copy_from_slice(&ORIOLEDB_STATE_MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
        // Set tree_count past the safety bound.
        let huge = ORIOLEDB_STATE_V3_MAX_ELEMENTS + 1;
        bytes[48..52].copy_from_slice(&huge.to_le_bytes());
        assert!(matches!(
            OrioleDBColdStartSummary::decode_packed(&bytes),
            Err(DecodeError::TooManyElements(_))
        ));
    }

    #[test]
    fn packed_v3_rejects_truncated_dynamic_section() {
        // Header claims 5 trees but body is too short to contain them.
        let mut bytes = vec![0u8; ORIOLEDB_STATE_V3_HEADER_SIZE];
        bytes[0..4].copy_from_slice(&ORIOLEDB_STATE_MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
        bytes[48..52].copy_from_slice(&5u32.to_le_bytes());
        assert!(matches!(
            OrioleDBColdStartSummary::decode_packed(&bytes),
            Err(DecodeError::TooShort)
        ));
    }

    // --- serde roundtrip (still supported for debug / introspection) --------

    #[test]
    fn serde_roundtrip_preserves_state() {
        let mut sum = OrioleDBColdStartSummary::default();
        let mut body = wal_rec_xid_bytes(5000);
        body.extend_from_slice(&wal_rec_finish_bytes(wal_rec_type::COMMIT, 5000, 999));
        let payload = build_container_payload(
            WAL_CONTAINER_HAS_XACT_INFO,
            Some(0x1234),
            None,
            &body,
        );
        sum.ingest_container_record(&payload, 2000).unwrap();
        let encoded = serde_json::to_vec(&sum).unwrap();
        let decoded: OrioleDBColdStartSummary = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(sum, decoded);
    }
}
