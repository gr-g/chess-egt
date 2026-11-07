//! Single-table finalization in the caller's Rayon pool, with ordered frames.

mod stats;

use std::io::Write;
use std::sync::atomic::AtomicU16;

use rayon::prelude::*;

use self::stats::TableStats;
use crate::PackedOutcome;
use crate::egt::Egt;
use crate::egt_file::writer::{COMPRESSION_LEVEL, CompressedSegment, EgtFileWriter, encode_frame};
use crate::error::{EgtError, EgtResult};
use crate::position::InvalidCounts;
use crate::stats::{EgtStats, write_stats};
use crate::storage::{FRAME_SIZE, Location};
use crate::table_id::TableId;

fn finalize_batch(
    values: &mut [AtomicU16],
    start: usize,
    egt: &Egt,
    frame_size: usize,
    maxima: [u16; 2],
) -> EgtResult<(CompressedSegment, TableStats)> {
    let mut data = Vec::new();
    let mut encoder = zeekstd::EncodeOptions::new()
        .compression_level(COMPRESSION_LEVEL)
        .into_encoder(&mut data)?;
    let mut plain = Vec::with_capacity(frame_size.min(values.len()));
    let mut stats = TableStats::default();
    for (frame_idx, chunk) in values.chunks_mut(frame_size).enumerate() {
        plain.clear();
        for (offset, cell) in chunk.iter_mut().enumerate() {
            // Propagation has joined; exclusive access needs no atomic load/CAS.
            let raw = PackedOutcome::from_u16(*cell.get_mut());
            let outcome = if raw.is_unknown() {
                PackedOutcome::DRAW
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
pub(super) fn finalize_table(
    location: &Location,
    id: &TableId,
    egt: &Egt,
    mut values: Vec<AtomicU16>,
    maxima: [u16; 2],
    invalid: InvalidCounts,
) -> EgtResult<EgtStats> {
    if values.len() != egt.index_range() {
        return Err(EgtError::Internal("completed table has wrong length"));
    }
    let writer = EgtFileWriter::new(location, id, egt.index_range(), FRAME_SIZE)?;
    let frames = values.len().div_ceil(FRAME_SIZE);
    let batch_size = frames.div_ceil(rayon::current_num_threads()).max(1) * FRAME_SIZE;
    // Contiguous batches reuse one encoder each. Indexed collection preserves
    // frame and example order, independently of worker completion order.
    let batches: Vec<_> = values
        .par_chunks_mut(batch_size)
        .enumerate()
        .map(|(idx, batch)| finalize_batch(batch, idx * batch_size, egt, FRAME_SIZE, maxima))
        .collect();
    drop(values);
    let batches = batches.into_iter().collect::<EgtResult<Vec<_>>>()?;
    let (segments, parts): (Vec<_>, Vec<_>) = batches.into_iter().unzip();
    let mut stats = TableStats::default();
    stats.merge(parts);
    let (bytes, sha256) = writer.publish(segments)?;
    let stats = stats.build(egt, maxima, invalid, bytes, sha256);
    write_stats(location, id, &stats)?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrong_length_does_not_publish() {
        let dir = crate::TestDir::new("finalization_wrong_length");
        let location = Location::parse(&dir.0).unwrap();
        let id = TableId::parse("K_K").unwrap();
        let egt = Egt::from_elements(id.parse_elements()).unwrap();
        assert!(matches!(
            finalize_table(
                &location,
                &id,
                &egt,
                Vec::new(),
                [0, 0],
                InvalidCounts::default()
            ),
            Err(EgtError::Internal("completed table has wrong length"))
        ));
        assert!(!dir.0.join(id.relative_path("ggegt")).exists());
        assert!(!dir.0.join(id.relative_path("json")).exists());
    }
}
