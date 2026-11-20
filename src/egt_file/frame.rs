use std::{cmp::Reverse, ops::Range};

use crate::outcome::PackedOutcome;
use crate::{EgtError, EgtResult};

#[derive(Clone, Debug)]
pub(crate) struct Frame {
    data: Vec<u8>,
    len: usize,
    d: usize,
}

fn invalid(message: &'static str) -> EgtError {
    EgtError::invalid_data("tablebase frame", message)
}

fn serialized_length(d: usize, len: usize) -> u128 {
    2 + 2 * d as u128
        + len as u128
            * if d == 1 {
                0
            } else if d <= 256 {
                1
            } else {
                2
            }
}

fn allocate<T>(len: usize) -> EgtResult<Vec<T>> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(len)
        .map_err(|error| EgtError::WorkingMemoryAllocation {
            context: "frame payload".into(),
            bytes: len as u128 * std::mem::size_of::<T>() as u128,
            reason: error.to_string(),
        })?;
    Ok(values)
}

impl Frame {
    pub(super) fn decode(data: Vec<u8>, len: usize) -> EgtResult<Self> {
        if data.len() < 2 {
            return Err(invalid("truncated frame palette count"));
        }
        let d = u16::from_le_bytes([data[0], data[1]]) as usize;
        if d == 0 || d > len || data.len() as u128 != serialized_length(d, len) {
            return Err(invalid("invalid frame palette count or payload length"));
        }
        let mut seen = vec![false; 65536];
        for entry in data[2..2 + 2 * d].chunks_exact(2) {
            let raw = u16::from_le_bytes([entry[0], entry[1]]) as usize;
            if std::mem::replace(&mut seen[raw], true) {
                return Err(invalid("duplicate frame palette entry"));
            }
        }
        let frame = Self { data, len, d };
        if d > 1 && (0..len).any(|index| frame.rank(index) >= d) {
            return Err(invalid("frame palette index out of bounds"));
        }
        Ok(frame)
    }

    fn rank(&self, index: usize) -> usize {
        let offset = 2 + 2 * self.d;
        match self.d {
            1 => 0,
            2..=256 => self.data[offset + index] as usize,
            _ => {
                self.data[offset + index] as usize
                    | ((self.data[offset + self.len + index] as usize) << 8)
            }
        }
    }

    pub(super) fn get(&self, index: usize) -> PackedOutcome {
        debug_assert!(index < self.len);
        let offset = 2 + 2 * self.rank(index);
        PackedOutcome::from_u16(u16::from_le_bytes([
            self.data[offset],
            self.data[offset + 1],
        ]))
    }

    /// Retained serialized payload allocation, excluding the Frame itself.
    pub(super) fn bytes(&self) -> usize {
        self.data.capacity()
    }

    pub(super) fn expand(&self, range: Range<usize>) -> EgtResult<Vec<PackedOutcome>> {
        if range.start > range.end || range.end > self.len {
            return Err(invalid("frame expansion range out of bounds"));
        }
        let mut values = allocate(range.end - range.start)?;
        values.extend(range.map(|index| self.get(index)));
        Ok(values)
    }
}

pub(super) fn encode(values: &[PackedOutcome]) -> EgtResult<Vec<u8>> {
    if values.is_empty() {
        return Err(EgtError::Internal("cannot encode an empty frame"));
    }
    let mut counts = vec![0usize; 65536];
    for value in values {
        counts[value.to_u16() as usize] += 1;
    }
    let mut palette: Vec<u16> = (0..=u16::MAX)
        .filter(|&raw| counts[raw as usize] != 0)
        .collect();
    let d = palette.len();
    if d > u16::MAX as usize {
        return Err(EgtError::Internal("frame palette exceeds 65535 entries"));
    }
    palette.sort_unstable_by_key(|&raw| (Reverse(counts[raw as usize]), raw));
    let size = usize::try_from(serialized_length(d, values.len()))
        .map_err(|_| EgtError::Internal("encoded frame length overflows usize"))?;
    let mut data = allocate(size)?;
    data.extend_from_slice(&(d as u16).to_le_bytes());
    let mut ranks = vec![0u16; 65536];
    for (rank, raw) in palette.into_iter().enumerate() {
        data.extend_from_slice(&raw.to_le_bytes());
        ranks[raw as usize] = rank as u16;
    }
    if d > 1 {
        data.extend(
            values
                .iter()
                .map(|value| ranks[value.to_u16() as usize] as u8),
        );
    }
    if d > 256 {
        data.extend(
            values
                .iter()
                .map(|value| (ranks[value.to_u16() as usize] >> 8) as u8),
        );
    }
    Ok(data)
}

