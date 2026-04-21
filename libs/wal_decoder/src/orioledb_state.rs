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

/// Offset of the `oxid` field inside `WALRecXid` — immediately after
/// `recType` (1 byte). Width is `sizeof(OXid) = 8`. See
/// `pgxn/orioledb/include/recovery/wal.h:120-127`.
const WAL_REC_XID_OXID_OFFSET: usize = 1;

/// Minimum length we need to read a `WALRecXid` up to and including
/// the `oxid` field. The full record is larger (includes logicalXid
/// and heapXid) but we do not decode those fields in v0.2.
const WAL_REC_XID_MIN_LEN_FOR_OXID: usize = WAL_REC_XID_OXID_OFFSET + 8;

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
    /// OrioleDB OXid from the first `WAL_REC_XID` sub-record in the
    /// body (v0.2 decodes only the first sub-record).
    pub oxid_in_body: Option<u64>,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestError {
    Parse(ContainerParseError),
    NonMonotonicLsn {
        previous: u64,
        attempted: u64,
    },
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

/// Decode the body's first sub-record if it is `WAL_REC_XID`.
/// Returns the OXid from the record, else `None`.
///
/// v0.2 only decodes the first sub-record. A CONTAINER body typically
/// starts with `WAL_REC_XID` when the emitting backend writes the
/// OXid binding via `add_xid_wal_record`
/// (`pgxn/orioledb/src/recovery/wal.c`). Containers that start with a
/// different record type return `None` here — v0.3/B.4 adds full
/// sub-record traversal.
fn first_oxid_from_body(body: &[u8]) -> Option<u64> {
    if body.len() < WAL_REC_XID_MIN_LEN_FOR_OXID {
        return None;
    }
    if body[0] != wal_rec_type::XID {
        return None;
    }
    let oxid_bytes: [u8; 8] = body[WAL_REC_XID_OXID_OFFSET..WAL_REC_XID_OXID_OFFSET + 8]
        .try_into()
        .expect("slice length checked above");
    Some(u64::from_le_bytes(oxid_bytes))
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
    OrioleDbRecordDelta {
        pg_xid: header.pg_xid,
        oxid_in_body: first_oxid_from_body(body),
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
        if let Some(oxid) = delta.oxid_in_body {
            let candidate = oxid.saturating_add(1);
            if candidate > self.next_oxid {
                self.next_oxid = candidate;
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
    /// logicalXid(4) + heapXid(4) = 17 bytes. Values other than `oxid`
    /// are filler — v0.2 does not decode them.
    fn wal_rec_xid_bytes(oxid: u64) -> Vec<u8> {
        let mut v = Vec::with_capacity(17);
        v.push(wal_rec_type::XID);
        v.extend_from_slice(&oxid.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // logicalXid
        v.extend_from_slice(&0u32.to_le_bytes()); // heapXid
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

    // --- body parser tests --------------------------------------------------

    #[test]
    fn body_first_xid_extracted() {
        let body = wal_rec_xid_bytes(0x0000_0000_0000_1234);
        assert_eq!(first_oxid_from_body(&body), Some(0x1234));
    }

    #[test]
    fn body_without_xid_returns_none() {
        // recType = RELATION, not XID; v0.2 returns None.
        let body = vec![wal_rec_type::RELATION, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(first_oxid_from_body(&body), None);
    }

    #[test]
    fn body_too_short_returns_none() {
        assert_eq!(first_oxid_from_body(&[]), None);
        assert_eq!(first_oxid_from_body(&[wal_rec_type::XID, 1, 2]), None);
    }

    // --- decode_container_for_summary ---------------------------------------

    #[test]
    fn decode_extracts_both_pg_xid_and_body_oxid() {
        let mut payload = build_container_payload(
            WAL_CONTAINER_HAS_XACT_INFO,
            Some(0xABCD),
            None,
            &wal_rec_xid_bytes(0x5555_6666),
        );
        let delta = decode_container_for_summary(&payload);
        assert_eq!(delta.pg_xid, Some(0xABCD));
        assert_eq!(delta.oxid_in_body, Some(0x5555_6666));

        // With no xact_info flag, pg_xid disappears but body is still
        // parsed.
        payload = build_container_payload(0, None, None, &wal_rec_xid_bytes(42));
        let delta = decode_container_for_summary(&payload);
        assert_eq!(delta.pg_xid, None);
        assert_eq!(delta.oxid_in_body, Some(42));
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
                oxid_in_body: Some(10),
            },
            100,
        )
        .unwrap();
        assert_eq!(sum.next_oxid, 11);

        // Larger OXid bumps next_oxid.
        sum.ingest_delta(
            &OrioleDbRecordDelta {
                pg_xid: None,
                oxid_in_body: Some(25),
            },
            200,
        )
        .unwrap();
        assert_eq!(sum.next_oxid, 26);

        // Smaller OXid does NOT regress next_oxid.
        sum.ingest_delta(
            &OrioleDbRecordDelta {
                pg_xid: None,
                oxid_in_body: Some(15),
            },
            300,
        )
        .unwrap();
        assert_eq!(sum.next_oxid, 26);
    }

    #[test]
    fn ingest_delta_without_oxid_leaves_next_oxid_unchanged() {
        let mut sum = OrioleDBColdStartSummary::default();
        sum.ingest_delta(
            &OrioleDbRecordDelta {
                pg_xid: Some(77),
                oxid_in_body: None,
            },
            500,
        )
        .unwrap();
        assert_eq!(sum.next_oxid, 0);
        assert_eq!(sum.last_pg_xid_seen, 77);
        assert_eq!(sum.ingested_count, 1);
    }

    #[test]
    fn ingest_container_record_round_trip() {
        let mut sum = OrioleDBColdStartSummary::default();
        let payload = build_container_payload(
            WAL_CONTAINER_HAS_XACT_INFO,
            Some(0x1111),
            None,
            &wal_rec_xid_bytes(99),
        );
        sum.ingest_container_record(&payload, 1000).unwrap();
        assert_eq!(sum.ingested_count, 1);
        assert_eq!(sum.last_ingested_lsn_raw, 1000);
        assert_eq!(sum.last_pg_xid_seen, 0x1111);
        assert_eq!(sum.next_oxid, 100);
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

    #[test]
    fn serde_roundtrip_preserves_state() {
        let mut sum = OrioleDBColdStartSummary::default();
        let payload = build_container_payload(
            WAL_CONTAINER_HAS_XACT_INFO,
            Some(0x1234),
            None,
            &wal_rec_xid_bytes(5000),
        );
        sum.ingest_container_record(&payload, 2000).unwrap();
        let encoded = serde_json::to_vec(&sum).unwrap();
        let decoded: OrioleDBColdStartSummary = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(sum, decoded);
    }
}
