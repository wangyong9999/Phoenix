//! This module implements batch type for serialized [`crate::models::value::Value`]
//! instances. Each batch contains a raw buffer (serialized values)
//! and a list of metadata for each (key, LSN) tuple present in the batch.
//!
//! Such batches are created from decoded PG wal records and ingested
//! by the pageserver by writing directly to the ephemeral file.

use std::collections::{BTreeSet, HashMap};

use bytes::{Bytes, BytesMut};
use pageserver_api::key::{CompactKey, Key, rel_block_to_key, orioledb_wal_key};
use pageserver_api::keyspace::KeySpace;
use pageserver_api::reltag::RelTag;
use pageserver_api::shard::ShardIdentity;
use postgres_ffi::walrecord::{DecodedBkpBlock, DecodedWALRecord};
use postgres_ffi::{BLCKSZ, PgMajorVersion, page_is_new, page_set_lsn, pg_constants};
use serde::{Deserialize, Serialize};
use utils::bin_ser::BeSer;
use utils::lsn::Lsn;

use crate::models::InterpretedWalRecord;
use crate::models::record::NeonWalRecord;
use crate::models::value::Value;

static ZERO_PAGE: Bytes = Bytes::from_static(&[0u8; BLCKSZ as usize]);

/// Accompanying metadata for the batch
/// A value may be serialized and stored into the batch or just "observed".
/// Shard 0 currently "observes" all values in order to accurately track
/// relation sizes. In the case of "observed" values, we only need to know
/// the key and LSN, so two types of metadata are supported to save on network
/// bandwidth.
#[derive(Serialize, Deserialize, Clone)]
pub enum ValueMeta {
    Serialized(SerializedValueMeta),
    Observed(ObservedValueMeta),
}

impl ValueMeta {
    pub fn key(&self) -> CompactKey {
        match self {
            Self::Serialized(ser) => ser.key,
            Self::Observed(obs) => obs.key,
        }
    }

    pub fn lsn(&self) -> Lsn {
        match self {
            Self::Serialized(ser) => ser.lsn,
            Self::Observed(obs) => obs.lsn,
        }
    }
}

/// Wrapper around [`ValueMeta`] that implements ordering by
/// (key, LSN) tuples
struct OrderedValueMeta(ValueMeta);

impl Ord for OrderedValueMeta {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.0.key(), self.0.lsn()).cmp(&(other.0.key(), other.0.lsn()))
    }
}

impl PartialOrd for OrderedValueMeta {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for OrderedValueMeta {
    fn eq(&self, other: &Self) -> bool {
        (self.0.key(), self.0.lsn()) == (other.0.key(), other.0.lsn())
    }
}

impl Eq for OrderedValueMeta {}

/// Metadata for a [`Value`] serialized into the batch.
#[derive(Serialize, Deserialize, Clone)]
pub struct SerializedValueMeta {
    pub key: CompactKey,
    pub lsn: Lsn,
    /// Starting offset of the value for the (key, LSN) tuple
    /// in [`SerializedValueBatch::raw`]
    pub batch_offset: u64,
    pub len: usize,
    pub will_init: bool,
}

/// Metadata for a [`Value`] observed by the batch
#[derive(Serialize, Deserialize, Clone)]
pub struct ObservedValueMeta {
    pub key: CompactKey,
    pub lsn: Lsn,
}

/// Batch of serialized [`Value`]s.
#[derive(Serialize, Deserialize, Clone)]
pub struct SerializedValueBatch {
    /// [`Value`]s serialized in EphemeralFile's native format,
    /// ready for disk write by the pageserver
    pub raw: Vec<u8>,

    /// Metadata to make sense of the bytes in [`Self::raw`]
    /// and represent "observed" values.
    ///
    /// Invariant: Metadata entries for any given key are ordered
    /// by LSN. Note that entries for a key do not have to be contiguous.
    pub metadata: Vec<ValueMeta>,

    /// The highest LSN of any value in the batch
    pub max_lsn: Lsn,

