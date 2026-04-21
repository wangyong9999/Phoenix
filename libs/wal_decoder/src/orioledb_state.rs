//! OrioleDB cold-start summary (walingest-maintained).
//!
//! Phase 2.1 B.3 — minimum-viable infrastructure for the
//! walingest-side state blob that compute reads at cold-start to
//! initialize OrioleDB shmem without replaying WAL.
//!
//! # Invariant linkage
//!
//! - **I4** (`docs/INVARIANTS.md §4`): compute cold-start does zero WAL
//!   replay. This module maintains the per-timeline summary that makes
//!   that possible. Alone it does not achieve I4 — basebackup delivery
//!   (Phase 2.1 C.1/C.2) + compute init codepath (C.3) close the loop.
//! - **I1** (persistence): the summary itself is derivable from the
//!   ingested rmid=129 stream — no new persistence source introduced.
//!   Summary checkpointing for pageserver restart is a separate
//!   mechanism (outside this module's scope).
//! - **I2, I3, I5**: orthogonal — the summary does not emit records,
//!   does not enter the `(rel, blkno)` keyspace, and does not
//!   participate in per-record transaction atomicity guarantees.
//!
//! # Scope of v0.1
//!
//! Infrastructure only. The summary struct + a parser for the common
//! CONTAINER record header and an ingest entry point that validates
//! input and updates bookkeeping fields (counter, last LSN).
//! Field-level extraction of OXID / CSN / per-tree counters from the
//! record body is Phase 2.1 **B.4**; the payload format for that
//! requires decoding OrioleDB's in-body sub-records
//! (`add_xid_wal_record`, `add_finish_wal_record`, etc.) which is
//! left to B.4 to do in one place. Here we establish the module,
//! types, and test harness so B.4 is a targeted expansion.
//!
//! # References
//!
//! - `docs/Q5_COLDSTART_SOURCES.md §2` — summary schema sketch.
//! - `docs/Q5_COLDSTART_SOURCES.md §3` — per-record update rules.
//! - `pgxn/orioledb/include/recovery/wal.h` — CONTAINER header layout.
//! - `pgxn/orioledb/src/recovery/wal.c:936-979` — header emit code.

use serde::{Deserialize, Serialize};

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

/// Parsed header of an `ORIOLEDB_XLOG_CONTAINER` (info=0x00) record.
///
/// `body_offset` indexes into the original payload where the
/// OrioleDB-internal serialized sub-record batch begins. This module
/// does not decode that body; B.4 adds decoders for the sub-record
/// types (WAL_REC_XID / WAL_REC_COMMIT / WAL_REC_ROLLBACK / ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContainerHeader {
    pub wal_version: u16,
    pub flags: u8,
    /// PG-layer TransactionId from the xact_info sub-structure, if the
    /// `HAS_XACT_INFO` flag was set. `None` otherwise.
    pub pg_xid: Option<u32>,
    /// Offset from start-of-payload to the body bytes that follow the
    /// fixed header.
    pub body_offset: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerParseError {
    /// Payload was shorter than the minimum CONTAINER header size.
    TooShort,
    /// `wal_version` in the payload does not match a supported range.
    /// (Currently we simply surface the value; the decision on which
    /// versions to accept lives with B.4.)
    VersionOutOfRange(u16),
}

/// Parse the header of a CONTAINER record payload.
///
/// Header wire format (see `pgxn/orioledb/src/recovery/wal.c:939-977`):
///
/// ```text
/// uint16 wal_version          (little endian)
/// uint8  flags
/// if (flags & HAS_XACT_INFO)   WALRecXactInfo (12 bytes)
/// if (flags & HAS_ORIGIN_INFO) WALRecOriginInfo (10 bytes)
/// body ...                     (OrioleDB sub-record batch)
/// ```
///
/// Returns the parsed header including extracted PG xid where present,
/// and the offset at which the body begins.
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
        // xactTime occupies bytes [cursor, cursor+8); xid occupies
        // [cursor+8, cursor+12).
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
        // Skip the origin bytes — B.4 decodes them if needed.
        cursor += WAL_REC_ORIGIN_INFO_LEN;
    }

    Ok(ContainerHeader {
        wal_version,
        flags,
        pg_xid,
        body_offset: cursor,
    })
}

