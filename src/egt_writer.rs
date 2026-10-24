//! Retains compressed sub-tables in memory until publication in file order.

use std::io::{Read, Write};

use bytes::Bytes;
use object_store::{buffered::BufWriter, path::Path as ObjectPath};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

#[cfg(test)]
use crate::egt_file::DEFAULT_COMPRESSION_LEVEL;
use crate::egt_file::{EgtFile, MaybeDtcOutcome, transpose_frame};
use crate::error::{EgtError, EgtResult};
use crate::storage::{Location, block_on};

/// A frame-aligned compressed batch, without a serialized seek-table footer.
pub(crate) struct CompressedSegment {
    pub(crate) data: Bytes,
    pub(crate) seek_table: zeekstd::SeekTable,
}

pub(crate) struct EgtFileWriter {
    location: Location,
    key: ObjectPath,
    frame_size: usize,
    lengths: Vec<usize>,
    tables: Vec<Option<Vec<CompressedSegment>>>,
    footer: Option<Bytes>,
    published: bool,
}

impl EgtFileWriter {
    pub(crate) fn new(file: &EgtFile) -> EgtResult<Self> {
        if file.frame_size == 0 || file.frame_size > (u32::MAX as usize) / 2 {
            return Err(EgtError::Internal("invalid writer frame size"));
        }
        file.location.ensure_writable()?;
        Ok(Self {
            location: file.location.clone(),
            key: file.key.clone(),
            frame_size: file.frame_size,
            lengths: file.egts.iter().map(|egt| egt.index_range()).collect(),
            tables: (0..file.egts.len()).map(|_| None).collect(),
            footer: None,
            published: false,
        })
    }

    fn writable_table_length(&self, egt_idx: usize) -> EgtResult<usize> {
        if self.footer.is_some() || self.published {
            return Err(EgtError::Internal("writer is already finished"));
        }
        let length = *self.lengths.get(egt_idx).ok_or(EgtError::IndexOutOfRange {
            index: egt_idx,
            range: self.lengths.len(),
        })?;
        if self.tables[egt_idx].is_some() {
            return Err(EgtError::Internal("table has already been written"));
        }
        Ok(length)
    }