    /// Number of values encoded by [`Self::raw`]
    pub len: usize,
}

impl Default for SerializedValueBatch {
    fn default() -> Self {
        Self {
            raw: Default::default(),
            metadata: Default::default(),
            max_lsn: Lsn(0),
            len: 0,
        }
    }
}

impl SerializedValueBatch {
    /// Populates the given `shard_records` with value batches from this WAL record, if any,
    /// discarding those belonging to other shards.
    ///
    /// The batch will only contain values for keys targeting the specifiec
    /// shard. Shard 0 is a special case, where any keys that don't belong to
    /// it are "observed" by the batch (i.e. present in [`SerializedValueBatch::metadata`],
    /// but absent from the raw buffer [`SerializedValueBatch::raw`]).
    pub(crate) fn from_decoded_filtered(
        decoded: DecodedWALRecord,
        shard_records: &mut HashMap<ShardIdentity, InterpretedWalRecord>,
        next_record_lsn: Lsn,
        pg_version: PgMajorVersion,
    ) -> anyhow::Result<()> {
        // First determine how big the buffers need to be and allocate it up-front.
        // This duplicates some of the work below, but it's empirically much faster.
        for (shard, record) in shard_records.iter_mut() {
            assert!(record.batch.is_empty());

            let estimate = Self::estimate_buffer_size(&decoded, shard, pg_version);
            record.batch.raw = Vec::with_capacity(estimate);
        }

        for blk in decoded.blocks.iter() {
            let rel = RelTag {
                spcnode: blk.rnode_spcnode,
                dbnode: blk.rnode_dbnode,
                relnode: blk.rnode_relnode,
                forknum: blk.forknum,
            };

            let key = rel_block_to_key(rel, blk.blkno);

            if !key.is_valid_key_on_write_path() {
                anyhow::bail!(
                    "Unsupported key decoded at LSN {}: {}",
                    next_record_lsn,
                    key
                );
            }

            for (shard, record) in shard_records.iter_mut() {
                let key_is_local = shard.is_key_local(&key);

                tracing::debug!(
                    lsn=%next_record_lsn,
                    key=%key,
                    "ingest: shard decision {}",
                    if !key_is_local { "drop" } else { "keep" },
                );

                if !key_is_local {
                    if shard.is_shard_zero() {
                        // Shard 0 tracks relation sizes.  Although we will not store this block, we will observe
                        // its blkno in case it implicitly extends a relation.
                        record
                            .batch
                            .metadata
                            .push(ValueMeta::Observed(ObservedValueMeta {
                                key: key.to_compact(),
                                lsn: next_record_lsn,
                            }))
                    }

                    continue;
                }

                // Instead of storing full-page-image WAL record,
                // it is better to store extracted image: we can skip wal-redo
                // in this case. Also some FPI records may contain multiple (up to 32) pages,
                // so them have to be copied multiple times.
                //
                // OrioleDB (rmid=129) page-level WAL:
                //   FPI records (REGBUF_FORCE_IMAGE): stored as Value::Image (below)
                //   Delta records (LEAF_INSERT/DELETE/UPDATE): stored as
                //     Value::WalRecord for wal-redo to apply via orioledb_page_redo()
                //
                // Old row-level OrioleDB WAL (ORIOLEDB_XLOG_CONTAINER, info=0x00)
                // has NO block refs, so this path is only hit by the new
                // page-level delta records (info >= 0x10).

                let val = if Self::block_is_image(&decoded, blk, pg_version) {
                    // Extract page image from FPI record
                    let img_len = blk.bimg_len as usize;
                    let img_offs = blk.bimg_offset as usize;
                    let mut image = BytesMut::with_capacity(BLCKSZ as usize);
                    // TODO(vlad): skip the copy
                    image.extend_from_slice(&decoded.record[img_offs..img_offs + img_len]);

                    if blk.hole_length != 0 {
                        let tail = image.split_off(blk.hole_offset as usize);
                        image.resize(image.len() + blk.hole_length as usize, 0u8);
                        image.unsplit(tail);
                    }
                    //
                    // Match the logic of XLogReadBufferForRedoExtended:
                    // The page may be uninitialized. If so, we can't set the LSN because
                    // that would corrupt the page.
                    //
                    // OrioleDB (rmid=129) pages have a different header layout
                    // (OrioleDBPageHeader: state + pageChangeCount + checkpointNum)
                    // that is incompatible with PG's PageHeaderData. Writing pd_lsn
                    // at offset 0 would corrupt the state field. PageServer tracks
                    // page versions by (key, LSN) pairs, not by in-page LSN, so
                    // skipping this is safe.
                    //
                    if decoded.xl_rmid != 129 && !page_is_new(&image) {
                        page_set_lsn(&mut image, next_record_lsn)
                    }
                    assert_eq!(image.len(), BLCKSZ as usize);

                    Value::Image(image.freeze())
                } else {
                    Value::WalRecord(NeonWalRecord::Postgres {
                        will_init: blk.will_init || blk.apply_image,
                        rec: decoded.record.clone(),
                    })
                };

                let relative_off = record.batch.raw.len() as u64;

                val.ser_into(&mut record.batch.raw)
                    .expect("Writing into in-memory buffer is infallible");

                let val_ser_size = record.batch.raw.len() - relative_off as usize;

                record
                    .batch
                    .metadata
                    .push(ValueMeta::Serialized(SerializedValueMeta {
                        key: key.to_compact(),
                        lsn: next_record_lsn,
                        batch_offset: relative_off,
                        len: val_ser_size,
                        will_init: val.will_init(),
                    }));
                record.batch.max_lsn = std::cmp::max(record.batch.max_lsn, next_record_lsn);
                record.batch.len += 1;
            }
        }

        // OrioleDB WAL records: store as relation-level stream regardless of
        // whether the record carries block references — PG walredo can't
        // replay rmid=129, so we don't associate the record with any page.
        // Non-FPI OrioleDB block refs are already skipped in the block loop
        // above; this path captures every rmid=129 record end-to-end.
        if decoded.xl_rmid == 129 {
            let key = orioledb_wal_key(next_record_lsn.0);

            let val = Value::WalRecord(NeonWalRecord::Postgres {
                will_init: true,
                rec: decoded.record.clone(),
            });

            for (_shard, record) in shard_records.iter_mut() {
                let relative_off = record.batch.raw.len() as u64;
                val.ser_into(&mut record.batch.raw)
                    .expect("Writing into in-memory buffer is infallible");
                let val_ser_size = record.batch.raw.len() - relative_off as usize;

                record
                    .batch
                    .metadata
                    .push(ValueMeta::Serialized(SerializedValueMeta {
                        key: key.to_compact(),
                        lsn: next_record_lsn,
                        batch_offset: relative_off,
                        len: val_ser_size,
                        will_init: true,
                    }));
                record.batch.max_lsn = std::cmp::max(record.batch.max_lsn, next_record_lsn);
                record.batch.len += 1;

                // Only store on shard 0
                break;
            }
        }

        if cfg!(any(debug_assertions, test)) {
            // Validate that the batches are correct
            for record in shard_records.values() {
                record.batch.validate_lsn_order();
            }
        }

        Ok(())
    }