/// Checks all three index widths without allocating or overflowing on metadata.
pub(super) fn valid_length(decompressed: u64, len: usize) -> bool {
    let n = len as u128;
    let size = decompressed as u128;
    [(1, 1, 0), (2, 256, 1), (257, 65535, 2)]
        .into_iter()
        .any(|(min, max, width)| {
            let Some(palette_bytes) = size.checked_sub(2 + width * n) else {
                return false;
            };
            let d = palette_bytes / 2;
            palette_bytes % 2 == 0 && d >= min && d <= max && d <= n
        })
}

pub(super) fn max_length(len: usize) -> usize {
    2usize
        .saturating_add(2 * len.min(65535))
        .saturating_add(len.saturating_mul(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_widths_frequency_ties_and_raw_values() {
        for d in [1, 256, 257] {
            let values: Vec<_> = (0..d)
                .map(|i| PackedOutcome::from_u16(u16::MAX - i))
                .collect();
            let data = encode(&values).unwrap();
            assert_eq!(
                data.len() as u128,
                serialized_length(d as usize, values.len())
            );
            assert!(valid_length(data.len() as u64, values.len()));
            assert!(data.len() <= max_length(values.len()));
            if d > 1 {
                assert_eq!(data[2 + 2 * d as usize], (d - 1) as u8);
                if d > 256 {
                    assert_eq!(data[2 + 3 * d as usize], 1);
                }
            }
            let capacity = data.capacity();
            let frame = Frame::decode(data, values.len()).unwrap();
            assert_eq!(frame.bytes(), capacity);
            assert_eq!(frame.expand(0..values.len()).unwrap(), values);
            assert_eq!(frame.get(values.len() - 1), *values.last().unwrap());
            assert_eq!(frame.expand(0..0).unwrap(), []);
            assert!(frame.expand(0..values.len() + 1).is_err());
        }
        let values = [65535, 0, 8, 65535, 8].map(PackedOutcome::from_u16);
        let data = encode(&values).unwrap();
        assert_eq!(data, [3, 0, 8, 0, 255, 255, 0, 0, 1, 2, 0, 1, 0]);
        assert_eq!(encode(&values).unwrap(), data);
        assert_eq!(
            Frame::decode(data, 5).unwrap().expand(1..5).unwrap(),
            values[1..]
        );
        assert!(encode(&[]).is_err());
        let all: Vec<_> = (0..=u16::MAX).map(PackedOutcome::from_u16).collect();
        assert!(encode(&all).is_err());
    }

    #[test]
    fn rejects_malformed_frames_and_impossible_metadata() {
        for (data, len) in [
            (vec![], 1),
            (vec![1], 1),
            (vec![0, 0], 1),
            (vec![1, 0, 0, 0], 0),
            (vec![2, 0, 0, 0, 1, 0, 0], 1),
            (vec![1, 0, 0], 1),
            (vec![1, 0, 0, 0, 0], 1),
            (vec![2, 0, 1, 0, 1, 0, 0, 1], 2),
            (vec![2, 0, 0, 0, 1, 0, 0, 2], 2),
        ] {
            assert!(Frame::decode(data, len).is_err());
        }
        let values: Vec<_> = (0..257).map(PackedOutcome::from_u16).collect();
        let mut data = encode(&values).unwrap();
        data[2 + 2 * 257] = 1;
        data[2 + 3 * 257] = 1;
        assert!(Frame::decode(data, 257).is_err());
        for len in [0, 1, 2, 256, 257, 65536] {
            let achievable: Vec<_> = (1..=len.min(65535))
                .map(|d| serialized_length(d, len) as u64)
                .collect();
            for size in 0..=max_length(len) as u64 + 1 {
                assert_eq!(
                    valid_length(size, len),
                    achievable.binary_search(&size).is_ok()
                );
            }
        }
        assert!(!valid_length(u64::MAX, usize::MAX));
        assert_eq!(max_length(usize::MAX), usize::MAX);
    }
}
