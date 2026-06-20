pub mod piece_set;
mod egt;
mod egt_file;
mod retrograde;

use egt_file::EgtFile;
use shakmaty::{Role, Chess, Color, Position};
use std::cmp::Ordering;
use std::path::PathBuf;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConversionType {
    Promotion,
    Capture,
    Checkmate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtcOutcome {
    Win(ConversionType, u16),
    Draw,
    Loss(ConversionType, u16),
}

impl Ord for DtcOutcome {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (DtcOutcome::Win(ct1, n1), DtcOutcome::Win(ct2, n2)) => n2.cmp(n1).then(ct1.cmp(ct2)),
            (DtcOutcome::Draw, DtcOutcome::Draw) => Ordering::Equal,
            (DtcOutcome::Loss(ct1, n1), DtcOutcome::Loss(ct2, n2)) => n1.cmp(n2).then(ct2.cmp(ct1)),
            (DtcOutcome::Win(_, _), DtcOutcome::Draw) => Ordering::Greater,
            (DtcOutcome::Draw, DtcOutcome::Win(_, _)) => Ordering::Less,
            (DtcOutcome::Win(_, _), DtcOutcome::Loss(_, _)) => Ordering::Greater,
            (DtcOutcome::Loss(_, _), DtcOutcome::Win(_, _)) => Ordering::Less,
            (DtcOutcome::Draw, DtcOutcome::Loss(_, _)) => Ordering::Greater,
            (DtcOutcome::Loss(_, _), DtcOutcome::Draw) => Ordering::Less,
        }
    }
}

impl PartialOrd for DtcOutcome {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(&other))
    }
}

pub struct EgtGenerator {
    base_path: PathBuf,
    assigned_memory: Option<usize>,
}

impl EgtGenerator {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            base_path: path.into(),
            assigned_memory: None,
        }
    }

    pub fn with_assigned_memory(&mut self, n: usize) {
        self.assigned_memory = Some(n);
    }

    pub fn generate(&self, tablename: &str) {
        let start_time = std::time::Instant::now();
        println!("Generating table {} at {:?}", tablename, self.base_path);

        // TODO: Preallocate memory
        //if let Some(n) = self.assigned_memory {
        //    ...
        //}

        // Run retrograde analysis
        let (mut file_a, mut file_b) = crate::retrograde::retrograde_analysis(&self.base_path, tablename);

        // Save to file
        let bytes_a = file_a.save_to_file().expect("Failed to flush EgtFile A");
        let mut bytes_b = None;
        if let Some(ref mut fb) = file_b {
            bytes_b = Some(fb.save_to_file().expect("Failed to flush EgtFile B"));
        }
        let duration = start_time.elapsed();
        print_pair_stats(&mut file_a, file_b.as_mut(), bytes_a, bytes_b, duration);

        // Check internal consistency
        let mut prober = EgtProber::new(&self.base_path);
        prober.verify_internal_consistency(&file_a.tablename).expect("Failed internal consistency check!");
        if let Some(ref mut fb) = file_b {
            prober.verify_internal_consistency(&fb.tablename).expect("Failed internal consistency check!");
        }
    }
}

pub struct EgtProber {
    base_path: PathBuf,
    assigned_memory: Option<usize>,
    cache: HashMap<String, EgtFile>,
}