    /// Look into the decoded PG WAL record and determine
    /// roughly how large the buffer for serialized values needs to be.
    fn estimate_buffer_size(
        decoded: &DecodedWALRecord,
        shard: &ShardIdentity,
        pg_version: PgMajorVersion,
    ) -> usize {
        let mut estimate: usize = 0;

        for blk in decoded.blocks.iter() {
            let rel = RelTag {
                spcnode: blk.rnode_spcnode,
                dbnode: blk.rnode_dbnode,
                relnode: blk.rnode_relnode,
                forknum: blk.forknum,
            };

            let key = rel_block_to_key(rel, blk.blkno);

            if !shard.is_key_local(&key) {
                continue;
            }

            if Self::block_is_image(decoded, blk, pg_version) {
                // 4 bytes for the Value::Image discriminator
                // 8 bytes for encoding the size of the buffer
                // BLCKSZ for the raw image
                estimate += (4 + 8 + BLCKSZ) as usize;
            } else {
                // 4 bytes for the Value::WalRecord discriminator
                // 4 bytes for the NeonWalRecord::Postgres discriminator
                // 1 bytes for NeonWalRecord::Postgres::will_init
                // 8 bytes for encoding the size of the buffer
                // length of the raw record
                estimate += 8 + 1 + 8 + decoded.record.len();
            }
        }

        // OrioleDB WAL records (no blocks)
        if decoded.blocks.is_empty() && decoded.xl_rmid == 129 {
            estimate += 8 + 1 + 8 + decoded.record.len();
        }

        estimate
    }

    fn block_is_image(
        decoded: &DecodedWALRecord,
        blk: &DecodedBkpBlock,
        pg_version: PgMajorVersion,
    ) -> bool {
        blk.apply_image
            && blk.has_image
            && (decoded.xl_rmid == pg_constants::RM_XLOG_ID
                && (decoded.xl_info == pg_constants::XLOG_FPI
                    || decoded.xl_info == pg_constants::XLOG_FPI_FOR_HINT)
                // OrioleDB Plan E: FPI emitted via ORIOLEDB_RMGR_ID (129)
                // with REGBUF_FORCE_IMAGE|REGBUF_WILL_INIT — treat as image.
                || decoded.xl_rmid == 129)
            // compression of WAL is not yet supported: fall back to storing the original WAL record
            && !postgres_ffi::bkpimage_is_compressed(blk.bimg_info, pg_version)
            // do not materialize null pages because them most likely be soon replaced with real data
            && blk.bimg_len != 0
    }

    /// Encode a list of values and metadata into a serialized batch
    ///
    /// This is used by the pageserver ingest code to conveniently generate
    /// batches for metadata writes.
    pub fn from_values(batch: Vec<(CompactKey, Lsn, usize, Value)>) -> Self {
        // Pre-allocate a big flat buffer to write into. This should be large but not huge: it is soft-limited in practice by
        // [`crate::pgdatadir_mapping::DatadirModification::MAX_PENDING_BYTES`]
        let buffer_size = batch.iter().map(|i| i.2).sum::<usize>();
        let mut buf = Vec::<u8>::with_capacity(buffer_size);

        let mut metadata: Vec<ValueMeta> = Vec::with_capacity(batch.len());
        let mut max_lsn: Lsn = Lsn(0);
        let len = batch.len();
        for (key, lsn, val_ser_size, val) in batch {
            let relative_off = buf.len() as u64;

            val.ser_into(&mut buf)
                .expect("Writing into in-memory buffer is infallible");

            metadata.push(ValueMeta::Serialized(SerializedValueMeta {
                key,
                lsn,
                batch_offset: relative_off,
                len: val_ser_size,
                will_init: val.will_init(),
            }));
            max_lsn = std::cmp::max(max_lsn, lsn);
        }

        // Assert that we didn't do any extra allocations while building buffer.
        debug_assert!(buf.len() <= buffer_size);

        if cfg!(any(debug_assertions, test)) {
            let batch = Self {
                raw: buf,
                metadata,
                max_lsn,
                len,
            };

            batch.validate_lsn_order();

            return batch;
        }

        Self {
            raw: buf,
            metadata,
            max_lsn,
            len,
        }
    }

    /// Add one value to the batch
    ///
    /// This is used by the pageserver ingest code to include metadata block
    /// updates for a single key.
    pub fn put(&mut self, key: CompactKey, value: Value, lsn: Lsn) {
        let relative_off = self.raw.len() as u64;
        value.ser_into(&mut self.raw).unwrap();

        let val_ser_size = self.raw.len() - relative_off as usize;
        self.metadata
            .push(ValueMeta::Serialized(SerializedValueMeta {
                key,
                lsn,
                batch_offset: relative_off,
                len: val_ser_size,
                will_init: value.will_init(),
            }));

        self.max_lsn = std::cmp::max(self.max_lsn, lsn);
        self.len += 1;

        if cfg!(any(debug_assertions, test)) {
            self.validate_lsn_order();
        }
    }

    /// Extend with the contents of another batch
    ///
    /// One batch is generated for each decoded PG WAL record.
    /// They are then merged to accumulate reasonably sized writes.
    pub fn extend(&mut self, mut other: SerializedValueBatch) {
        let extend_batch_start_offset = self.raw.len() as u64;

        self.raw.extend(other.raw);

        // Shift the offsets in the batch we are extending with
        other.metadata.iter_mut().for_each(|meta| match meta {
            ValueMeta::Serialized(ser) => {
                ser.batch_offset += extend_batch_start_offset;
                if cfg!(debug_assertions) {
                    let value_end = ser.batch_offset + ser.len as u64;
                    assert!((value_end as usize) <= self.raw.len());
                }
            }
            ValueMeta::Observed(_) => {}
        });
        self.metadata.extend(other.metadata);

        self.max_lsn = std::cmp::max(self.max_lsn, other.max_lsn);

        self.len += other.len;

        if cfg!(any(debug_assertions, test)) {
            self.validate_lsn_order();
        }
    }

