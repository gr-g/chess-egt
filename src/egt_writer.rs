//! Stages finalized tables without retaining their uncompressed arrays.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::egt_file::{DEFAULT_COMPRESSION_LEVEL, EgtFile, MaybeDtcOutcome, transpose_frame};
use crate::error::{EgtError, EgtResult};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub(crate) struct TemporaryFile {
    pub(crate) path: PathBuf,
    pub(crate) file: File,
}

impl TemporaryFile {
    pub(crate) fn new(destination: &Path, kind: &str) -> io::Result<Self> {
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let name = destination.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "destination has no filename")
        })?;
        loop {
            let mut temporary_name = name.to_os_string();
            temporary_name.push(format!(
                ".{}.{}.{}.tmp",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed),
                kind
            ));
            let path = parent.join(temporary_name);
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => return Ok(Self { path, file }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct Segment {
    offset: u64,
    length: u64,
    frames: Vec<(u32, u32)>,
}

/// One compressed spool per destination; publication is explicit and atomic.
pub(crate) struct EgtFileWriter {
    destination: PathBuf,
    frame_size: usize,
    lengths: Vec<usize>,
    segments: Vec<Option<Segment>>,
    spool: TemporaryFile,
    completed: Option<TemporaryFile>,
    published: bool,
}

impl EgtFileWriter {
    pub(crate) fn new(file: &EgtFile) -> EgtResult<Self> {
        if file.frame_size == 0 || file.frame_size > (u32::MAX as usize) / 2 {
            return Err(EgtError::Internal("invalid writer frame size"));
        }
        Ok(Self {
            destination: file.path.clone(),
            frame_size: file.frame_size,
            lengths: file.egts.iter().map(|egt| egt.index_range()).collect(),
            segments: (0..file.egts.len()).map(|_| None).collect(),
            spool: TemporaryFile::new(&file.path, "spool")?,
            completed: None,
            published: false,
        })
    }

    pub(crate) fn write_table(
        &mut self,
        egt_idx: usize,
        values: &[MaybeDtcOutcome],
    ) -> EgtResult<()> {
        if self.completed.is_some() || self.published {
            return Err(EgtError::Internal("writer is already finished"));
        }
        let length = *self.lengths.get(egt_idx).ok_or(EgtError::IndexOutOfRange {
            index: egt_idx,
            range: self.lengths.len(),
        })?;
        if self.segments[egt_idx].is_some() {
            return Err(EgtError::Internal("table has already been written"));
        }
        if values.len() != length {
            return Err(EgtError::Internal("finalized table has wrong length"));
        }
        if values.iter().any(|value| value.is_unknown()) {
            return Err(EgtError::Internal(
                "finalized table contains unknown outcomes",
            ));
        }

        // Failed writes leave only unreachable bytes; retries append a fresh segment.
        let offset = self.spool.file.seek(SeekFrom::End(0))?;
        let mut encoder = zeekstd::EncodeOptions::new()
            .compression_level(DEFAULT_COMPRESSION_LEVEL)
            .into_encoder(&mut self.spool.file)?;
        for chunk in values.chunks(self.frame_size) {
            encode_frame(&mut encoder, chunk)?;
        }
        // into_seek_table consumes the encoder without flushing its output buffer.
        encoder.flush()?;
        let table = encoder.into_seek_table();
        self.spool.file.flush()?;
        let mut frames = Vec::with_capacity(table.num_frames() as usize);
        for index in 0..table.num_frames() {
            frames.push((
                table.frame_size_comp(index)? as u32,
                table.frame_size_decomp(index)? as u32,
            ));
        }
        self.segments[egt_idx] = Some(Segment {
            offset,
            length: table.size_comp(),
            frames,
        });
        Ok(())
    }

    /// Assembles a complete temporary file but does not change the destination.
    pub(crate) fn finish(&mut self) -> EgtResult<()> {
        if self.published {
            return Err(EgtError::Internal("writer is already published"));
        }
        if self.completed.is_some() {
            return Ok(());
        }
        if self.segments.iter().any(Option::is_none) {
            return Err(EgtError::Internal("cannot finish with missing tables"));
        }
        self.spool.file.flush()?;
        let mut completed = TemporaryFile::new(&self.destination, "completed")?;
        {
            let mut output = BufWriter::new(&mut completed.file);
            let mut table = zeekstd::SeekTable::new();
            for segment in self.segments.iter().flatten() {
                self.spool.file.seek(SeekFrom::Start(segment.offset))?;
                let copied = io::copy(
                    &mut (&mut self.spool.file).take(segment.length),
                    &mut output,
                )?;
                if copied != segment.length {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "compressed spool segment is truncated",
                    )
                    .into());
                }
                for &(compressed, decompressed) in &segment.frames {
                    table.log_frame(compressed, decompressed)?;
                }
            }
            io::copy(&mut table.into_serializer(), &mut output)?;
            output.flush()?;
        }
        self.completed = Some(completed);
        Ok(())
    }

    /// Renames the finished file over the destination and returns its byte size.
    pub(crate) fn publish(&mut self) -> EgtResult<u64> {
        if self.published {
            return Err(EgtError::Internal("writer is already published"));
        }
        let completed = self.completed.as_mut().ok_or(EgtError::Internal(
            "writer must be finished before publication",
        ))?;
        completed.file.flush()?;
        let bytes = completed.file.metadata()?.len();
        fs::rename(&completed.path, &self.destination)?;
        self.completed = None;
        self.published = true;
        Ok(bytes)
    }
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

    #[test]
    fn reverse_order_and_short_tails_load_via_reader() {
        let dir = crate::TestDir::new("writer_reverse_order");
        let file = EgtFile::new(&dir.0, "KP_K").unwrap();
        // Each table ends in a short frame, exercising per-table frame boundaries.
        assert!(
            file.egts
                .iter()
                .all(|egt| egt.index_range() % file.frame_size != 0)
        );
        let mut writer = EgtFileWriter::new(&file).unwrap();
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
        assert!(!file.path.exists());
        let bytes = writer.publish().unwrap();
        assert_eq!(bytes, fs::metadata(&file.path).unwrap().len());
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
    fn unique_temporaries_and_drop_cleanup_preserve_destination() {
        let dir = crate::TestDir::new("writer_cleanup");
        let file = EgtFile::new(&dir.0, "K_K").unwrap();
        fs::write(&file.path, b"existing destination").unwrap();
        let mut writer = EgtFileWriter::new(&file).unwrap();
        let other = EgtFileWriter::new(&file).unwrap();
        assert_ne!(writer.spool.path, other.spool.path);
        let spool = writer.spool.path.clone();
        let other_spool = other.spool.path.clone();
        writer
            .write_table(0, &vec![MaybeDtcOutcome::DRAW; file.egts[0].index_range()])
            .unwrap();
        writer.finish().unwrap();
        let completed = writer.completed.as_ref().unwrap().path.clone();
        drop(writer);
        drop(other);
        for path in [spool, other_spool, completed] {
            assert!(!path.exists());
        }
        assert_eq!(fs::read(&file.path).unwrap(), b"existing destination");
    }

    #[test]
    fn truncated_spool_cleans_failed_assembly() {
        let dir = crate::TestDir::new("writer_truncated");
        let file = EgtFile::new(&dir.0, "K_K").unwrap();
        let mut writer = EgtFileWriter::new(&file).unwrap();
        writer
            .write_table(0, &vec![MaybeDtcOutcome::DRAW; file.egts[0].index_range()])
            .unwrap();
        writer.spool.file.set_len(0).unwrap();
        assert!(writer.finish().is_err());
        assert!(writer.completed.is_none());
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
        drop(writer);
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
    }
}
