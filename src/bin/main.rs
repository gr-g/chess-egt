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

    if let Some(table) = cli.generate {
        let g = EgtGenerator::new(&cli.path);
        g.generate(&table);
    } else if cli.generate_all_3 {
        println!("Generating all 3-men tables...");
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
                    format!("Win - {} can be forced in {} moves", conversion_str, n / 2)
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
                if n / 2 == 1 {
                    format!("Loss - {} on this move", conversion_str)
                } else {
                    format!("Loss - {} in {} moves", conversion_str, n / 2)
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