    /// Add zero images for the (key, LSN) tuples specified
    ///
    /// PG versions below 16 do not zero out pages before extending
    /// a relation and may leave gaps. Such gaps need to be identified
    /// by the pageserver ingest logic and get patched up here.
    ///
    /// Note that this function does not validate that the gaps have been
    /// identified correctly (it does not know relation sizes), so it's up
    /// to the call-site to do it properly.
    pub fn zero_gaps(&mut self, gaps: Vec<(KeySpace, Lsn)>) {
        // Implementation note:
        //
        // Values within [`SerializedValueBatch::raw`] do not have any ordering requirements,
        // but the metadata entries should be ordered properly (see
        // [`SerializedValueBatch::metadata`]).
        //
        // Exploiting this observation we do:
        // 1. Drain all the metadata entries into an ordered set.
        // The use of a BTreeSet keyed by (Key, Lsn) relies on the observation that Postgres never
        // includes more than one update to the same block in the same WAL record.
        // 2. For each (key, LSN) gap tuple, append a zero image to the raw buffer
        // and add an index entry to the ordered metadata set.
        // 3. Drain the ordered set back into a metadata vector

        let mut ordered_metas = self
            .metadata
            .drain(..)
            .map(OrderedValueMeta)
            .collect::<BTreeSet<_>>();
        for (keyspace, lsn) in gaps {
            self.max_lsn = std::cmp::max(self.max_lsn, lsn);

            for gap_range in keyspace.ranges {
                let mut key = gap_range.start;
                while key != gap_range.end {
                    let relative_off = self.raw.len() as u64;

                    // TODO(vlad): Can we be cheeky and write only one zero image, and
                    // make all index entries requiring a zero page point to it?
                    // Alternatively, we can change the index entry format to represent zero pages
                    // without writing them at all.
                    Value::Image(ZERO_PAGE.clone())
                        .ser_into(&mut self.raw)
                        .unwrap();
                    let val_ser_size = self.raw.len() - relative_off as usize;

                    ordered_metas.insert(OrderedValueMeta(ValueMeta::Serialized(
                        SerializedValueMeta {
                            key: key.to_compact(),
                            lsn,
                            batch_offset: relative_off,
                            len: val_ser_size,
                            will_init: true,
                        },
                    )));

                    self.len += 1;

                    key = key.next();
                }
            }
        }

        self.metadata = ordered_metas.into_iter().map(|ord| ord.0).collect();

        if cfg!(any(debug_assertions, test)) {
            self.validate_lsn_order();
        }
    }

    /// Checks if the batch contains any serialized or observed values
    pub fn is_empty(&self) -> bool {
        !self.has_data() && self.metadata.is_empty()
    }

    /// Checks if the batch contains only observed values
    pub fn is_observed(&self) -> bool {
        !self.has_data() && !self.metadata.is_empty()
    }

    /// Checks if the batch contains data
    ///
    /// Note that if this returns false, it may still contain observed values or
    /// a metadata record.
    pub fn has_data(&self) -> bool {
        let empty = self.raw.is_empty();

        if cfg!(debug_assertions) && empty {
            assert!(
                self.metadata
                    .iter()
                    .all(|meta| matches!(meta, ValueMeta::Observed(_)))
            );
        }

        !empty
    }

    /// Returns the number of values serialized in the batch
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns the size of the buffer wrapped by the batch
    pub fn buffer_size(&self) -> usize {
        self.raw.len()
    }

    pub fn updates_key(&self, key: &Key) -> bool {
        self.metadata.iter().any(|meta| match meta {
            ValueMeta::Serialized(ser) => key.to_compact() == ser.key,
            ValueMeta::Observed(_) => false,
        })
    }