    /// Retains batches in caller-supplied frame order without concatenating them.
    pub(crate) fn write_segments(
        &mut self,
        egt_idx: usize,
        segments: Vec<CompressedSegment>,
    ) -> EgtResult<()> {
        let length = self.writable_table_length(egt_idx)?;
        let expected_frames = length.div_ceil(self.frame_size);
        let mut frames = 0usize;
        let mut remaining = length;
        for segment in &segments {
            let num_frames = segment.seek_table.num_frames();
            if num_frames == 0 && !(length == 0 && segments.len() == 1 && segment.data.is_empty()) {
                return Err(EgtError::Internal("compressed segment has no frames"));
            }
            let mut compressed_length = 0u64;
            for index in 0..num_frames {
                let compressed = segment.seek_table.frame_size_comp(index)?;
                let decompressed = segment.seek_table.frame_size_decomp(index)?;
                let frame_length = remaining.min(self.frame_size);
                if compressed == 0 || frame_length == 0 || decompressed != (frame_length * 2) as u64
                {
                    return Err(EgtError::Internal(
                        "compressed segment has invalid frame lengths",
                    ));
                }
                compressed_length += compressed;
                remaining -= frame_length;
                frames += 1;
            }
            if compressed_length != segment.data.len() as u64 {
                return Err(EgtError::Internal(
                    "compressed segment has wrong data length",
                ));
            }
        }
        if frames != expected_frames || remaining != 0 {
            return Err(EgtError::Internal("compressed table has wrong frame count"));
        }
        self.tables[egt_idx] = Some(segments);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn write_table(
        &mut self,
        egt_idx: usize,
        values: &[MaybeDtcOutcome],
    ) -> EgtResult<()> {
        let length = self.writable_table_length(egt_idx)?;
        if values.len() != length {
            return Err(EgtError::Internal("finalized table has wrong length"));
        }
        if values.iter().any(|value| value.is_unknown()) {
            return Err(EgtError::Internal(
                "finalized table contains unknown outcomes",
            ));
        }

        let mut data = Vec::new();
        let mut encoder = zeekstd::EncodeOptions::new()
            .compression_level(DEFAULT_COMPRESSION_LEVEL)
            .into_encoder(&mut data)?;
        for chunk in values.chunks(self.frame_size) {
            encode_frame(&mut encoder, chunk)?;
        }
        // into_seek_table consumes the encoder without flushing its output buffer.
        encoder.flush()?;
        let seek_table = encoder.into_seek_table();
        self.write_segments(
            egt_idx,
            vec![CompressedSegment {
                data: data.into(),
                seek_table,
            }],
        )
    }

    /// Validates completeness and prepares the seek table without publishing.
    pub(crate) fn finish(&mut self) -> EgtResult<()> {
        if self.published {
            return Err(EgtError::Internal("writer is already published"));
        }
        if self.footer.is_some() {
            return Ok(());
        }
        let mut table = zeekstd::SeekTable::new();
        for compressed in &self.tables {
            let compressed = compressed
                .as_ref()
                .ok_or(EgtError::Internal("cannot finish with missing tables"))?;
            for segment in compressed {
                for index in 0..segment.seek_table.num_frames() {
                    table.log_frame(
                        segment.seek_table.frame_size_comp(index)? as u32,
                        segment.seek_table.frame_size_decomp(index)? as u32,
                    )?;
                }
            }
        }
        let mut footer = Vec::new();
        table.into_serializer().read_to_end(&mut footer)?;
        self.footer = Some(footer.into());
        Ok(())
    }

    /// Publishes in canonical order, then releases the compressed buffers.
    pub(crate) fn publish(&mut self) -> EgtResult<(u64, String)> {
        if self.published {
            return Err(EgtError::Internal("writer is already published"));
        }
        let footer = self.footer.as_ref().ok_or(EgtError::Internal(
            "writer must be finished before publication",
        ))?;
        let buffers = self
            .tables
            .iter()
            .flatten()
            .flatten()
            .map(|segment| segment.data.clone())
            .chain(std::iter::once(footer.clone()));
        let result = publish_buffers(&self.location, &self.key, buffers)?;
        self.tables.clear();
        self.footer = None;
        self.published = true;
        Ok(result)
    }
}

const UPLOAD_BUFFER_SIZE: usize = 8 * 1024 * 1024;

/// Publishes owned buffers without concatenating them into another full artifact.
pub(crate) fn publish_buffers(
    location: &Location,
    key: &ObjectPath,
    buffers: impl IntoIterator<Item = Bytes>,
) -> EgtResult<(u64, String)> {
    location.ensure_writable()?;
    block_on(async {
        let mut writer =
            BufWriter::with_capacity(location.store.clone(), key.clone(), UPLOAD_BUFFER_SIZE)
                .with_max_concurrency(2);
        let mut hash = Sha256::new();
        let mut length = 0;
        for buffer in buffers {
            // BufWriter applies backpressure per put, not per internally spawned part.
            for start in (0..buffer.len()).step_by(UPLOAD_BUFFER_SIZE) {
                let chunk = buffer.slice(start..buffer.len().min(start + UPLOAD_BUFFER_SIZE));
                hash.update(&chunk);
                length += chunk.len() as u64;
                if let Err(error) = writer.put(chunk).await {
                    let _ = writer.abort().await;
                    return Err(location.error(key, error));
                }
            }
        }
        // shutdown completes publication and delegates completion-error cleanup
        // to object_store; abort cannot be called after this consumes its upload.
        writer.shutdown().await.map_err(|error| EgtError::Storage {
            location: location.describe(key),
            source: object_store::Error::Generic {
                store: "tablebase publication",
                source: Box::new(error),
            },
        })?;
        Ok((length, format!("{:x}", hash.finalize())))
    })
}

pub(crate) fn encode_frame<W: Write>(
    encoder: &mut zeekstd::Encoder<'_, W>,
    values: &[MaybeDtcOutcome],
) -> EgtResult<()> {
    encoder.compress(&transpose_frame(values))?;
    encoder.end_frame()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::ObjectStoreExt;
    use std::fs;

    #[test]
    fn mixed_segments_match_serial_encoder_bytes() {
        let dir = crate::TestDir::new("writer_mixed_segments");
        let file = EgtFile::new(&dir.0, "K_K").unwrap();
        let mut writer = EgtFileWriter::new(&file).unwrap();
        writer.frame_size = 8;
        let values: Vec<_> = (0..file.egts[0].index_range())
            .map(|i| [MaybeDtcOutcome::DRAW, MaybeDtcOutcome::INVALID][i % 2])
            .collect();
        let options = || zeekstd::EncodeOptions::new().compression_level(DEFAULT_COMPRESSION_LEVEL);
        let mut expected = Vec::new();
        let mut serial = options().into_encoder(&mut expected).unwrap();
        for frame in values.chunks(writer.frame_size) {
            serial.compress(&transpose_frame(frame)).unwrap();
            serial.end_frame().unwrap();
        }
        serial.flush().unwrap();
        let table = serial.into_seek_table();
        table.into_serializer().read_to_end(&mut expected).unwrap();
        let mut segments = Vec::new();
        // One-frame, multi-frame, and short-tail batches share the same footer.
        for batch in values[..8].chunks(8).chain(values[8..].chunks(24)) {
            let mut data = Vec::new();
            let mut encoder = options().into_encoder(&mut data).unwrap();
            for frame in batch.chunks(writer.frame_size) {
                encode_frame(&mut encoder, frame).unwrap();
            }
            encoder.flush().unwrap();
            let seek_table = encoder.into_seek_table();
            segments.push(CompressedSegment {
                data: data.into(),
                seek_table,
            });
        }
        writer.write_segments(0, segments).unwrap();
        writer.finish().unwrap();
        let (bytes, sha256) = writer.publish().unwrap();
        assert_eq!(bytes, expected.len() as u64);
        assert_eq!(sha256, format!("{:x}", Sha256::digest(&expected)));
        assert_eq!(fs::read(dir.0.join("K_K.ggegt")).unwrap(), expected);
    }

    #[test]
    fn publication_roundtrip_in_memory() {
        let location = Location::parse("memory:///tables").unwrap();
        let key = location.prefix.clone().join("K_K.ggegt");
        for length in [
            0,
            17,
            UPLOAD_BUFFER_SIZE - 1,
            UPLOAD_BUFFER_SIZE,
            UPLOAD_BUFFER_SIZE + 17,
            2 * UPLOAD_BUFFER_SIZE,
        ] {
            let data: Bytes = (0..length)
                .map(|index| (index % 251) as u8)
                .collect::<Vec<_>>()
                .into();
            // Split buffers independently of upload boundaries, like sub-tables + footer.
            let split = length / 3;
            let buffers = [data.slice(..split), data.slice(split..)];
            let (bytes, sha256) = publish_buffers(&location, &key, buffers).unwrap();
            assert_eq!(bytes, data.len() as u64);
            assert_eq!(sha256, format!("{:x}", Sha256::digest(&data)));
            let uploaded = block_on(async {
                location
                    .store
                    .get(&key)
                    .await
                    .unwrap()
                    .bytes()
                    .await
                    .unwrap()
            });
            assert_eq!(uploaded, data);
        }
    }

    #[test]
    fn reverse_order_and_short_tails_load_via_reader() {
        let dir = crate::TestDir::new("writer_reverse_order");
        let file = EgtFile::new(&dir.0, "KP_K").unwrap();
        assert!(
            file.egts
                .iter()
                .all(|egt| egt.index_range() % file.frame_size != 0)
        );
        let mut writer = EgtFileWriter::new(&file).unwrap();
        // Independent canonical encoding provides a byte-for-byte reference.
        let mut expected = Vec::new();
        let mut encoder = zeekstd::EncodeOptions::new()
            .compression_level(DEFAULT_COMPRESSION_LEVEL)
            .into_encoder(&mut expected)
            .unwrap();
        for idx in 0..file.egts.len() {
            let value = if idx % 2 == 0 {
                MaybeDtcOutcome::DRAW
            } else {
                MaybeDtcOutcome::INVALID
            };
            let values = vec![value; file.egts[idx].index_range()];
            for chunk in values.chunks(file.frame_size) {
                encode_frame(&mut encoder, chunk).unwrap();
            }
        }
        encoder.flush().unwrap();
        let table = encoder.into_seek_table();
        table.into_serializer().read_to_end(&mut expected).unwrap();
        for idx in (0..file.egts.len()).rev() {
            let value = if idx % 2 == 0 {
                MaybeDtcOutcome::DRAW
            } else {
                MaybeDtcOutcome::INVALID
            };
            writer
                .write_table(idx, &vec![value; file.egts[idx].index_range()])
                .unwrap();
        }
        writer.finish().unwrap();
        writer.finish().unwrap();
        let destination = dir.0.join("KP_K.ggegt");
        assert!(!destination.exists());
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
        let (bytes, sha256) = writer.publish().unwrap();
        assert_eq!(bytes, expected.len() as u64);
        assert_eq!(sha256, format!("{:x}", Sha256::digest(&expected)));
        assert_eq!(fs::read(&destination).unwrap(), expected);
        assert!(writer.tables.is_empty());
        assert!(writer.publish().is_err());
        assert!(writer.finish().is_err());
        assert!(writer.write_table(0, &[]).is_err());
        let mut reader = EgtFile::new_from_file(&dir.0, "KP_K").unwrap();
        let mut offset = 0;
        for idx in 0..file.egts.len() {
            let value = if idx % 2 == 0 {
                MaybeDtcOutcome::DRAW
            } else {
                MaybeDtcOutcome::INVALID
            };
            let length = file.egts[idx].index_range();
            for local in [0, length / 2, length - 1] {
                assert_eq!(reader.read_from_index(offset + local).unwrap(), value);
            }
            offset += length;
        }
    }

    #[test]
    fn rejects_invalid_input_and_missing_tables() {
        let dir = crate::TestDir::new("writer_rejections");
        let file = EgtFile::new(&dir.0, "KP_K").unwrap();
        let mut writer = EgtFileWriter::new(&file).unwrap();
        assert!(writer.finish().is_err());
        assert!(writer.publish().is_err());
        assert!(writer.write_table(file.egts.len(), &[]).is_err());
        assert!(writer.write_table(0, &[]).is_err());
        let mut values = vec![MaybeDtcOutcome::DRAW; file.egts[0].index_range()];
        values[0] = MaybeDtcOutcome::new_unknown(1);
        assert!(writer.write_table(0, &values).is_err());
        values[0] = MaybeDtcOutcome::INVALID;
        writer.write_table(0, &values).unwrap();
        assert!(writer.write_table(0, &values).is_err());
        assert!(writer.finish().is_err());
    }

    #[test]
    fn unpublished_writer_preserves_destination_without_temporary_files() {
        let dir = crate::TestDir::new("writer_cleanup");
        let file = EgtFile::new(&dir.0, "K_K").unwrap();
        let destination = dir.0.join("K_K.ggegt");
        fs::write(&destination, b"existing destination").unwrap();
        let mut writer = EgtFileWriter::new(&file).unwrap();
        writer
            .write_table(0, &vec![MaybeDtcOutcome::DRAW; file.egts[0].index_range()])
            .unwrap();
        writer.finish().unwrap();
        drop(writer);
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
        assert_eq!(fs::read(&destination).unwrap(), b"existing destination");
    }
}
