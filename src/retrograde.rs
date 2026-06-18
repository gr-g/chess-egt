use shakmaty::{Color, CastlingMode, Chess, FromSetup, Position, EnPassantMode, Role, Setup};
use shakmaty::retrograde::{RetrogradeAnalysis, CastlingRetrogradeMode};
use crate::{MaybeDtcOutcome, ConversionType, DtcOutcome};
use crate::egt_file::{EgtFile, Arena, PawnKey};
use crate::piece_set::{EgtPiece, EgtSide};

#[derive(Clone, Debug)]
pub struct RetrogradeTable {
    pub egt_idx: usize,
    pub is_table_b: bool,
}

fn get_pawn_files(pieces: &[(EgtPiece, EgtSide, usize)]) -> (Vec<shakmaty::File>, Vec<shakmaty::File>) {
    let mut stm_files = Vec::new();
    let mut sntm_files = Vec::new();
    for &(piece, side, multiplicity) in pieces {
        if let EgtPiece::Pawn(file) = piece {
            for _ in 0..multiplicity {
                match side {
                    EgtSide::SideToMove => stm_files.push(file),
                    EgtSide::SideNotToMove => sntm_files.push(file),
                }
            }
        }
    }
    stm_files.sort_by_key(|f| f.to_usize());
    sntm_files.sort_by_key(|f| f.to_usize());
    (stm_files, sntm_files)
}

fn read_outcome(
    file_a: &mut EgtFile,
    file_b: &mut Option<EgtFile>,
    table: &RetrogradeTable,
    local_index: usize,
    arena: &mut Arena,
) -> MaybeDtcOutcome {
    let file = if !table.is_table_b { &mut *file_a } else { file_b.as_mut().unwrap() };
    let global_index = file.get_global_index(table.egt_idx, local_index);
    file.read_by_global_index(global_index, arena).unwrap()
}

fn write_outcome(
    file_a: &mut EgtFile,
    file_b: &mut Option<EgtFile>,
    table: &RetrogradeTable,
    local_index: usize,
    outcome: MaybeDtcOutcome,
    arena: &mut Arena,
) {
    let file = if !table.is_table_b { &mut *file_a } else { file_b.as_mut().unwrap() };
    let global_index = file.get_global_index(table.egt_idx, local_index);
    file.write_by_global_index(global_index, outcome, arena).unwrap();
}

fn both_kings_on_diagonal(chess: &Chess) -> bool {
    let kings = chess.board().by_role(Role::King);
    kings.into_iter().all(|sq| sq.rank().to_usize() == sq.file().to_usize())
}

fn reflect_setup_diagonally(setup: &Setup) -> Setup {
    let mut reflected = setup.clone();
    reflected.board.flip_diagonal();
    reflected
}

pub fn quiet_unmoves(
    file_a: &mut EgtFile,
    file_b: &mut Option<EgtFile>,
    table: &RetrogradeTable,
    twin: &RetrogradeTable,
    local_index: usize,
    _arena: &mut Arena,
) -> Vec<usize> {
    let mut predecessors = Vec::new();
    let setup = {
        let file = if !table.is_table_b { &mut *file_a } else { file_b.as_mut().unwrap() };
        file.egts[table.egt_idx].indexer.board_from_index(local_index, Color::White)
    };
    let setup = match setup {
        Some(s) => s,
        None => return predecessors,
    };

    let mut indexer_copy = {
        let twin_file = if !twin.is_table_b { &mut *file_a } else { file_b.as_mut().unwrap() };
        twin_file.egts[twin.egt_idx].indexer.clone()
    };

    let pawnless = indexer_copy.n_pawns == 0;

    if let Ok(chess) = Chess::from_setup(setup, CastlingMode::Standard) {
        let current_on_diagonal = both_kings_on_diagonal(&chess);

        RetrogradeAnalysis::new(&chess)
            .with_castling_mode(CastlingRetrogradeMode::NoCastling)
            .quiet_unmoves(|pred_chess, _m| {
                let pred_setup = pred_chess.to_setup(EnPassantMode::Legal);
                let pred_idx = indexer_copy.board_to_index(&pred_setup);
                predecessors.push(pred_idx);

                if pawnless && !current_on_diagonal && both_kings_on_diagonal(&pred_chess) {
                    // #p=4 and #p'=8: push the diagonal reflection of the predecessor
                    let reflected_setup = reflect_setup_diagonally(&pred_setup);
                    let reflected_idx = indexer_copy.board_to_index(&reflected_setup);
                    predecessors.push(reflected_idx);
                }
            });
    }

    predecessors
}