    pub fn validate_lsn_order(&self) {
        use std::collections::HashMap;

        let mut last_seen_lsn_per_key: HashMap<CompactKey, Lsn> = HashMap::default();

        for meta in self.metadata.iter() {
            let lsn = meta.lsn();
            let key = meta.key();

            if let Some(prev_lsn) = last_seen_lsn_per_key.insert(key, lsn) {
                assert!(
                    lsn >= prev_lsn,
                    "Ordering violated by {}: {} < {}",
                    Key::from_compact(key),
                    lsn,
                    prev_lsn
                );
            }
        }
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;

    fn validate_batch(
        batch: &SerializedValueBatch,
        values: &[(CompactKey, Lsn, usize, Value)],
        gaps: Option<&Vec<(KeySpace, Lsn)>>,
    ) {
        // Invariant 1: The metadata for a given entry in the batch
        // is correct and can be used to deserialize back to the original value.
        for (key, lsn, size, value) in values.iter() {
            let meta = batch
                .metadata
                .iter()
                .find(|meta| (meta.key(), meta.lsn()) == (*key, *lsn))
                .unwrap();
            let meta = match meta {
                ValueMeta::Serialized(ser) => ser,
                ValueMeta::Observed(_) => unreachable!(),
            };

            assert_eq!(meta.len, *size);
            assert_eq!(meta.will_init, value.will_init());

            let start = meta.batch_offset as usize;
            let end = meta.batch_offset as usize + meta.len;
            let value_from_batch = Value::des(&batch.raw[start..end]).unwrap();
            assert_eq!(&value_from_batch, value);
        }

        let mut expected_buffer_size: usize = values.iter().map(|(_, _, size, _)| size).sum();
        let mut gap_pages_count: usize = 0;

        // Invariant 2: Zero pages were added for identified gaps and their metadata
        // is correct.
        if let Some(gaps) = gaps {
            for (gap_keyspace, lsn) in gaps {
                for gap_range in &gap_keyspace.ranges {
                    let mut gap_key = gap_range.start;
                    while gap_key != gap_range.end {
                        let meta = batch
                            .metadata
                            .iter()
                            .find(|meta| (meta.key(), meta.lsn()) == (gap_key.to_compact(), *lsn))
                            .unwrap();
                        let meta = match meta {
                            ValueMeta::Serialized(ser) => ser,
                            ValueMeta::Observed(_) => unreachable!(),
                        };

                        let zero_value = Value::Image(ZERO_PAGE.clone());
                        let zero_value_size = zero_value.serialized_size().unwrap() as usize;

                        assert_eq!(meta.len, zero_value_size);
                        assert_eq!(meta.will_init, zero_value.will_init());

                        let start = meta.batch_offset as usize;
                        let end = meta.batch_offset as usize + meta.len;
                        let value_from_batch = Value::des(&batch.raw[start..end]).unwrap();
                        assert_eq!(value_from_batch, zero_value);

                        gap_pages_count += 1;
                        expected_buffer_size += zero_value_size;
                        gap_key = gap_key.next();
                    }
                }
            }
        }

        // Invariant 3: The length of the batch is equal to the number
        // of values inserted, plus the number of gap pages. This extends
        // to the raw buffer size.
        assert_eq!(batch.len(), values.len() + gap_pages_count);
        assert_eq!(expected_buffer_size, batch.buffer_size());

        // Invariant 4: Metadata entries for any given key are sorted in LSN order.
        batch.validate_lsn_order();
    }

    #[test]
    fn test_creation_from_values() {
        const LSN: Lsn = Lsn(0x10);
        let key = Key::from_hex("110000000033333333444444445500000001").unwrap();

        let values = vec![
            (
                key.to_compact(),
                LSN,
                Value::WalRecord(NeonWalRecord::wal_append("foo")),
            ),
            (
                key.next().to_compact(),
                LSN,
                Value::WalRecord(NeonWalRecord::wal_append("bar")),
            ),
            (
                key.to_compact(),
                Lsn(LSN.0 + 0x10),
                Value::WalRecord(NeonWalRecord::wal_append("baz")),
            ),
            (
                key.next().next().to_compact(),
                LSN,
                Value::WalRecord(NeonWalRecord::wal_append("taz")),
            ),
        ];

        let values = values
            .into_iter()
            .map(|(key, lsn, value)| (key, lsn, value.serialized_size().unwrap() as usize, value))
            .collect::<Vec<_>>();
        let batch = SerializedValueBatch::from_values(values.clone());

        validate_batch(&batch, &values, None);

        assert!(!batch.is_empty());
    }

    #[test]
    fn test_put() {
        const LSN: Lsn = Lsn(0x10);
        let key = Key::from_hex("110000000033333333444444445500000001").unwrap();

        let values = vec![
            (
                key.to_compact(),
                LSN,
                Value::WalRecord(NeonWalRecord::wal_append("foo")),
            ),
            (
                key.next().to_compact(),
                LSN,
                Value::WalRecord(NeonWalRecord::wal_append("bar")),
            ),
        ];

        let mut values = values
            .into_iter()
            .map(|(key, lsn, value)| (key, lsn, value.serialized_size().unwrap() as usize, value))
            .collect::<Vec<_>>();
        let mut batch = SerializedValueBatch::from_values(values.clone());

        validate_batch(&batch, &values, None);

        let value = (
            key.to_compact(),
            Lsn(LSN.0 + 0x10),
            Value::WalRecord(NeonWalRecord::wal_append("baz")),
        );
        let serialized_size = value.2.serialized_size().unwrap() as usize;
        let value = (value.0, value.1, serialized_size, value.2);
        values.push(value.clone());
        batch.put(value.0, value.3, value.1);

        validate_batch(&batch, &values, None);

        let value = (
            key.next().next().to_compact(),
            LSN,
            Value::WalRecord(NeonWalRecord::wal_append("taz")),
        );
        let serialized_size = value.2.serialized_size().unwrap() as usize;
        let value = (value.0, value.1, serialized_size, value.2);
        values.push(value.clone());
        batch.put(value.0, value.3, value.1);

        validate_batch(&batch, &values, None);
    }

    #[test]
    fn test_extension() {
        const LSN: Lsn = Lsn(0x10);
        let key = Key::from_hex("110000000033333333444444445500000001").unwrap();

        let values = vec![
            (
                key.to_compact(),
                LSN,
                Value::WalRecord(NeonWalRecord::wal_append("foo")),
            ),
            (
                key.next().to_compact(),
                LSN,
                Value::WalRecord(NeonWalRecord::wal_append("bar")),
            ),
            (
                key.next().next().to_compact(),
                LSN,
                Value::WalRecord(NeonWalRecord::wal_append("taz")),
            ),
        ];

        let mut values = values
            .into_iter()
            .map(|(key, lsn, value)| (key, lsn, value.serialized_size().unwrap() as usize, value))
            .collect::<Vec<_>>();
        let mut batch = SerializedValueBatch::from_values(values.clone());

        let other_values = vec![
            (
                key.to_compact(),
                Lsn(LSN.0 + 0x10),
                Value::WalRecord(NeonWalRecord::wal_append("foo")),
            ),
            (
                key.next().to_compact(),
                Lsn(LSN.0 + 0x10),
                Value::WalRecord(NeonWalRecord::wal_append("bar")),
            ),
            (
                key.next().next().to_compact(),
                Lsn(LSN.0 + 0x10),
                Value::WalRecord(NeonWalRecord::wal_append("taz")),
            ),
        ];

        let other_values = other_values
            .into_iter()
            .map(|(key, lsn, value)| (key, lsn, value.serialized_size().unwrap() as usize, value))
            .collect::<Vec<_>>();
        let other_batch = SerializedValueBatch::from_values(other_values.clone());

        values.extend(other_values);
        batch.extend(other_batch);

        validate_batch(&batch, &values, None);
    }

    #[test]
    fn test_gap_zeroing() {
        const LSN: Lsn = Lsn(0x10);
        let rel_foo_base_key = Key::from_hex("110000000033333333444444445500000001").unwrap();

        let rel_bar_base_key = {
            let mut key = rel_foo_base_key;
            key.field4 += 1;
            key
        };

        let values = vec![
            (
                rel_foo_base_key.to_compact(),
                LSN,
                Value::WalRecord(NeonWalRecord::wal_append("foo1")),
            ),
            (
                rel_foo_base_key.add(1).to_compact(),
                LSN,
                Value::WalRecord(NeonWalRecord::wal_append("foo2")),
            ),
            (
                rel_foo_base_key.add(5).to_compact(),
                LSN,
                Value::WalRecord(NeonWalRecord::wal_append("foo3")),
            ),
            (
                rel_foo_base_key.add(1).to_compact(),
                Lsn(LSN.0 + 0x10),
                Value::WalRecord(NeonWalRecord::wal_append("foo4")),
            ),
            (
                rel_foo_base_key.add(10).to_compact(),
                Lsn(LSN.0 + 0x10),
                Value::WalRecord(NeonWalRecord::wal_append("foo5")),
            ),
            (
                rel_foo_base_key.add(11).to_compact(),
                Lsn(LSN.0 + 0x10),
                Value::WalRecord(NeonWalRecord::wal_append("foo6")),
            ),
            (
                rel_foo_base_key.add(12).to_compact(),
                Lsn(LSN.0 + 0x10),
                Value::WalRecord(NeonWalRecord::wal_append("foo7")),
            ),
            (
                rel_bar_base_key.to_compact(),
                LSN,
                Value::WalRecord(NeonWalRecord::wal_append("bar1")),
            ),
            (
                rel_bar_base_key.add(4).to_compact(),
                LSN,
                Value::WalRecord(NeonWalRecord::wal_append("bar2")),
            ),
        ];

        let values = values
            .into_iter()
            .map(|(key, lsn, value)| (key, lsn, value.serialized_size().unwrap() as usize, value))
            .collect::<Vec<_>>();

        let mut batch = SerializedValueBatch::from_values(values.clone());

        let gaps = vec![
            (
                KeySpace {
                    ranges: vec![
                        rel_foo_base_key.add(2)..rel_foo_base_key.add(5),
                        rel_bar_base_key.add(1)..rel_bar_base_key.add(4),
                    ],
                },
                LSN,
            ),
            (
                KeySpace {
                    ranges: vec![rel_foo_base_key.add(6)..rel_foo_base_key.add(10)],
                },
                Lsn(LSN.0 + 0x10),
            ),
        ];

        batch.zero_gaps(gaps.clone());
        validate_batch(&batch, &values, Some(&gaps));
    }

    /// OrioleDB FPI (ORIOLEDB_RMGR_ID = 129) round-trip: feed a
    /// hand-built DecodedWALRecord into from_decoded_filtered and
    /// verify that
    ///   1. the block produces a Value::Image under rel_block_to_key,
    ///   2. the full record is mirrored as a Value::WalRecord at
    ///      orioledb_wal_key(lsn),
    ///   3. page_set_lsn is NOT applied — OrioleDB pages carry an
    ///      OrioleDBPageHeader at offset 0, so overwriting the first
    ///      8 bytes with a PG LSN would corrupt the state field.
    #[test]
    fn test_orioledb_fpi_round_trip() {
        use crate::models::FlushUncommittedRecords;

        const LSN: Lsn = Lsn(0x1000);
        const SPC: u32 = 1663;
        const DB: u32 = 24577;
        const REL: u32 = 24578;
        const BLKNO: u32 = 7;

        // Build an 8KB OrioleDB page. The first 8 bytes double as the
        // OrioleDBPageHeader state/pageChangeCount/checkpointNum prefix
        // and MUST remain byte-identical after the decode round-trip.
        let mut page = vec![0u8; BLCKSZ as usize];
        page[0..8].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE]);
        page[2048] = 0x42;
        page[BLCKSZ as usize - 1] = 0xFF;
        let record_bytes = Bytes::from(page.clone());

