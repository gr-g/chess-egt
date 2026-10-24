//! Frame-batched finalization in the solver's Rayon pool, with ordered publication.

use std::io::Write;
use std::sync::atomic::AtomicU16;

use rayon::prelude::*;

use crate::egt::Egt;
use crate::egt_file::{DEFAULT_COMPRESSION_LEVEL, EgtFile, MaybeDtcOutcome};
use crate::egt_writer::{CompressedSegment, EgtFileWriter, encode_frame};
use crate::error::{EgtError, EgtResult};
use crate::finalization_stats::{EgtFileStatsBuilder, TableStats};

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

pub(crate) struct Finalizer {
    artifacts: Vec<(EgtFileWriter, EgtFileStatsBuilder)>,
}

impl Finalizer {
    pub(crate) fn new(files: &[EgtFile]) -> EgtResult<Self> {
        let artifacts = files
            .iter()
            .map(|file| Ok((EgtFileWriter::new(file)?, EgtFileStatsBuilder::default())))
            .collect::<EgtResult<_>>()?;
        Ok(Self { artifacts })
    }

    pub(crate) fn accept_table(
        &mut self,
        file_idx: usize,
        file: &EgtFile,
        egt_idx: usize,
        mut values: Vec<AtomicU16>,
        maxima: [u16; 2],
    ) -> EgtResult<()> {
        let egt = &file.egts[egt_idx];
        if values.len() != egt.index_range() {
            return Err(EgtError::Internal("completed table has wrong length"));
        }
        let frames = values.len().div_ceil(file.frame_size);
        let batch_size = frames.div_ceil(rayon::current_num_threads()).max(1) * file.frame_size;
        // Contiguous batches reuse one encoder each. Indexed collection preserves
        // frame and example order, independently of worker completion order.
        let batches: Vec<_> = values
            .par_chunks_mut(batch_size)
            .enumerate()
            .map(|(idx, batch)| {
                finalize_batch(batch, idx * batch_size, egt, file.frame_size, maxima)
            })
            .collect();
        drop(values);
        let batches = batches.into_iter().collect::<EgtResult<Vec<_>>>()?;
        let (segments, stats): (Vec<_>, Vec<_>) = batches.into_iter().unzip();
        let (writer, builder) = &mut self.artifacts[file_idx];
        writer.write_segments(egt_idx, segments)?;
        builder.merge_table(egt_idx, maxima[0], maxima[1], stats);
        Ok(())
    }

    pub(crate) fn finish_and_publish(mut self, files: &mut [EgtFile]) -> EgtResult<()> {
        // Validate both orientations before publishing either. As before, the
        // tablebases and JSON sidecars are not an atomic multi-file transaction.
        for (writer, _) in &mut self.artifacts {
            writer.finish()?;
        }
        let published = self
            .artifacts
            .iter_mut()
            .map(|(writer, _)| writer.publish())
            .collect::<EgtResult<Vec<_>>>()?;
        for (((_, builder), file), (bytes, sha256)) in
            self.artifacts.into_iter().zip(files).zip(published)
        {
            let stats = builder.build(file, bytes, sha256);
            file.save_stats(&stats)?;
            let mut reader = EgtFile::open_in(&file.location, &file.endgame)?;
            reader.stats = Some(stats);
            *file = reader;
        }
        Ok(())
    }
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
            let file = EgtFile::new(&dir.0, "KQ_KR").unwrap();
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            pool.install(|| {
                let mut finalizer = Finalizer::new(std::slice::from_ref(&file)).unwrap();
                for idx in 0..file.egts.len() {
                    let values = (0..file.egts[idx].index_range())
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
                    finalizer
                        .accept_table(0, &file, idx, values, [7, 9])
                        .unwrap();
                }
                let mut files = [file];
                finalizer.finish_and_publish(&mut files).unwrap();
                assert_eq!(
                    files[0].read_from_index(0).unwrap(),
                    MaybeDtcOutcome::INVALID
                );
                assert_eq!(files[0].read_from_index(1).unwrap(), MaybeDtcOutcome::DRAW);
                results.push((
                    std::fs::read(dir.0.join("KQ_KR.ggegt")).unwrap(),
                    std::fs::read(dir.0.join("KQ_KR.json")).unwrap(),
                ));
            });
        }
        assert!(results.windows(2).all(|pair| pair[0] == pair[1]));
    }
}