impl EgtProber {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            base_path: path.into(),
            assigned_memory: None,
            cache: HashMap::new(),
        }
    }

    pub fn with_assigned_memory(&mut self, n: usize) {
        self.assigned_memory = Some(n);
    }

    pub fn probe(&mut self, position: &Chess) -> Result<DtcOutcome, ()> {
        let tablename = get_tablename(position);
        if !self.cache.contains_key(&tablename) {
            let file = EgtFile::new_from_file(&self.base_path, &tablename)?;
            self.cache.insert(tablename.clone(), file);
        }
        let file = self.cache.get_mut(&tablename).unwrap();
        file.probe(position)?.to_outcome()
    }

    pub fn verify_internal_consistency(&mut self, tablename: &str) -> Result<(), ()> {
        // Ensure the main table is loaded in the cache
        if !self.cache.contains_key(tablename) {
            let file = EgtFile::new_from_file(&self.base_path, tablename)?;
            self.cache.insert(tablename.to_string(), file);
        }

        let index_range = self.cache.get(tablename).unwrap().index_range;

        println!("Verifying internal consistency of table {} ({} indexes)...", tablename, index_range);

        for idx in 0..index_range {
            // Retrieve the setup and the outcome for this index
            let (position_opt, outcome_maybe) = {
                let file = self.cache.get_mut(tablename).unwrap();
                let position_opt = file.index_to_position(idx, Color::White);
                let outcome_maybe = file.read_from_index(idx)?;
                (position_opt, outcome_maybe)
            };

            if position_opt.is_none() {
                if !outcome_maybe.is_invalid() {
                    println!("Error: index {} is invalid but outcome is not invalid", idx);
                    return Err(());
                }
                continue;
            }

            if outcome_maybe.is_invalid() {
                println!("Error: index {} is valid but outcome is invalid", idx);
                return Err(());
            }

            let position = position_opt.unwrap();
            let outcome = outcome_maybe.to_outcome()?;
            let legals = position.legal_moves();

            if legals.is_empty() {
                if position.is_check() {
                    if outcome != DtcOutcome::Loss(ConversionType::Checkmate, 0) {
                        println!("Error: checkmate position at index {} has outcome {:?}", idx, outcome);
                        return Err(());
                    }
                } else {
                    if outcome != DtcOutcome::Draw {
                        println!("Error: stalemate position at index {} has outcome {:?}", idx, outcome);
                        return Err(());
                    }
                }
            } else {
                let mut best_value: Option<DtcOutcome> = None;

                for m in legals {
                    let mut successor_position = position.clone();
                    successor_position.play_unchecked(m);

                    // Use self.probe() to get the successor outcome
                    let successor_outcome = self.probe(&successor_position)?;

                    let is_capture = m.is_capture();
                    let is_promotion = m.is_promotion();
                    let v_m = if is_capture || is_promotion {
                        let ct = if is_capture { ConversionType::Capture } else { ConversionType::Promotion };
                        match successor_outcome {
                            DtcOutcome::Loss(_, _) => DtcOutcome::Win(ct, 1),
                            DtcOutcome::Draw => DtcOutcome::Draw,
                            DtcOutcome::Win(_, _) => DtcOutcome::Loss(ct, 1),
                        }
                    } else {
                        match successor_outcome {
                            DtcOutcome::Loss(ct, n) => DtcOutcome::Win(ct, n + 1),
                            DtcOutcome::Draw => DtcOutcome::Draw,
                            DtcOutcome::Win(ct, n) => DtcOutcome::Loss(ct, n + 1),
                        }
                    };

                    if let Some(ref mut best) = best_value {
                        if v_m > *best {
                            *best = v_m;
                        }
                    } else {
                        best_value = Some(v_m);
                    }
                }

                let best = best_value.unwrap();
                if outcome != best {
                    println!(
                        "Error: consistency check failed at index {} of {}.\nPosition: {:?}\nOutcome in file: {:?}\nBest outcome from legal moves: {:?}",
                        idx, tablename, position, outcome, best
                    );
                    println!("Legal moves and their outcomes:");
                    for m in position.legal_moves() {
                        let mut successor_position = position.clone();
                        successor_position.play_unchecked(m);
                        let successor_outcome = self.probe(&successor_position).unwrap();
                        let is_capture = m.is_capture();
                        let is_promotion = m.is_promotion();
                        let v_m = if is_capture || is_promotion {
                            let ct = if is_capture { ConversionType::Capture } else { ConversionType::Promotion };
                            match successor_outcome {
                                DtcOutcome::Loss(_, _) => DtcOutcome::Win(ct, 1),
                                DtcOutcome::Draw => DtcOutcome::Draw,
                                DtcOutcome::Win(_, _) => DtcOutcome::Loss(ct, 1),
                            }
                        } else {
                            match successor_outcome {
                                DtcOutcome::Loss(ct, n) => DtcOutcome::Win(ct, n + 1),
                                DtcOutcome::Draw => DtcOutcome::Draw,
                                DtcOutcome::Win(ct, n) => DtcOutcome::Loss(ct, n + 1),
                            }
                        };
                        println!("  Move: {:?}, Successor Outcome: {:?}, Value: {:?}", m, successor_outcome, v_m);
                    }
                    return Err(());
                }
            }
        }

        println!("Successfully verified internal consistency of table {}.", tablename);
        Ok(())
    }
}