        let mut block = DecodedBkpBlock::new();
        block.rnode_spcnode = SPC;
        block.rnode_dbnode = DB;
        block.rnode_relnode = REL;
        block.forknum = 0;
        block.blkno = BLKNO;
        block.has_image = true;
        block.apply_image = true;
        block.will_init = true;
        block.bimg_offset = 0;
        block.bimg_len = BLCKSZ;

        let decoded = DecodedWALRecord {
            xl_xid: 42,
            xl_info: 0x10,
            xl_rmid: 129,
            record: record_bytes.clone(),
            blocks: vec![block],
            main_data_offset: 0,
            origin_id: 0,
        };

        let shard = ShardIdentity::unsharded();
        let mut shard_records: HashMap<ShardIdentity, InterpretedWalRecord> = HashMap::new();
        shard_records.insert(
            shard,
            InterpretedWalRecord {
                metadata_record: None,
                batch: SerializedValueBatch::default(),
                next_record_lsn: LSN,
                flush_uncommitted: FlushUncommittedRecords::No,
                xid: 42,
            },
        );

        SerializedValueBatch::from_decoded_filtered(
            decoded,
            &mut shard_records,
            LSN,
            PgMajorVersion::PG17,
        )
        .unwrap();

        let batch = shard_records.remove(&shard).unwrap().batch;
        assert_eq!(batch.len, 2, "expected FPI image + WAL-stream entry");
        assert_eq!(batch.max_lsn, LSN);