fn symmetry_adjusted_move_counter(
    chess: &Chess,
    pawnless: bool,
) -> u16 {
    let current_on_diagonal = both_kings_on_diagonal(chess);

    let mut counter = 0;
    for m in chess.legal_moves() {
        let successor = chess.clone().play(m).unwrap();

        if pawnless && !current_on_diagonal && both_kings_on_diagonal(&successor) {
            counter += 2;
        } else {
            counter += 1;
        }
    }
    counter
}

fn initialize_table(
    file_a: &mut EgtFile,
    file_b: &mut Option<EgtFile>,
    table: &RetrogradeTable,
    twin: &RetrogradeTable,
    arena: &mut Arena,
) {
    let (size, pawnless) = {
        let file = if !table.is_table_b { &mut *file_a } else { file_b.as_mut().unwrap() };
        let indexer = &file.egts[table.egt_idx].indexer;
        (indexer.index_range, indexer.n_pawns == 0)
    };

    for idx in 0..size {
        // Decode position using Color::White as side-to-move
        let setup_opt = {
            let file = if !table.is_table_b { &mut *file_a } else { file_b.as_mut().unwrap() };
            file.egts[table.egt_idx].indexer.board_from_index(idx, Color::White)
        };
        if setup_opt.is_none() {
            write_outcome(file_a, file_b, table, idx, MaybeDtcOutcome::INVALID, arena);
            continue;
        }

        let setup = setup_opt.unwrap();
        let chess = Chess::from_setup(setup, CastlingMode::Standard).unwrap();
        let legals = chess.legal_moves();

        if legals.is_empty() {
            if chess.is_check() {
                // Checkmate!
                write_outcome(file_a, file_b, table, idx, MaybeDtcOutcome::new_loss(ConversionType::Checkmate, 0), arena);
                // Propagate to twin as win in 1 ply
                let predecessors = quiet_unmoves(file_a, file_b, table, twin, idx, arena);
                for pred_idx in predecessors {
                    let pred_outcome = read_outcome(file_a, file_b, twin, pred_idx, arena);
                    if pred_outcome.is_invalid() || pred_outcome.is_unknown() || pred_outcome == MaybeDtcOutcome::INVALID {
                        write_outcome(file_a, file_b, twin, pred_idx, MaybeDtcOutcome::new_win(ConversionType::Checkmate, 1), arena);
                    }
                }
            } else {
                // Stalemate!
                write_outcome(file_a, file_b, table, idx, MaybeDtcOutcome::DRAW, arena);
            }
        } else {
            // Unknown position, initialize move counter if not already marked as win
            let current_outcome = read_outcome(file_a, file_b, table, idx, arena);
            if current_outcome.is_invalid() {
                let counter = symmetry_adjusted_move_counter(&chess, pawnless);
                write_outcome(file_a, file_b, table, idx, MaybeDtcOutcome::new_unknown(counter), arena);
            }
        }
    }
}

fn propagate_wins_to_losses(
    file_a: &mut EgtFile,
    file_b: &mut Option<EgtFile>,
    table: &RetrogradeTable,
    twin: &RetrogradeTable,
    plies: u16,
    arena: &mut Arena,
) -> bool {
    let mut marked_any = false;
    let size = {
        let file = if !table.is_table_b { &mut *file_a } else { file_b.as_mut().unwrap() };
        file.egts[table.egt_idx].indexer.index_range
    };

    for idx in 0..size {
        let outcome = read_outcome(file_a, file_b, table, idx, arena);
        if outcome.is_win() && outcome.get_win_loss_distance() == Some(plies) {
            // Get conversion type
            let ct = match outcome.unwrap() {
                DtcOutcome::Win(ct, _) => ct,
                _ => unreachable!(),
            };

            // Find quiet predecessors in twin
            let predecessors = quiet_unmoves(file_a, file_b, table, twin, idx, arena);
            for pred_idx in predecessors {
                let pred_outcome = read_outcome(file_a, file_b, twin, pred_idx, arena);
                if pred_outcome.is_unknown() {
                    let counter = pred_outcome.get_unknown_counter();
                    assert!(counter > 0);
                    if counter == 1 {
                        // Counter reaches 0! Mark as loss in plies + 1
                        write_outcome(file_a, file_b, twin, pred_idx, MaybeDtcOutcome::new_loss(ct, plies + 1), arena);
                        marked_any = true;
                    } else {
                        // Decrement counter
                        write_outcome(file_a, file_b, twin, pred_idx, MaybeDtcOutcome::new_unknown(counter - 1), arena);
                    }
                }
            }
        }
    }

    marked_any
}

