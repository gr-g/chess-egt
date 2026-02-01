use chess::Color;
use clap::Parser;
use chess_egt::{egt, EgtGenerator, EgtProber};
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Parser)]
#[command(name = "chess-egt")]
#[command(about = "Chess Endgame Table Tool", long_about = None)]
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

use egt::Egt;

fn main() {
    //let cli = Cli::parse();

    //if let Some(table) = cli.generate {
    //    let mut g = EgtGenerator::new(&cli.path);
    //    g.set_wdl_oneside(cli.save_wdl_oneside);
    //    g.set_dtc_oneside(cli.save_dtc_oneside);
    //    g.generate(&table);
    //} else if cli.generate_all_3 {
    //    println!("Generating all 3-men tables...");
    //} else if let Some(fen) = cli.position {
    //    let board = chess::Board::from_str(&fen).expect("Invalid FEN");
    //   let prober = EgtProber::new(&cli.path);
    //    println!("WDL: {:?}", prober.probe_wdl(&board));
    //    println!("DTC: {:?}", prober.probe_dtc(&board));
    //} else {
    //    println!("No action specified. Use --help for options.");
    //}

    let egt = Egt::from_tablename("KQR_KQQQ").unwrap();
    println!("KQR_KQQQ number of combinations: {}", egt.index_range());
    println!();

    let egt = Egt::from_tablename("KPaPa_K").unwrap();
    println!("KPaPa_K number of combinations: {}", egt.index_range());
    println!();

    let egt = Egt::from_tablename("KQR_KQR").unwrap();
    println!("KQR_KQR number of combinations: {}", egt.index_range());
    println!();

    let egt = Egt::from_tablename("KQ_K").unwrap();
    println!("KQ_K number of combinations: {}", egt.index_range());
    println!();

    let egt = Egt::from_tablename("KPa_KPc").unwrap();
    println!("KPa_KPc number of combinations: {}", egt.index_range());
    println!();

    let egt = Egt::from_tablename("KPc_KPb").unwrap();
    println!("KPc_KPb number of combinations: {}", egt.index_range());
    println!();

    let egt = Egt::from_tablename("KPdPe_KPePePe").unwrap();
    println!("KPdPe_KPePePe number of combinations: {}", egt.index_range());
    println!();

    let egt = Egt::from_tablename("KPdPf_KPePe").unwrap();
    println!("KPdPf_KPePe number of combinations: {}", egt.index_range());
    println!();

    let egt = Egt::from_tablename("KPePe_KPdPf").unwrap();
    println!("KPePe_KPdPf number of combinations: {}", egt.index_range());
    println!();
}