        let rel = RelTag {
            spcnode: SPC,
            dbnode: DB,
            relnode: REL,
            forknum: 0,
        };
        let block_key = rel_block_to_key(rel, BLKNO).to_compact();
        let wal_key = orioledb_wal_key(LSN.0).to_compact();

        let mut saw_image = false;
        let mut saw_wal_record = false;
        for meta in &batch.metadata {
            let ser = match meta {
                ValueMeta::Serialized(s) => s,
                ValueMeta::Observed(_) => panic!("did not expect observed entries"),
            };
            let start = ser.batch_offset as usize;
            let val = Value::des(&batch.raw[start..start + ser.len]).unwrap();

            if ser.key == block_key {
                let img = match val {
                    Value::Image(b) => b,
                    other => panic!("expected Value::Image at block key, got {other:?}"),
                };
                assert_eq!(img.len(), BLCKSZ as usize);
                assert_eq!(
                    &img[0..8],
                    &page[0..8],
                    "page_set_lsn leaked into rmid=129 path — would corrupt OrioleDBPageHeader"
                );
                assert_eq!(img[2048], 0x42);
                assert_eq!(img[BLCKSZ as usize - 1], 0xFF);
                saw_image = true;
            } else if ser.key == wal_key {
                match val {
                    Value::WalRecord(NeonWalRecord::Postgres { will_init, rec }) => {
                        assert!(will_init, "OrioleDB WAL-stream entries must init");
                        assert_eq!(rec.as_ref(), record_bytes.as_ref());
                        saw_wal_record = true;
                    }
                    other => {
                        panic!("expected Postgres WalRecord at WAL key, got {other:?}")
                    }
                }
            } else {
                panic!("unexpected key in batch: {}", Key::from_compact(ser.key));
            }
        }
        assert!(saw_image, "FPI page image missing from batch");
        assert!(saw_wal_record, "OrioleDB WAL-stream record missing from batch");
    }

    /// OrioleDB delta / container records carry no block refs. They
    /// must still land in the WAL-stream key space unconditionally so
    /// wal-redo can replay the tree-level log on GetPage.
    #[test]
    fn test_orioledb_record_without_blocks() {
        use crate::models::FlushUncommittedRecords;

        const LSN: Lsn = Lsn(0x2000);
        let record_bytes = Bytes::from_static(b"\x01\x02\x03opaque oriole payload");

        let decoded = DecodedWALRecord {
            xl_xid: 99,
            xl_info: 0x00,
            xl_rmid: 129,
            record: record_bytes.clone(),
            blocks: vec![],
            main_data_offset: 0,
            origin_id: 0,
        };

        let shard = ShardIdentity::unsharded();
        let mut shard_records: HashMap<ShardIdentity, InterpretedWalRecord> = HashMap::new();
        shard_records.insert(
            shard,
            InterpretedWalRecord {
                metadata_record: None,
                batch: SerializedValueBatch::default(),
                next_record_lsn: LSN,
                flush_uncommitted: FlushUncommittedRecords::No,
                xid: 99,
            },
        );

        SerializedValueBatch::from_decoded_filtered(
            decoded,
            &mut shard_records,
            LSN,
            PgMajorVersion::PG17,
        )
        .unwrap();

        let batch = shard_records.remove(&shard).unwrap().batch;
        assert_eq!(batch.len, 1);
        assert_eq!(batch.max_lsn, LSN);

        let meta = match &batch.metadata[0] {
            ValueMeta::Serialized(s) => s,
            _ => panic!("serialized expected"),
        };
        assert_eq!(meta.key, orioledb_wal_key(LSN.0).to_compact());
        assert!(meta.will_init);

        let val =
            Value::des(&batch.raw[meta.batch_offset as usize..][..meta.len]).unwrap();
        match val {
            Value::WalRecord(NeonWalRecord::Postgres { will_init, rec }) => {
                assert!(will_init);
                assert_eq!(rec.as_ref(), record_bytes.as_ref());
            }
            other => panic!("expected WalRecord(Postgres), got {other:?}"),
        }
    }