fn propagate_losses_to_wins(
    file_a: &mut EgtFile,
    file_b: &mut Option<EgtFile>,
    table: &RetrogradeTable,
    twin: &RetrogradeTable,
    plies: u16,
    arena: &mut Arena,
) -> bool {
    let mut marked_any = false;
    let size = {
        let file = if !table.is_table_b { &mut *file_a } else { file_b.as_mut().unwrap() };
        file.egts[table.egt_idx].indexer.index_range
    };

    for idx in 0..size {
        let outcome = read_outcome(file_a, file_b, table, idx, arena);
        if outcome.is_loss() && outcome.get_win_loss_distance() == Some(plies) {
            // Get conversion type
            let ct = match outcome.unwrap() {
                DtcOutcome::Loss(ct, _) => ct,
                _ => unreachable!(),
            };

            // Find quiet predecessors in twin
            let predecessors = quiet_unmoves(file_a, file_b, table, twin, idx, arena);
            for pred_idx in predecessors {
                let pred_outcome = read_outcome(file_a, file_b, twin, pred_idx, arena);
                if pred_outcome.is_unknown() {
                    write_outcome(file_a, file_b, twin, pred_idx, MaybeDtcOutcome::new_win(ct, plies + 1), arena);
                    marked_any = true;
                }
            }
        }
    }

    marked_any
}

