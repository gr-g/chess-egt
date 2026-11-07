//! Validates and publishes compressed single-table batches in frame order.

use std::io::{Read, Write};

use bytes::Bytes;
use object_store::{buffered::BufWriter, path::Path as ObjectPath};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::transpose_frame;
use crate::PackedOutcome;
use crate::error::{EgtError, EgtResult};
use crate::storage::{Location, block_on};
use crate::table_id::TableId;

pub(crate) const COMPRESSION_LEVEL: i32 = 19;

/// A frame-aligned compressed batch, without a serialized seek-table footer.
pub(crate) struct CompressedSegment {
    pub(crate) data: Bytes,
    pub(crate) seek_table: zeekstd::SeekTable,
}

pub(crate) struct EgtFileWriter {
    location: Location,
    key: ObjectPath,
    frame_size: usize,
    length: usize,
}

impl EgtFileWriter {
    pub(crate) fn new(
        location: &Location,
        id: &TableId,
        length: usize,
        frame_size: usize,
    ) -> EgtResult<Self> {
        if frame_size == 0 || frame_size > (u32::MAX as usize) / 2 {
            return Err(EgtError::Internal("invalid writer frame size"));
        }
        location.ensure_format()?;
        Ok(Self {
            location: location.clone(),
            key: location.key(&id.relative_path("ggegt")),
            frame_size,
            length,
        })
    }

    /// Validates all batches before publishing, without concatenating their buffers.
    /// The caller supplies segments in frame order.
    pub(crate) fn publish(self, segments: Vec<CompressedSegment>) -> EgtResult<(u64, String)> {
        let length = self.length;
        let mut table = zeekstd::SeekTable::new();

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
                table.log_frame(compressed as u32, decompressed as u32)?;
                compressed_length += compressed;
                remaining -= frame_length;
            }
            if compressed_length != segment.data.len() as u64 {
                return Err(EgtError::Internal(
                    "compressed segment has wrong data length",
                ));
            }
        }
        if remaining != 0 {
            return Err(EgtError::Internal("compressed table has wrong frame count"));
        }
        let mut footer = Vec::new();
        table.into_serializer().read_to_end(&mut footer)?;
        let buffers = segments
            .into_iter()
            .map(|segment| segment.data)
            .chain(std::iter::once(footer.into()));
        publish_buffers(&self.location, &self.key, buffers)
    }
}

const UPLOAD_BUFFER_SIZE: usize = 8 * 1024 * 1024;

/// Publishes owned buffers without concatenating them into another full artifact.
fn publish_buffers(
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
    values: &[PackedOutcome],
) -> EgtResult<()> {
    encoder.compress(&transpose_frame(values))?;
    encoder.end_frame()?;
    Ok(())
}

