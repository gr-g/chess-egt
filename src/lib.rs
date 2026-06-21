pub mod piece_set;
mod egt;
mod egt_file;
mod retrograde;

pub use egt_file::{EgtFile, EgtFileStats, LongestDtcPosition};
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

    pub fn list_n_pieces_endgames(n: usize) -> Vec<String> {
        assert!(n <= 8, "list_n_pieces_endgames() called with n > 8");
        if n < 2 {
            return Vec::new();
        }
        let mut endgames = Vec::new();
        let num_non_kings = n - 2;

        // Helper to generate all combinations of non-king pieces of size k
        fn get_piece_combinations(k: usize) -> Vec<String> {
            let mut results = Vec::new();
            let mut current = String::new();
            let roles = ['Q', 'R', 'B', 'N', 'P'];
            fn recurse(k: usize, start_idx: usize, current: &mut String, results: &mut Vec<String>, roles: &[char]) {
                if current.len() == k {
                    results.push(current.clone());
                    return;
                }
                for idx in start_idx..roles.len() {
                    current.push(roles[idx]);
                    recurse(k, idx, current, results, roles);
                    current.pop();
                }
            }
            recurse(k, 0, &mut current, &mut results, &roles);
            results
        }

        // Distribute num_non_kings between side_a and side_b
        for a in 0..=num_non_kings {
            let b = num_non_kings - a;
            let combos_a = get_piece_combinations(a);
            let combos_b = get_piece_combinations(b);

            for pieces_a in &combos_a {
                for pieces_b in &combos_b {
                    let side_a = format!("K{}", pieces_a);
                    let side_b = format!("K{}", pieces_b);

                    // Keep only one-sided endgames: side_a >= side_b
                    if side_a >= side_b {
                        endgames.push(format!("{}_{}", side_a, side_b));
                    }
                }
            }
        }

        // Sort by:
        // 1. Number of pawns (ascending)
        // 2. Lexicographical order of the endgame string
        endgames.sort_by(|a, b| {
            let pawns_a = a.chars().filter(|&c| c == 'P').count();
            let pawns_b = b.chars().filter(|&c| c == 'P').count();
            pawns_a.cmp(&pawns_b).then_with(|| a.cmp(b))
        });

        endgames
    }

    pub fn generate(&self, endgame: &str) -> Result<(EgtFileStats, Option<EgtFileStats>), ()> {
        let start_time = std::time::Instant::now();
        println!("Generating endgame {} at {:?}", endgame, self.base_path);

        // TODO: Preallocate memory
        //if let Some(n) = self.assigned_memory {
        //    ...
        //}

        // Run retrograde analysis
        let (mut file_a, mut file_b) = crate::retrograde::retrograde_analysis(&self.base_path, endgame)?;

        // Save to file
        let bytes_a = file_a.save_to_file().map_err(|_| ())?;
        let mut bytes_b = None;
        if let Some(ref mut fb) = file_b {
            bytes_b = Some(fb.save_to_file().map_err(|_| ())?);
        }

        // Compute SHA-256 and finalize stats
        let sha256_a = compute_sha256(&file_a.path).map_err(|_| ())?;
        let mut stats_a = file_a.stats.take().unwrap();
        stats_a.bytes = bytes_a;
        stats_a.sha256 = sha256_a;

        // Save JSON file for A
        let json_path_a = file_a.path.with_extension("json");
        let json_str_a = serde_json::to_string_pretty(&stats_a).map_err(|_| ())?;
        std::fs::write(json_path_a, json_str_a).map_err(|_| ())?;

        let mut stats_b = None;
        if let Some(ref mut fb) = file_b {
            let sha256_b = compute_sha256(&fb.path).map_err(|_| ())?;
            let mut s_b = fb.stats.take().unwrap();
            s_b.bytes = bytes_b.unwrap();
            s_b.sha256 = sha256_b;

            // Save JSON file for B
            let json_path_b = fb.path.with_extension("json");
            let json_str_b = serde_json::to_string_pretty(&s_b).map_err(|_| ())?;
            std::fs::write(json_path_b, json_str_b).map_err(|_| ())?;

            stats_b = Some(s_b);
        }

        let duration = start_time.elapsed();
        print_pair_stats(&stats_a, stats_b.as_ref(), duration);

        // Check internal consistency
        let mut prober = EgtProber::new(&self.base_path);
        prober.verify_internal_consistency(&file_a.endgame)?;
        if let Some(ref mut fb) = file_b {
            prober.verify_internal_consistency(&fb.endgame)?;
        }

        Ok((stats_a, stats_b))
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
        let endgame = get_endgame(position);
        if !self.cache.contains_key(&endgame) {
            let file = EgtFile::new_from_file(&self.base_path, &endgame)?;
            self.cache.insert(endgame.clone(), file);
        }
        let file = self.cache.get_mut(&endgame).unwrap();
        file.probe(position)?.to_outcome()
    }

    pub fn verify_internal_consistency(&mut self, endgame: &str) -> Result<(), ()> {
        // Ensure the main file is in the cache
        if !self.cache.contains_key(endgame) {
            let file = EgtFile::new_from_file(&self.base_path, endgame)?;
            self.cache.insert(endgame.to_string(), file);
        }

        let index_range = self.cache.get(endgame).unwrap().index_range;

        println!("Verifying internal consistency of endgame {} ({} indexes)...", endgame, index_range);

        for idx in 0..index_range {
            // Retrieve the setup and the outcome for this index
            let (position_opt, outcome_maybe) = {
                let file = self.cache.get_mut(endgame).unwrap();
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
                        idx, endgame, position, outcome, best
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

        println!("Successfully verified internal consistency of endgame {}.", endgame);
        Ok(())
    }
}

pub fn get_endgame(position: &Chess) -> String {
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
pub fn print_table_stats(stats: &EgtFileStats) {
    let unique_positions = stats.unique_positions;
    let compressed_size_mb = stats.bytes as f64 / (1024.0 * 1024.0);
    let bits_per_pos = if unique_positions > 0 {
        (stats.bytes as f64 * 8.0) / unique_positions as f64
    } else {
        0.0
    };

    println!(
        "Generated endgame {} with {} unique positions: {} wins, {} draws, {} losses. Compressed size: {:.2}MiB ({:.2} bits/pos).",
        stats.endgame,
        unique_positions,
        stats.win,
        stats.draw,
        stats.loss,
        compressed_size_mb,
        bits_per_pos
    );
}

/// Prints detailed statistics about a pair of generated files (or a single file if symmetric).
pub fn print_pair_stats(
    stats_a: &EgtFileStats,
    stats_b: Option<&EgtFileStats>,
    duration: std::time::Duration,
) {
    let mut unique_positions = stats_a.unique_positions;
    print_table_stats(stats_a);

    if let Some(sb) = stats_b {
        unique_positions += sb.unique_positions;
        print_table_stats(sb);
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


fn compute_sha256(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Sha256, Digest};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_internal_consistency_k_k() {
        let temp_dir = std::env::temp_dir();
        let (mut file_a, mut file_b) = crate::retrograde::retrograde_analysis(&temp_dir, "K_K").unwrap();
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
        let (mut file_a, mut file_b) = crate::retrograde::retrograde_analysis(&temp_dir, "KR_K").unwrap();
        file_a.save_to_file().unwrap();
        if let Some(ref mut fb) = file_b {
            fb.save_to_file().unwrap();
        }

        let mut prober = EgtProber::new(&temp_dir);
        prober.verify_internal_consistency("KR_K").unwrap();
    }

    #[test]
    fn test_list_n_pieces_endgames() {
        let all_three = EgtGenerator::list_n_pieces_endgames(3);
        assert_eq!(all_three, vec![
            "KB_K".to_string(),
            "KN_K".to_string(),
            "KQ_K".to_string(),
            "KR_K".to_string(),
            "KP_K".to_string(),
        ]);

        let all_two = EgtGenerator::list_n_pieces_endgames(2);
        assert_eq!(all_two, vec!["K_K".to_string()]);

        let all_four = EgtGenerator::list_n_pieces_endgames(4);
        // Ensure pawnless endgames are first, then 1 pawn, then 2 pawns
        let mut max_pawns = 0;
        for eg in all_four {
            let pawns = eg.chars().filter(|&c| c == 'P').count();
            assert!(pawns >= max_pawns, "Endgame {} with {} pawns came after an endgame with more pawns", eg, pawns);
            max_pawns = pawns;
        }
    }
}