pub fn retrograde_analysis(base_path: &std::path::Path, tablename: &str, arena: &mut Arena) -> (EgtFile, Option<EgtFile>) {
    let parts: Vec<&str> = tablename.split('_').collect();
    assert_eq!(parts.len(), 2);
    let twin_tablename = format!("{}_{}", parts[1], parts[0]);

    let is_symmetric = tablename == twin_tablename;

    let mut file_a = EgtFile::new(base_path.join(format!("{}.egt", tablename)), tablename, true).unwrap();
    let mut file_b = if is_symmetric {
        None
    } else {
        Some(EgtFile::new(base_path.join(format!("{}.egt", twin_tablename)), &twin_tablename, true).unwrap())
    };

    println!("Initializing retrograde analysis for {} and {}...", tablename, twin_tablename);

    // Match Egt sub-tables into pairs
    let mut table_pairs = Vec::new();

    for (egt_idx_a, egt_a) in file_a.egts.iter().enumerate() {
        let (stm_files, sntm_files) = get_pawn_files(egt_a.pieces());
        let twin_key = PawnKey::new(&sntm_files, &stm_files);

        let egt_idx_b = if is_symmetric {
            *file_a.egt_map.get(&twin_key).unwrap()
        } else {
            *file_b.as_ref().unwrap().egt_map.get(&twin_key).unwrap()
        };

        let table_a = RetrogradeTable {
            egt_idx: egt_idx_a,
            is_table_b: false,
        };

        let table_b = RetrogradeTable {
            egt_idx: egt_idx_b,
            is_table_b: !is_symmetric,
        };

        table_pairs.push((table_a, table_b));
    }

    // Initialization Phase
    for (table_a, table_b) in &table_pairs {
        initialize_table(&mut file_a, &mut file_b, table_a, table_b, arena);
        if !is_symmetric {
            initialize_table(&mut file_a, &mut file_b, table_b, table_a, arena);
        }
    }

    // Propagation Loop
    let mut plies = 1;
    loop {
        let mut marked_any = false;

        for (table_a, table_b) in &table_pairs {
            if plies % 2 == 1 {
                // Odd plies: propagate wins of distance `plies` to losses of distance `plies + 1`
                marked_any |= propagate_wins_to_losses(&mut file_a, &mut file_b, table_a, table_b, plies, arena);
                if !is_symmetric {
                    marked_any |= propagate_wins_to_losses(&mut file_a, &mut file_b, table_b, table_a, plies, arena);
                }
            } else {
                // Even plies: propagate losses of distance `plies` to wins of distance `plies + 1`
                marked_any |= propagate_losses_to_wins(&mut file_a, &mut file_b, table_a, table_b, plies, arena);
                if !is_symmetric {
                    marked_any |= propagate_losses_to_wins(&mut file_a, &mut file_b, table_b, table_a, plies, arena);
                }
            }
        }

        if !marked_any {
            break;
        }

        plies += 1;
    }

    // Mark all remaining 'unknown' positions as draws
    for (table_a, table_b) in &table_pairs {
        let size_a = file_a.egts[table_a.egt_idx].indexer.index_range;
        for idx in 0..size_a {
            let outcome = read_outcome(&mut file_a, &mut file_b, table_a, idx, arena);
            if outcome.is_unknown() {
                write_outcome(&mut file_a, &mut file_b, table_a, idx, MaybeDtcOutcome::DRAW, arena);
            }
        }

        if !is_symmetric {
            let size_b = file_b.as_ref().unwrap().egts[table_b.egt_idx].indexer.index_range;
            for idx in 0..size_b {
                let outcome = read_outcome(&mut file_a, &mut file_b, table_b, idx, arena);
                if outcome.is_unknown() {
                    write_outcome(&mut file_a, &mut file_b, table_b, idx, MaybeDtcOutcome::DRAW, arena);
                }
            }
        }
    }

    println!("Retrograde analysis complete! Max plies: {}", plies - 1);

    (file_a, file_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_k_k_table_generation() {
        let mut arena = Arena::new(16 * 1024 * 1024);
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "K_K", &mut arena);

        assert!(file_b.is_none());
        assert_eq!(file_a.total_positions, 462);

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.total_positions() {
            let outcome = file_a.read_by_global_index(idx, &mut arena).unwrap();
            if outcome.is_draw() {
                draw_count += 1;
            } else if outcome.is_win() {
                win_count += 1;
            } else if outcome.is_loss() {
                loss_count += 1;
            } else if outcome.is_invalid() {
                invalid_count += 1;
            }
        }

        println!("K_K table stats:");
        println!("  Draws: {}", draw_count);
        println!("  Wins: {}", win_count);
        println!("  Losses: {}", loss_count);
        println!("  Invalids: {}", invalid_count);

        assert_eq!(draw_count, 462);
        assert_eq!(win_count, 0);
        assert_eq!(loss_count, 0);
        assert_eq!(invalid_count, 0);
    }

    #[test]
    fn test_kq_k_table_generation() {
        let mut arena = Arena::new(64 * 1024 * 1024);
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "KQ_K", &mut arena);

        assert!(file_b.is_some());
        let mut file_b = file_b.unwrap();

        println!("KQ_K total positions: {}", file_a.total_positions());
        println!("K_KQ total positions: {}", file_b.total_positions());

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.total_positions() {
            let outcome = file_a.read_by_global_index(idx, &mut arena).unwrap();
            if outcome.is_draw() {
                draw_count += 1;
            } else if outcome.is_win() {
                win_count += 1;
            } else if outcome.is_loss() {
                loss_count += 1;
            } else if outcome.is_invalid() {
                invalid_count += 1;
            }
        }

        println!("KQ_K table stats:");
        println!("  Draws: {}", draw_count);
        println!("  Wins: {}", win_count);
        println!("  Losses: {}", loss_count);
        println!("  Invalids: {}", invalid_count);

        assert_eq!(draw_count, 0);
        assert_eq!(loss_count, 0);
        assert!(win_count > 0);

        let mut draw_count_b = 0;
        let mut win_count_b = 0;
        let mut loss_count_b = 0;
        let mut invalid_count_b = 0;

        for idx in 0..file_b.total_positions() {
            let outcome = file_b.read_by_global_index(idx, &mut arena).unwrap();
            if outcome.is_draw() {
                draw_count_b += 1;
            } else if outcome.is_win() {
                win_count_b += 1;
            } else if outcome.is_loss() {
                loss_count_b += 1;
            } else if outcome.is_invalid() {
                invalid_count_b += 1;
            }
        }

        println!("K_KQ table stats:");
        println!("  Draws: {}", draw_count_b);
        println!("  Wins: {}", win_count_b);
        println!("  Losses: {}", loss_count_b);
        println!("  Invalids: {}", invalid_count_b);

        assert!(draw_count_b > 0);
        assert_eq!(win_count_b, 0);
        assert!(loss_count_b > 0);
    }

    #[test]
    fn test_kr_k_table_generation() {
        let mut arena = Arena::new(64 * 1024 * 1024);
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "KR_K", &mut arena);

        assert!(file_b.is_some());
        let mut file_b = file_b.unwrap();

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.total_positions() {
            let outcome = file_a.read_by_global_index(idx, &mut arena).unwrap();
            if outcome.is_draw() {
                draw_count += 1;
            } else if outcome.is_win() {
                win_count += 1;
            } else if outcome.is_loss() {
                loss_count += 1;
            } else if outcome.is_invalid() {
                invalid_count += 1;
            }
        }

        println!("KR_K table stats:");
        println!("  Draws: {}", draw_count);
        println!("  Wins: {}", win_count);
        println!("  Losses: {}", loss_count);
        println!("  Invalids: {}", invalid_count);

        assert_eq!(draw_count, 0);
        assert_eq!(loss_count, 0);
        assert!(win_count > 0);

        let mut draw_count_b = 0;
        let mut win_count_b = 0;
        let mut loss_count_b = 0;
        let mut invalid_count_b = 0;

        for idx in 0..file_b.total_positions() {
            let outcome = file_b.read_by_global_index(idx, &mut arena).unwrap();
            if outcome.is_draw() {
                draw_count_b += 1;
            } else if outcome.is_win() {
                win_count_b += 1;
            } else if outcome.is_loss() {
                loss_count_b += 1;
            } else if outcome.is_invalid() {
                invalid_count_b += 1;
            }
        }

        println!("K_KR table stats:");
        println!("  Draws: {}", draw_count_b);
        println!("  Wins: {}", win_count_b);
        println!("  Losses: {}", loss_count_b);
        println!("  Invalids: {}", invalid_count_b);

        assert!(draw_count_b > 0);
        assert_eq!(win_count_b, 0);
        assert!(loss_count_b > 0);
    }

    #[test]
    fn test_kb_k_table_generation() {
        let mut arena = Arena::new(64 * 1024 * 1024);
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "KB_K", &mut arena);

        assert!(file_b.is_some());
        let mut file_b = file_b.unwrap();

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.total_positions() {
            let outcome = file_a.read_by_global_index(idx, &mut arena).unwrap();
            if outcome.is_draw() {
                draw_count += 1;
            } else if outcome.is_win() {
                win_count += 1;
            } else if outcome.is_loss() {
                loss_count += 1;
            } else if outcome.is_invalid() {
                invalid_count += 1;
            }
        }

        println!("KB_K table stats:");
        println!("  Draws: {}", draw_count);
        println!("  Wins: {}", win_count);
        println!("  Losses: {}", loss_count);
        println!("  Invalids: {}", invalid_count);

        assert_eq!(win_count, 0);
        assert_eq!(loss_count, 0);
        assert!(draw_count > 0);

        let mut draw_count_b = 0;
        let mut win_count_b = 0;
        let mut loss_count_b = 0;
        let mut invalid_count_b = 0;

        for idx in 0..file_b.total_positions() {
            let outcome = file_b.read_by_global_index(idx, &mut arena).unwrap();
            if outcome.is_draw() {
                draw_count_b += 1;
            } else if outcome.is_win() {
                win_count_b += 1;
            } else if outcome.is_loss() {
                loss_count_b += 1;
            } else if outcome.is_invalid() {
                invalid_count_b += 1;
            }
        }

        println!("K_KB table stats:");
        println!("  Draws: {}", draw_count_b);
        println!("  Wins: {}", win_count_b);
        println!("  Losses: {}", loss_count_b);
        println!("  Invalids: {}", invalid_count_b);

        assert_eq!(win_count_b, 0);
        assert_eq!(loss_count_b, 0);
        assert!(draw_count_b > 0);
    }

    #[test]
    fn test_kn_k_table_generation() {
        let mut arena = Arena::new(64 * 1024 * 1024);
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "KN_K", &mut arena);

        assert!(file_b.is_some());
        let mut file_b = file_b.unwrap();

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.total_positions() {
            let outcome = file_a.read_by_global_index(idx, &mut arena).unwrap();
            if outcome.is_draw() {
                draw_count += 1;
            } else if outcome.is_win() {
                win_count += 1;
            } else if outcome.is_loss() {
                loss_count += 1;
            } else if outcome.is_invalid() {
                invalid_count += 1;
            }
        }

        println!("KN_K table stats:");
        println!("  Draws: {}", draw_count);
        println!("  Wins: {}", win_count);
        println!("  Losses: {}", loss_count);
        println!("  Invalids: {}", invalid_count);

        assert_eq!(win_count, 0);
        assert_eq!(loss_count, 0);
        assert!(draw_count > 0);

        let mut draw_count_b = 0;
        let mut win_count_b = 0;
        let mut loss_count_b = 0;
        let mut invalid_count_b = 0;

        for idx in 0..file_b.total_positions() {
            let outcome = file_b.read_by_global_index(idx, &mut arena).unwrap();
            if outcome.is_draw() {
                draw_count_b += 1;
            } else if outcome.is_win() {
                win_count_b += 1;
            } else if outcome.is_loss() {
                loss_count_b += 1;
            } else if outcome.is_invalid() {
                invalid_count_b += 1;
            }
        }

        println!("K_KN table stats:");
        println!("  Draws: {}", draw_count_b);
        println!("  Wins: {}", win_count_b);
        println!("  Losses: {}", loss_count_b);
        println!("  Invalids: {}", invalid_count_b);

        assert_eq!(win_count_b, 0);
        assert_eq!(loss_count_b, 0);
        assert!(draw_count_b > 0);
    }

    #[test]
    fn test_kp_k_table_generation() {
        let mut arena = Arena::new(64 * 1024 * 1024);
        let (mut file_a, file_b) = retrograde_analysis(&std::env::temp_dir(), "KP_K", &mut arena);

        assert!(file_b.is_some());
        let mut file_b = file_b.unwrap();

        let mut draw_count = 0;
        let mut win_count = 0;
        let mut loss_count = 0;
        let mut invalid_count = 0;

        for idx in 0..file_a.total_positions() {
            let outcome = file_a.read_by_global_index(idx, &mut arena).unwrap();
            if outcome.is_draw() {
                draw_count += 1;
            } else if outcome.is_win() {
                win_count += 1;
            } else if outcome.is_loss() {
                loss_count += 1;
            } else if outcome.is_invalid() {
                invalid_count += 1;
            }
        }

        println!("KP_K table stats:");
        println!("  Draws: {}", draw_count);
        println!("  Wins: {}", win_count);
        println!("  Losses: {}", loss_count);
        println!("  Invalids: {}", invalid_count);

        assert!(draw_count > 0);

        let mut draw_count_b = 0;
        let mut win_count_b = 0;
        let mut loss_count_b = 0;
        let mut invalid_count_b = 0;

        for idx in 0..file_b.total_positions() {
            let outcome = file_b.read_by_global_index(idx, &mut arena).unwrap();
            if outcome.is_draw() {
                draw_count_b += 1;
            } else if outcome.is_win() {
                win_count_b += 1;
            } else if outcome.is_loss() {
                loss_count_b += 1;
            } else if outcome.is_invalid() {
                invalid_count_b += 1;
            }
        }

        println!("K_KP table stats:");
        println!("  Draws: {}", draw_count_b);
        println!("  Wins: {}", win_count_b);
        println!("  Losses: {}", loss_count_b);
        println!("  Invalids: {}", invalid_count_b);

        assert!(draw_count_b > 0);
    }
}