/// Publishes arbitrary outcomes, including deliberately corrupt test data.
#[cfg(test)]
pub(crate) fn write_fixture(
    base: &std::path::Path,
    name: &str,
    outcome: impl Fn(usize) -> PackedOutcome,
) -> crate::egt_file::EgtFileReader {
    use crate::storage::FRAME_SIZE;

    let location = Location::parse(base).unwrap();
    let id = TableId::parse(name).unwrap();
    let length = crate::Egt::from_elements(id.parse_elements())
        .unwrap()
        .index_range();
    location.ensure_format().unwrap();
    let mut data = Vec::new();
    let mut encoder = zeekstd::EncodeOptions::new()
        .compression_level(COMPRESSION_LEVEL)
        .into_encoder(&mut data)
        .unwrap();
    for start in (0..length).step_by(FRAME_SIZE) {
        let values: Vec<_> = (start..length.min(start + FRAME_SIZE))
            .map(&outcome)
            .collect();
        encoder.compress(&transpose_frame(&values)).unwrap();
        if start + values.len() < length {
            encoder.end_frame().unwrap();
        }
    }
    encoder.finish().unwrap();
    let key = location.key(&id.relative_path("ggegt"));
    publish_buffers(&location, &key, [data.into()]).unwrap();
    crate::egt_file::EgtFileReader::open_in(&location, name).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egt_file::EgtFileReader;
    use crate::storage::FRAME_SIZE;
    use object_store::ObjectStoreExt;
    use std::{fs, path::Path};

    fn writer(base: &Path, name: &str) -> EgtFileWriter {
        let id = TableId::parse(name).unwrap();
        let length = crate::Egt::from_elements(id.parse_elements())
            .unwrap()
            .index_range();
        EgtFileWriter::new(&Location::parse(base).unwrap(), &id, length, FRAME_SIZE).unwrap()
    }

    #[test]
    fn mixed_segments_match_serial_encoder_bytes() {
        let dir = crate::TestDir::new("writer_mixed_segments");
        let mut writer = writer(&dir.0, "K_K");
        writer.frame_size = 8;
        let values: Vec<_> = (0..writer.length)
            .map(|i| [PackedOutcome::DRAW, PackedOutcome::INVALID][i % 2])
            .collect();
        let options = || zeekstd::EncodeOptions::new().compression_level(COMPRESSION_LEVEL);
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
        let (bytes, sha256) = writer.publish(segments).unwrap();
        assert_eq!(bytes, expected.len() as u64);
        assert_eq!(sha256, format!("{:x}", Sha256::digest(&expected)));
        assert_eq!(
            fs::read(
                dir.0
                    .join(TableId::parse("K_K").unwrap().relative_path("ggegt"))
            )
            .unwrap(),
            expected
        );
    }

    #[test]
    fn publication_roundtrip_in_memory() {
        let location = Location::parse("memory:///tables").unwrap();
        let key = location.key("2/1v1/op0/0pawns/K_K.ggegt");
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
    fn short_tail_loads_via_reader() {
        let dir = crate::TestDir::new("writer_short_tail");
        let writer = writer(&dir.0, "KQ_KR");
        assert!(writer.length % writer.frame_size != 0);
        let values: Vec<_> = (0..writer.length)
            .map(|index| {
                if index % 2 == 0 {
                    PackedOutcome::DRAW
                } else {
                    PackedOutcome::INVALID
                }
            })
            .collect();
        let mut expected = Vec::new();
        let mut encoder = zeekstd::EncodeOptions::new()
            .compression_level(COMPRESSION_LEVEL)
            .into_encoder(&mut expected)
            .unwrap();
        for chunk in values.chunks(writer.frame_size) {
            encode_frame(&mut encoder, chunk).unwrap();
        }
        encoder.flush().unwrap();
        let seek_table = encoder.into_seek_table();
        let segment = CompressedSegment {
            data: expected.clone().into(),
            seek_table: seek_table.clone(),
        };
        seek_table
            .into_serializer()
            .read_to_end(&mut expected)
            .unwrap();
        let length = writer.length;
        let frame_size = writer.frame_size;
        let destination = dir
            .0
            .join(TableId::parse("KQ_KR").unwrap().relative_path("ggegt"));
        assert!(!destination.exists());
        let (bytes, sha256) = writer.publish(vec![segment]).unwrap();
        assert_eq!(bytes, expected.len() as u64);
        assert_eq!(sha256, format!("{:x}", Sha256::digest(&expected)));
        assert_eq!(fs::read(&destination).unwrap(), expected);

        let mut reader = EgtFileReader::new_from_file(&dir.0, "KQ_KR").unwrap();
        for index in [0, frame_size - 1, frame_size, length - 1] {
            assert_eq!(reader.read_from_index(index).unwrap(), values[index]);
        }
    }

    #[test]
    fn rejects_invalid_segments_and_missing_frames() {
        let dir = crate::TestDir::new("writer_rejections");
        assert!(writer(&dir.0, "KPa_K").publish(Vec::new()).is_err());
        let length = writer(&dir.0, "KPa_K").length;
        for (compressed, decompressed) in [
            (0, length * 2),
            (1, 0),
            (1, length * 2 - 2),
            (1, length * 2 + 2),
        ] {
            let mut seek_table = zeekstd::SeekTable::new();
            seek_table
                .log_frame(compressed, decompressed as u32)
                .unwrap();
            assert!(
                writer(&dir.0, "KPa_K")
                    .publish(vec![CompressedSegment {
                        data: Bytes::from_static(&[0]),
                        seek_table,
                    }])
                    .is_err()
            );
        }
        assert!(
            !dir.0
                .join(TableId::parse("KPa_K").unwrap().relative_path("ggegt"))
                .exists()
        );
    }

    #[test]
    fn invalid_publication_preserves_destination_without_temporary_files() {
        let dir = crate::TestDir::new("writer_cleanup");
        let writer = writer(&dir.0, "K_K");
        let destination = dir
            .0
            .join(TableId::parse("K_K").unwrap().relative_path("ggegt"));
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(&destination, b"existing destination").unwrap();
        let mut table = zeekstd::SeekTable::new();
        table.log_frame(1, (writer.length * 2) as u32).unwrap();
        assert!(
            writer
                .publish(vec![CompressedSegment {
                    data: Bytes::new(),
                    seek_table: table,
                }])
                .is_err()
        );
        assert_eq!(
            fs::read_dir(destination.parent().unwrap()).unwrap().count(),
            1
        );
        assert_eq!(fs::read(&destination).unwrap(), b"existing destination");
    }
}
