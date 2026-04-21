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
/// - v2 (48 bytes): v1 + next_csn. Current.
pub const ORIOLEDB_STATE_VERSION: u32 = 2;

/// Fixed wire-format size for the current `ORIOLEDB_STATE_VERSION`.
pub const ORIOLEDB_STATE_ENCODED_SIZE: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Byte slice too short for the declared version.
    TooShort,
    /// Magic prefix did not match `ORIOLEDB_STATE_MAGIC`.
    BadMagic(u32),
    /// Version is beyond what this reader understands.
    UnsupportedVersion(u32),
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

    /// Encode the summary into a fixed-layout, C-readable byte blob.
    ///
    /// Wire format v2 (all little-endian, no padding beyond what's
    /// spelled out; total `ORIOLEDB_STATE_ENCODED_SIZE = 48 bytes`):
    ///
    /// ```text
    /// offset  size  field
    /// 0       4     magic                  (= ORIOLEDB_STATE_MAGIC)
    /// 4       4     version                (= ORIOLEDB_STATE_VERSION = 2)
    /// 8       8     next_oxid              (u64)
    /// 16      4     last_pg_xid_seen       (u32)
    /// 20      4     _reserved (0)          (alignment pad for C)
    /// 24      8     last_ingested_lsn_raw  (u64)
    /// 32      8     ingested_count         (u64)
    /// 40      8     next_csn               (u64)   <- NEW in v2
    /// ```
    ///
    /// Consumers: pageserver basebackup ships these bytes as
    /// `global/orioledb.state`; the C-side reader in
    /// `pgxn/orioledb/src/checkpoint/control.c` reads them at shmem
    /// startup.
    pub fn encode_packed(&self) -> [u8; ORIOLEDB_STATE_ENCODED_SIZE] {
        let mut buf = [0u8; ORIOLEDB_STATE_ENCODED_SIZE];
        buf[0..4].copy_from_slice(&ORIOLEDB_STATE_MAGIC.to_le_bytes());
        buf[4..8].copy_from_slice(&ORIOLEDB_STATE_VERSION.to_le_bytes());
        buf[8..16].copy_from_slice(&self.next_oxid.to_le_bytes());
        buf[16..20].copy_from_slice(&self.last_pg_xid_seen.to_le_bytes());
        // bytes 20..24 are the reserved/padding slot, already zero.
        buf[24..32].copy_from_slice(&self.last_ingested_lsn_raw.to_le_bytes());
        buf[32..40].copy_from_slice(&self.ingested_count.to_le_bytes());
        buf[40..48].copy_from_slice(&self.next_csn.to_le_bytes());
        buf
    }

    /// Decode a packed blob produced by `encode_packed`. Validates
    /// magic + version; unknown versions are rejected to keep the
    /// C-side reader strict.
    pub fn decode_packed(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() < ORIOLEDB_STATE_ENCODED_SIZE {
            return Err(DecodeError::TooShort);
        }
        let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        if magic != ORIOLEDB_STATE_MAGIC {
            return Err(DecodeError::BadMagic(magic));
        }
        let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        if version != ORIOLEDB_STATE_VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        Ok(Self {
            next_oxid: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            last_pg_xid_seen: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            last_ingested_lsn_raw: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
            ingested_count: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
            next_csn: u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
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
        assert_eq!(bytes.len(), ORIOLEDB_STATE_ENCODED_SIZE);
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
        assert_eq!(encoded.len(), 48);
        let decoded = OrioleDBColdStartSummary::decode_packed(&encoded).unwrap();
        assert_eq!(decoded, sum);
    }

    #[test]
    fn packed_v2_has_next_csn_at_offset_40() {
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
        let mut bytes = [0u8; ORIOLEDB_STATE_ENCODED_SIZE];
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
