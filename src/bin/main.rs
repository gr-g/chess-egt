use clap::Parser;
use chess_egt::{DtcOutcome, ConversionType, EgtGenerator, EgtProber};
use shakmaty::{CastlingMode, Position};
use shakmaty::fen::Fen;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "chess-egt")]
#[command(about = "Chess Endgame Tablebase Tool", long_about = None)]
struct Cli {
    #[arg(long)]
    path: PathBuf,

    #[arg(long)]
    memory: Option<String>,

    #[arg(long)]
    generate: Option<String>,

    #[arg(long)]
    generate_all_3: bool,

    #[arg(long)]
    generate_all_4: bool,

    #[arg(long)]
    generate_all_5: bool,

    position: Option<String>,
}

fn main() {
    let cli = Cli::parse();

    if let Some(endgame) = cli.generate {
        let g = EgtGenerator::new(&cli.path);
        match g.generate(&endgame) {
            Err(_) => println!("Failed to generate endgame"),
            _ => {},
        };
    } else if cli.generate_all_3 {
        generate_all(3, &cli.path);
    } else if cli.generate_all_4 {
        generate_all(4, &cli.path);
    } else if cli.generate_all_5 {
        generate_all(5, &cli.path);
    } else if let Some(fen) = cli.position {
        let fen_obj: Fen = fen.parse().expect("Invalid FEN");
        let position = fen_obj.into_position(CastlingMode::Standard).expect("Invalid position");
        let mut prober = EgtProber::new(&cli.path);
        let outcome = prober.probe(&position).expect("Failed to query position");

        let message = match outcome {
            DtcOutcome::Win(ct, n) => {
                let conversion_str = match (ct, n % 2) {
                    (ConversionType::Checkmate, _) => "Checkmate",
                    (ConversionType::Capture, 1) => "A capture converting to a checkmated/winning position",
                    (ConversionType::Promotion, 1) => "A promotion converting to a checkmated/winning position ",
                    (ConversionType::Capture, _) => "A capture of your own piece converting to a winning position",
                    (ConversionType::Promotion, _) => "A promotion of an opponent's pawn converting to a winning position",
                };
                if n / 2 == 1 {
                    format!("Win - {} can be played on this move", conversion_str)
                } else {
                    format!("Win - {} can be forced in {} moves ({} plies)", conversion_str, (n+1) / 2, n)
                }
            },
            DtcOutcome::Draw => {
                format!("Draw")
            },
            DtcOutcome::Loss(ct, n) => {
                let conversion_str = match (ct, n % 2) {
                    (ConversionType::Checkmate, _) => "Checkmate cannot be avoided",
                    (ConversionType::Capture, 0) => "A capture converting to a checkmated/losing position cannot be avoided",
                    (ConversionType::Promotion, 0) => "A promotion converting to a checkmated/losing position cannot be avoided",
                    (ConversionType::Capture, _) => "A forced capture of an opponent's piece converting to a losing position cannot be avoided",
                    (ConversionType::Promotion, _) => "A forced promotion of your pawn converting to a winning position cannot be avoided",
                };
                if n / 2 == 0 {
                    format!("Loss - {} on this move", conversion_str)
                } else {
                    format!("Loss - {} in {} moves ({} plies)", conversion_str, n / 2, n)
                }
            }
        };
        print!("{:?}", position.board());
        println!("({} to move)", position.turn());
        println!("{}", message);
    } else {
        println!("No action specified. Use --help for options.");
    }
}

fn generate_all(n: usize, path: &std::path::Path) {
    println!("Generating all {}-pieces tables...", n);
    let endgames = EgtGenerator::list_n_pieces_endgames(n);
    let g = EgtGenerator::new(path);
    let start_all = std::time::Instant::now();
    let mut unique_positions = 0;
    let mut total_bytes = 0;
    let mut worst_compression_endgame = String::new();
    let mut worst_bits_per_pos = 0.0;

    for endgame in endgames {
        match g.generate(&endgame) {
            Ok((stats_a, stats_b_opt)) => {
                let mut process_stats = |stats: chess_egt::EgtFileStats| {
                    let unique_pos = stats.win + stats.draw + stats.loss;
                    unique_positions += unique_pos;
                    total_bytes += stats.bytes;
                    let bits_per_pos = if unique_pos > 0 {
                        (stats.bytes as f64 * 8.0) / unique_pos as f64
                    } else {
                        0.0
                    };
                    if bits_per_pos > worst_bits_per_pos {
                        worst_bits_per_pos = bits_per_pos;
                        worst_compression_endgame = stats.endgame;
                    }
                };
                process_stats(stats_a);
                if let Some(stats_b) = stats_b_opt {
                    process_stats(stats_b);
                }
            }
            Err(_) => {
                println!("Failed to generate endgame {}", endgame);
            }
        }
    }
    let duration = start_all.elapsed();
    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    let format_dur = format!("{:02}h{:02}m{:02}s", hours, minutes, seconds);

    let size_mb = total_bytes as f64 / (1024.0 * 1024.0);
    let avg_bits_per_pos = if unique_positions > 0 {
        (total_bytes as f64 * 8.0) / unique_positions as f64
    } else {
        0.0
    };

    println!("\n================================================================================");
    println!("Generated all {}-pieces endgames, corresponding to {} unique positions.", n, unique_positions);
    println!("Time: {}.", format_dur);
    if !worst_compression_endgame.is_empty() {
        println!(
            "Size on disk: {:.2}MiB ({:.2} bits/pos on average; lowest compression for {}: {:.2} bits/pos).",
            size_mb, avg_bits_per_pos, worst_compression_endgame, worst_bits_per_pos
        );
    } else {
        println!("Size on disk: {:.2}MiB ({:.2} bits/pos on average).", size_mb, avg_bits_per_pos);
    }
    println!("================================================================================");
}