    /// Phase 6.6.3: LSN externality invariant.
    ///
    /// PageServer addresses OrioleDB pages by (key, LSN) tuples held
    /// in batch metadata, *not* by LSN bytes embedded inside the page.
    /// This test locks in that invariant by emitting the same logical
    /// page at two different LSNs and verifying:
    ///
    ///   1. The two emissions produce two independently addressable
    ///      metadata entries that share the block key but carry the
    ///      distinct LSNs that were supplied.
    ///   2. The serialized `Value::Image` bytes are byte-identical
    ///      across the two LSNs — no `page_set_lsn` ever overwrites
    ///      any part of the page.
    ///   3. The `OrioleDBPageHeader` prefix (bytes 0..8: state,
    ///      pageChangeCount, checkpointNum) survives both emissions
    ///      without mutation.
    ///
    /// If this assertion ever regresses, Neon's timeline machinery
    /// (branching, PITR) has started relying on in-page LSNs for
    /// OrioleDB pages — which is incompatible with OrioleDB's page
    /// header layout and will silently corrupt the state prefix.
    ///
    /// There is no conflict between Neon's LSN and OrioleDB's CSN
    /// (`checkpointNum`): LSN is a per-byte WAL offset tracked
    /// externally by PageServer, CSN is an in-page epoch counter
    /// used by OrioleDB for MVCC snapshots. The two coexist because
    /// the LSN never enters the page body.
    #[test]
    fn test_orioledb_page_lsn_externality() {
        use crate::models::FlushUncommittedRecords;

        const SPC: u32 = 1663;
        const DB: u32 = 24577;
        const REL: u32 = 24578;
        const BLKNO: u32 = 42;
        const LSN_A: Lsn = Lsn(0x1000);
        const LSN_B: Lsn = Lsn(0x8000);

        // Build a page whose first 8 bytes are a distinctive
        // OrioleDBPageHeader prefix: if any LSN write leaked into
        // the page, the lower 8 bytes would be clobbered.
        let mut page = vec![0u8; BLCKSZ as usize];
        page[0..8].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
        page[4096] = 0xAB;
        page[BLCKSZ as usize - 1] = 0xCD;
        let page_bytes = Bytes::from(page.clone());

        let emit = |lsn: Lsn| -> SerializedValueBatch {
            let mut block = DecodedBkpBlock::new();
            block.rnode_spcnode = SPC;
            block.rnode_dbnode = DB;
            block.rnode_relnode = REL;
            block.forknum = 0;
            block.blkno = BLKNO;
            block.has_image = true;
            block.apply_image = true;
            block.will_init = true;
            block.bimg_offset = 0;
            block.bimg_len = BLCKSZ;

            let decoded = DecodedWALRecord {
                xl_xid: 7,
                xl_info: 0x10,
                xl_rmid: 129,
                record: page_bytes.clone(),
                blocks: vec![block],
                main_data_offset: 0,
                origin_id: 0,
            };

            let shard = ShardIdentity::unsharded();
            let mut shard_records: HashMap<ShardIdentity, InterpretedWalRecord> =
                HashMap::new();
            shard_records.insert(
                shard,
                InterpretedWalRecord {
                    metadata_record: None,
                    batch: SerializedValueBatch::default(),
                    next_record_lsn: lsn,
                    flush_uncommitted: FlushUncommittedRecords::No,
                    xid: 7,
                },
            );

            SerializedValueBatch::from_decoded_filtered(
                decoded,
                &mut shard_records,
                lsn,
                PgMajorVersion::PG17,
            )
            .unwrap();

            shard_records.remove(&shard).unwrap().batch
        };

        let batch_a = emit(LSN_A);
        let batch_b = emit(LSN_B);

        let rel = RelTag {
            spcnode: SPC,
            dbnode: DB,
            relnode: REL,
            forknum: 0,
        };
        let block_key = rel_block_to_key(rel, BLKNO).to_compact();

        // Extract the FPI image entry from each batch and capture
        // its metadata LSN.
        let extract_image = |batch: &SerializedValueBatch| -> (Lsn, Bytes) {
            let mut found: Option<(Lsn, Bytes)> = None;
            for meta in &batch.metadata {
                let ser = match meta {
                    ValueMeta::Serialized(s) => s,
                    ValueMeta::Observed(_) => continue,
                };
                if ser.key != block_key {
                    continue;
                }
                let val = Value::des(&batch.raw[ser.batch_offset as usize..][..ser.len])
                    .unwrap();
                let img = match val {
                    Value::Image(b) => b,
                    other => panic!("expected Value::Image at block key, got {other:?}"),
                };
                assert!(found.is_none(), "more than one image entry per batch");
                found = Some((ser.lsn, img));
            }
            found.expect("no FPI image entry found at block_key")
        };

        let (lsn_a_meta, image_a) = extract_image(&batch_a);
        let (lsn_b_meta, image_b) = extract_image(&batch_b);

        // Invariant 1: the metadata carries the LSN we emitted.
        assert_eq!(lsn_a_meta, LSN_A);
        assert_eq!(lsn_b_meta, LSN_B);
        assert_ne!(
            lsn_a_meta, lsn_b_meta,
            "two emissions at distinct LSNs should produce distinct metadata LSNs"
        );

        // Invariant 2: the page body is byte-identical across the
        // two LSNs — no LSN write ever touched the image.
        assert_eq!(
            image_a.len(),
            BLCKSZ as usize,
            "image A length must equal BLCKSZ"
        );
        assert_eq!(
            image_b.len(),
            BLCKSZ as usize,
            "image B length must equal BLCKSZ"
        );
        assert_eq!(
            image_a, image_b,
            "OrioleDB page body must be byte-identical across LSN emissions; \
             a diff implies page_set_lsn leaked into the rmid=129 path"
        );

        // Invariant 3: the OrioleDBPageHeader prefix survived both
        // emissions. We already checked image_a == image_b, so one
        // assertion on image_a covers both.
        assert_eq!(
            &image_a[0..8],
            &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
            "OrioleDBPageHeader prefix was mutated — LSN or some other \
             header byte leaked into the page body"
        );
        assert_eq!(image_a[4096], 0xAB);
        assert_eq!(image_a[BLCKSZ as usize - 1], 0xCD);
    }
}
