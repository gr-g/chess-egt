use clap::Parser;
use chess_egt::{EgtGenerator, EgtProber};
use std::path::PathBuf;
use std::str::FromStr;

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

    #[arg(long)]
    save_wdl_oneside: bool,

    #[arg(long)]
    save_dtc_oneside: bool,

    position: Option<String>,
}

fn main() {
    let cli = Cli::parse();

    if let Some(table) = cli.generate {
        let mut g = EgtGenerator::new(&cli.path);
        g.set_wdl_oneside(cli.save_wdl_oneside);
        g.set_dtc_oneside(cli.save_dtc_oneside);
        g.generate(&table);
    } else if cli.generate_all_3 {
        println!("Generating all 3-men tables...");
    } else if let Some(fen) = cli.position {
        let board = chess::Board::from_str(&fen).expect("Invalid FEN");
        let _prober = EgtProber::new(&cli.path);
        println!("Probing position: {}", board);
    } else {
        println!("No action specified. Use --help for options.");
    }
}