pub fn get_tablename(position: &Chess) -> String {
    let stm_color = position.turn();
    let sntm_color = !stm_color;

    let mut stm = String::new();
    let mut sntm = String::new();

    // We always have exactly 1 King for each side
    stm.push('K');
    sntm.push('K');

    // Count other pieces in order: Q, R, B, N, P
    let board = position.board();
    for &role in &[Role::Queen, Role::Rook, Role::Bishop, Role::Knight, Role::Pawn] {
        let stm_count = (board.by_role(role) & board.by_color(stm_color)).into_iter().count();
        for _ in 0..stm_count {
            stm.push(role.upper_char());
        }

        let sntm_count = (board.by_role(role) & board.by_color(sntm_color)).into_iter().count();
        for _ in 0..sntm_count {
            sntm.push(role.upper_char());
        }
    }

    format!("{}_{}", stm, sntm)
}

/// Prints table-specific statistics (wins, draws, losses, compression).
pub fn print_table_stats(tablename: &str, wins: usize, draws: usize, losses: usize, bytes: u64) {
    let unique_positions = wins + draws + losses;
    let compressed_size_mb = bytes as f64 / (1024.0 * 1024.0);
    let bits_per_pos = if unique_positions > 0 {
        (bytes as f64 * 8.0) / unique_positions as f64
    } else {
        0.0
    };

    println!(
        "Generated table {} with {} unique positions: {} wins, {} draws, {} losses. Compressed size: {:.0}MB ({:.2} bits/pos).",
        tablename,
        unique_positions,
        wins,
        draws,
        losses,
        compressed_size_mb,
        bits_per_pos
    );
}

/// Prints detailed statistics about a pair of generated tables (or a single table if symmetric).
pub fn print_pair_stats(
    file_a: &mut EgtFile,
    file_b: Option<&mut EgtFile>,
    bytes_a: u64,
    bytes_b: Option<u64>,
    duration: std::time::Duration,
) {
    let (wins_a, draws_a, losses_a, _) = file_a.count_outcomes();
    let mut unique_positions = wins_a + draws_a + losses_a;
    print_table_stats(&file_a.tablename, wins_a, draws_a, losses_a, bytes_a);

    if let Some(fb) = file_b {
        let (wins_b, draws_b, losses_b, _) = fb.count_outcomes();
        unique_positions += wins_b + draws_b + losses_b;
        print_table_stats(&fb.tablename, wins_b, draws_b, losses_b, bytes_b.unwrap());
    }

    let us_per_pos = if unique_positions > 0 {
        duration.as_micros() as f64 / unique_positions as f64
    } else {
        0.0
    };

    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    let format_dur = format!("{:02}h:{:02}m:{:02}s", hours, minutes, seconds);

    println!(
        "Time used {} ({:.2} μs/pos).",
        format_dur,
        us_per_pos,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_internal_consistency_k_k() {
        let temp_dir = std::env::temp_dir();
        let (mut file_a, mut file_b) = crate::retrograde::retrograde_analysis(&temp_dir, "K_K");
        file_a.save_to_file().unwrap();
        if let Some(ref mut fb) = file_b {
            fb.save_to_file().unwrap();
        }

        let mut prober = EgtProber::new(&temp_dir);
        prober.verify_internal_consistency("K_K").unwrap();
    }

    #[test]
    fn test_verify_internal_consistency_kr_k() {
        let temp_dir = std::env::temp_dir();
        let (mut file_a, mut file_b) = crate::retrograde::retrograde_analysis(&temp_dir, "KR_K");
        file_a.save_to_file().unwrap();
        if let Some(ref mut fb) = file_b {
            fb.save_to_file().unwrap();
        }

        let mut prober = EgtProber::new(&temp_dir);
        prober.verify_internal_consistency("KR_K").unwrap();
    }
}
