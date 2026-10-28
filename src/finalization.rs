//! Single-table finalization in the caller's Rayon pool, with ordered frames.

use std::io::Write;
use std::sync::atomic::AtomicU16;

use rayon::prelude::*;

use crate::egt::Egt;
use crate::egt_file::{DEFAULT_COMPRESSION_LEVEL, EgtFileReader, MaybeDtcOutcome};
use crate::egt_writer::{CompressedSegment, EgtFileWriter, encode_frame};
use crate::error::{EgtError, EgtResult};
use crate::finalization_stats::{EgtStatsBuilder, TableStats};
use crate::stats::EgtStats;

fn finalize_batch(
    values: &mut [AtomicU16],
    start: usize,
    egt: &Egt,
    frame_size: usize,
    maxima: [u16; 2],
) -> EgtResult<(CompressedSegment, TableStats)> {
    let mut data = Vec::new();
    let mut encoder = zeekstd::EncodeOptions::new()
        .compression_level(DEFAULT_COMPRESSION_LEVEL)
        .into_encoder(&mut data)?;
    let mut plain = Vec::with_capacity(frame_size.min(values.len()));
    let mut stats = TableStats::default();
    for (frame_idx, chunk) in values.chunks_mut(frame_size).enumerate() {
        plain.clear();
        for (offset, cell) in chunk.iter_mut().enumerate() {
            // Propagation has joined; exclusive access needs no atomic load/CAS.
            let raw = MaybeDtcOutcome::from_u16(*cell.get_mut());
            let outcome = if raw.is_unknown() {
                MaybeDtcOutcome::DRAW
            } else {
                raw
            };
            stats.record_outcome(
                outcome,
                start + frame_idx * frame_size + offset,
                egt,
                maxima[0],
                maxima[1],
            );
            plain.push(outcome);
        }
        encode_frame(&mut encoder, &plain)?;
    }
    // into_seek_table does not flush, and finish would append a per-batch footer.
    encoder.flush()?;
    let seek_table = encoder.into_seek_table();
    Ok((
        CompressedSegment {
            data: data.into(),
            seek_table,
        },
        stats,
    ))
}

/// Resolves unknowns to draws, computes statistics, and publishes one table and
/// its JSON sidecar. Call only after all propagation workers have joined.
/// Binary and sidecar publication are not an atomic multi-object transaction.
pub(crate) fn finalize_table(
    file: &EgtFileReader,
    mut values: Vec<AtomicU16>,
    maxima: [u16; 2],
) -> EgtResult<EgtStats> {
    if values.len() != file.egt.index_range() {
        return Err(EgtError::Internal("completed table has wrong length"));
    }
    let mut writer = EgtFileWriter::new(file)?;
    let frames = values.len().div_ceil(file.frame_size);
    let batch_size = frames.div_ceil(rayon::current_num_threads()).max(1) * file.frame_size;
    // Contiguous batches reuse one encoder each. Indexed collection preserves
    // frame and example order, independently of worker completion order.
    let batches: Vec<_> = values
        .par_chunks_mut(batch_size)
        .enumerate()
        .map(|(idx, batch)| {
            finalize_batch(batch, idx * batch_size, &file.egt, file.frame_size, maxima)
        })
        .collect();
    drop(values);
    let batches = batches.into_iter().collect::<EgtResult<Vec<_>>>()?;
    let (segments, stats): (Vec<_>, Vec<_>) = batches.into_iter().unzip();
    let mut builder = EgtStatsBuilder::new(maxima);
    builder.merge(stats);
    writer.write_segments(segments)?;
    writer.finish()?;
    let (bytes, sha256) = writer.publish()?;
    let stats = builder.build(file, bytes, sha256);
    file.save_stats(&stats)?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConversionType;

    #[test]
    fn parallel_batches_match_serial_bytes_and_stats() {
        let mut results = Vec::new();
        for threads in [1, 4, 8] {
            let dir = crate::TestDir::new(&format!("finalization_t{threads}"));
            let file = EgtFileReader::new(&dir.0, "KQ_KR").unwrap();
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            pool.install(|| {
                let values = (0..file.egt.index_range())
                    .map(|i| {
                        let outcome = match i % 4 {
                            0 => MaybeDtcOutcome::INVALID,
                            1 => MaybeDtcOutcome::new_unknown(17),
                            2 => MaybeDtcOutcome::new_win(ConversionType::Capture, 3),
                            _ => MaybeDtcOutcome::new_loss(ConversionType::Checkmate, 0),
                        };
                        AtomicU16::new(outcome.to_u16())
                    })
                    .collect();
                // Maxima above the synthetic distances avoid large EPD lists.
                let stats = finalize_table(&file, values, [7, 9]).unwrap();
                assert_eq!(stats.tablename, "KQ_KR");
                assert_eq!(
                    stats.unique_positions + stats.invalid_or_redundant,
                    file.index_range
                );
                let mut published = EgtFileReader::new_from_file(&dir.0, "KQ_KR").unwrap();
                assert_eq!(
                    published.read_from_index(0).unwrap(),
                    MaybeDtcOutcome::INVALID
                );
                assert_eq!(published.read_from_index(1).unwrap(), MaybeDtcOutcome::DRAW);
                assert_eq!(
                    crate::stats::read_stats(&file.location, &file.id).unwrap(),
                    stats
                );
                results.push((
                    std::fs::read(dir.0.join(file.id.relative_path("ggegt"))).unwrap(),
                    std::fs::read(dir.0.join(file.id.relative_path("json"))).unwrap(),
                ));
            });
        }
        assert!(results.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn wrong_length_does_not_publish() {
        let dir = crate::TestDir::new("finalization_wrong_length");
        let file = EgtFileReader::new(&dir.0, "K_K").unwrap();
        assert!(matches!(
            finalize_table(&file, Vec::new(), [0, 0]),
            Err(EgtError::Internal("completed table has wrong length"))
        ));
        assert!(!dir.0.join(file.id.relative_path("ggegt")).exists());
        assert!(!dir.0.join(file.id.relative_path("json")).exists());
    }
}