/// Per-timeline OrioleDB cold-start summary.
///
/// Fields in v0.1 are deliberately minimal — they establish the
/// persistence and ingest pipeline without committing to the full
/// Q5 schema. Every addition in B.4 must be walingest-derivable from
/// the rmid=129 stream (see Q5 §3).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrioleDBColdStartSummary {
    /// Number of rmid=129 records successfully ingested into this
    /// summary. Monotonic; reset only when the summary is discarded.
    pub ingested_count: u64,

    /// Raw XLogRecPtr value of the most recently ingested record's
    /// `next_record_lsn`. Stored as u64; conversion to Postgres
    /// `XLogRecPtr`/`Lsn` happens at callsite boundaries.
    pub last_ingested_lsn_raw: u64,

    /// Last PG-layer TransactionId extracted from a CONTAINER
    /// `xact_info` sub-structure. `0` means "none seen yet".
    ///
    /// This is PG's xid, not OrioleDB's OXID. OXID extraction requires
    /// decoding the container body (Phase 2.1 B.4 scope).
    pub last_pg_xid_seen: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestError {
    /// The payload could not be parsed as a CONTAINER header.
    Parse(ContainerParseError),
    /// The caller supplied a `next_record_lsn` that is not strictly
    /// greater than the currently recorded `last_ingested_lsn_raw`.
    /// WAL is monotonic; reordering here would indicate a caller bug.
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

impl OrioleDBColdStartSummary {
    /// Ingest one rmid=129 CONTAINER record into the summary.
    ///
    /// The caller is responsible for:
    /// - Confirming the record's resource manager id is 129 and its
    ///   masked info byte is `CONTAINER` (0x00). LEAF_*/SPLIT/etc.
    ///   records are NOT consumed here in v0.1 (B.4 adds them).
    /// - Supplying the full payload bytes following the pg_xlog record
    ///   header, i.e. what `XLogRecGetData()` would return.
    /// - Supplying a `next_record_lsn` that is strictly greater than
    ///   any previously ingested record's LSN (WAL order invariant).
    pub fn ingest_container_record(
        &mut self,
        payload: &[u8],
        next_record_lsn_raw: u64,
    ) -> Result<(), IngestError> {
        if next_record_lsn_raw <= self.last_ingested_lsn_raw && self.ingested_count > 0 {
            return Err(IngestError::NonMonotonicLsn {
                previous: self.last_ingested_lsn_raw,
                attempted: next_record_lsn_raw,
            });
        }

        let header = parse_container_header(payload)?;
        if let Some(xid) = header.pg_xid {
            self.last_pg_xid_seen = xid;
        }

        self.ingested_count += 1;
        self.last_ingested_lsn_raw = next_record_lsn_raw;
        Ok(())
    }
}

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
        // wal_version — use an arbitrary supported-looking value.
        out.extend_from_slice(&42u16.to_le_bytes());
        out.push(flags);
        if flags & WAL_CONTAINER_HAS_XACT_INFO != 0 {
            let xid = xact_xid.expect("flag set but no xid provided");
            // xactTime — 8 opaque bytes; value doesn't matter for the parser.
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

        // XACT flag set but payload cut off mid-xact-info.
        let mut truncated = vec![1u8, 0, WAL_CONTAINER_HAS_XACT_INFO];
        truncated.extend_from_slice(&[0u8; 4]); // only 4 of 12 xact bytes
        assert!(matches!(
            parse_container_header(&truncated),
            Err(ContainerParseError::TooShort)
        ));
    }

    #[test]
    fn ingest_counts_and_tracks_lsn() {
        let mut sum = OrioleDBColdStartSummary::default();
        let payload = build_container_payload(0, None, None, b"x");

        sum.ingest_container_record(&payload, 100).unwrap();
        assert_eq!(sum.ingested_count, 1);
        assert_eq!(sum.last_ingested_lsn_raw, 100);
        assert_eq!(sum.last_pg_xid_seen, 0);

        sum.ingest_container_record(&payload, 150).unwrap();
        assert_eq!(sum.ingested_count, 2);
        assert_eq!(sum.last_ingested_lsn_raw, 150);
    }

    #[test]
    fn ingest_captures_pg_xid_from_xact_info() {
        let mut sum = OrioleDBColdStartSummary::default();
        let payload = build_container_payload(
            WAL_CONTAINER_HAS_XACT_INFO,
            Some(12345),
            None,
            b"",
        );
        sum.ingest_container_record(&payload, 200).unwrap();
        assert_eq!(sum.last_pg_xid_seen, 12345);
    }

    #[test]
    fn ingest_rejects_non_monotonic_lsn() {
        let mut sum = OrioleDBColdStartSummary::default();
        let payload = build_container_payload(0, None, None, b"x");

        sum.ingest_container_record(&payload, 100).unwrap();
        let err = sum
            .ingest_container_record(&payload, 100)
            .expect_err("equal LSN must be rejected");
        assert!(matches!(err, IngestError::NonMonotonicLsn { .. }));

        let err = sum
            .ingest_container_record(&payload, 50)
            .expect_err("earlier LSN must be rejected");
        assert!(matches!(err, IngestError::NonMonotonicLsn { .. }));

        // State unchanged after failed ingests.
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
            b"abc",
        );
        sum.ingest_container_record(&payload, 1000).unwrap();

        let encoded = serde_json::to_vec(&sum).unwrap();
        let decoded: OrioleDBColdStartSummary = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(sum, decoded);
    }
}
