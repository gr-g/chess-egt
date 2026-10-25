use chess_egt::{
    AggregateStats, ConversionType, DtcOutcome, EgtError, EgtGenerator, EgtProber,
    resolve_selectors,
};
use clap::Parser;
use shakmaty::fen::Fen;
use shakmaty::{CastlingMode, Position};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "chess-egt")]
#[command(about = "Chess Endgame Tablebase Tool", long_about = None)]
#[command(group(clap::ArgGroup::new("action")
    .args(["generate", "verify", "stats", "list", "position"])
    .multiple(false)))]
struct Cli {
    #[arg(long, default_value = "./ggegt")]
    path: PathBuf,

    /// Additional path used to look up dependency tables, in addition to --path.
    /// If not given, --path is used for dependencies as well.
    #[arg(long)]
    input_path: Option<PathBuf>,

    #[arg(long)]
    memory: Option<String>,

    /// Number of generation and verification workers (0 uses Rayon's default).
    #[arg(long, default_value_t = 0)]
    threads: u16,

    /// Generate selected tables and corresponding twin tables (other dependencies must exist or be selected).
    #[arg(long, value_name = "SELECTOR", num_args = 1..)]
    generate: Vec<String>,

    /// Verify the selected tables without generating them.
    #[arg(long, value_name = "SELECTOR", num_args = 1..)]
    verify: Vec<String>,

    /// Print aggregate statistics for the selected tables as JSON.
    #[arg(long, value_name = "SELECTOR", num_args = 1..)]
    stats: Vec<String>,

    /// List canonical table names.
    #[arg(long, value_name = "SELECTOR", num_args = 1..)]
    list: Vec<String>,

    /// Skip the internal consistency check performed after generation.
    #[arg(long, requires = "generate", conflicts_with_all = ["verify", "stats", "list", "position"])]
    noverify: bool,

    /// Probe a quoted FEN position (mutually exclusive with selector actions).
    #[arg(value_name = "FEN")]
    position: Option<String>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), EgtError> {
    let cli = Cli::parse();

    if !cli.list.is_empty() {
        let selectors: Vec<_> = cli.list.iter().map(String::as_str).collect();
        for id in resolve_selectors(&selectors)? {
            println!("{}", id.tablename());
        }
    } else if !cli.generate.is_empty() {
        let selectors: Vec<_> = cli.generate.iter().map(String::as_str).collect();
        let mut generator = EgtGenerator::new(&cli.path)?;
        configure_generator(&mut generator, &cli)?;
        let start = std::time::Instant::now();
        let tables = generator.generate_selected(&selectors)?;
        let mut total = AggregateStats {
            selected_tables: tables.len(),
            ..Default::default()
        };
        for stats in &tables {
            total.add(stats);
        }
        let lowest_compression = tables
            .iter()
            .filter(|stats| stats.unique_positions > 0)
            .map(|stats| {
                (
                    stats.tablename.as_str(),
                    stats.bytes as f64 * 8.0 / stats.unique_positions as f64,
                )
            })
            .max_by(|a, b| a.1.total_cmp(&b.1));
        println!(
            "{}",
            generation_summary(&total, start.elapsed(), lowest_compression)
        );
    } else if !cli.verify.is_empty() {
        let selectors: Vec<_> = cli.verify.iter().map(String::as_str).collect();
        let mut prober = configured_prober(&cli)?;
        prober.verify_selected(&selectors)?;
    } else if !cli.stats.is_empty() {
        let selectors: Vec<_> = cli.stats.iter().map(String::as_str).collect();
        let prober = configured_prober(&cli)?;
        let stats = prober.stats(&selectors)?;
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else if let Some(ref fen) = cli.position {
        let fen_obj: Fen = fen
            .parse()
            .map_err(|e| EgtError::InvalidPosition(format!("invalid FEN: {e:?}")))?;
        let position = fen_obj
            .into_position(CastlingMode::Standard)
            .map_err(|e| EgtError::InvalidPosition(format!("invalid position: {e:?}")))?;
        let mut prober = configured_prober(&cli)?;
        let outcome = prober.probe(&position)?;

        let message = match outcome {
            DtcOutcome::Win(ct, n) => {
                let conversion_str = match (ct, n % 2) {
                    (ConversionType::Checkmate, _) => "Checkmate",
                    (ConversionType::Capture, 1) => {
                        "A capture converting to a checkmated/winning position"
                    }
                    (ConversionType::Promotion, 1) => {
                        "A promotion converting to a checkmated/winning position"
                    }
                    (ConversionType::Capture, _) => {
                        "A capture of your own piece converting to a winning position"
                    }
                    (ConversionType::Promotion, _) => {
                        "A promotion of an opponent's pawn converting to a winning position"
                    }
                };
                if n.div_ceil(2) == 1 {
                    format!("Win - {} can be played on this move", conversion_str)
                } else {
                    format!(
                        "Win - {} can be forced in {} moves ({} plies)",
                        conversion_str,
                        n.div_ceil(2),
                        n
                    )
                }
            }
            DtcOutcome::Draw => "Draw".to_string(),
            DtcOutcome::Loss(ct, n) => {
                let conversion_str = match (ct, n % 2) {
                    (ConversionType::Checkmate, _) => "Checkmate cannot be avoided",
                    (ConversionType::Capture, 0) => {
                        "A capture converting to a checkmated/losing position cannot be avoided"
                    }
                    (ConversionType::Promotion, 0) => {
                        "A promotion converting to a checkmated/losing position cannot be avoided"
                    }
                    (ConversionType::Capture, _) => {
                        "A forced capture of an opponent's piece converting to a losing position cannot be avoided"
                    }
                    (ConversionType::Promotion, _) => {
                        "A forced promotion of your pawn converting to a winning position cannot be avoided"
                    }
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
    Ok(())
}

fn generation_summary(
    total: &AggregateStats,
    elapsed: std::time::Duration,
    lowest_compression: Option<(&str, f64)>,
) -> String {
    let seconds = elapsed.as_secs();
    let average_compression = if total.unique_positions > 0 {
        total.bytes as f64 * 8.0 / total.unique_positions as f64
    } else {
        0.0
    };
    let mut summary = format!(
        "Generated {} tables: {} unique positions, {:.2} MiB in {:02}h{:02}m{:02}s.\nAverage compression: {:.2} bits/pos.",
        total.loaded_tables,
        total.unique_positions,
        total.bytes as f64 / (1024.0 * 1024.0),
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
        average_compression,
    );
    if let Some((tablename, bits_per_pos)) = lowest_compression {
        summary.push_str(&format!(
            " Lowest compression ({tablename}): {bits_per_pos:.2} bits/pos."
        ));
    }
    summary
}

fn configured_prober(cli: &Cli) -> Result<EgtProber, EgtError> {
    let mut prober = EgtProber::new(&cli.path)?;
    prober.with_threads(usize::from(cli.threads));
    if let Some(ref input) = cli.input_path {
        prober.with_input_path(input)?;
    }
    Ok(prober)
}

fn configure_generator(generator: &mut EgtGenerator, cli: &Cli) -> Result<(), EgtError> {
    generator.with_threads(usize::from(cli.threads));
    if let Some(ref input) = cli.input_path {
        generator.with_input_path(input)?;
    }
    generator.with_verify(!cli.noverify);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    const ACTIONS: [&str; 4] = ["--generate", "--verify", "--stats", "--list"];
    const FEN: &str = "8/8/8/8/8/8/3K4/k7 w - - 0 1";

    #[test]
    fn selector_actions_accept_multiple_selectors_without_a_path() {
        for action in ACTIONS {
            let cli = Cli::try_parse_from(["chess-egt", action, "3", "KQ_K"]).unwrap();
            assert_eq!(cli.path, PathBuf::from("./ggegt"));
            let selectors = match action {
                "--generate" => cli.generate,
                "--verify" => cli.verify,
                "--stats" => cli.stats,
                "--list" => cli.list,
                _ => unreachable!(),
            };
            assert_eq!(selectors, ["3", "KQ_K"]);
        }
    }

    #[test]
    fn actions_conflict_with_each_other_and_fen() {
        for (i, action) in ACTIONS.iter().enumerate() {
            for other in &ACTIONS[i + 1..] {
                let error =
                    Cli::try_parse_from(["chess-egt", action, "KQ_K", other, "KR_K"]).unwrap_err();
                assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
            }
            // FEN comes first because a trailing positional value would be
            // consumed as another selector by a variadic action argument.
            let error = Cli::try_parse_from(["chess-egt", FEN, action, "KQ_K"]).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
        }
    }

    #[test]
    fn selector_actions_require_at_least_one_selector() {
        for action in ACTIONS {
            assert!(Cli::try_parse_from(["chess-egt", action]).is_err());
        }
    }

    #[test]
    fn generation_options_and_fen_probing_are_preserved() {
        let cli = Cli::try_parse_from([
            "chess-egt",
            "--generate",
            "KQ_K",
            "KR_K",
            "--path",
            "out",
            "--input-path",
            "in",
            "--threads",
            "2",
            "--noverify",
        ])
        .unwrap();
        assert_eq!(cli.generate, ["KQ_K", "KR_K"]);
        assert_eq!(cli.path, PathBuf::from("out"));
        assert_eq!(cli.input_path, Some(PathBuf::from("in")));
        assert_eq!(cli.threads, 2);
        assert!(cli.noverify);
        let cli = Cli::try_parse_from(["chess-egt", FEN]).unwrap();
        assert_eq!(cli.position.as_deref(), Some(FEN));
        assert!(Cli::try_parse_from(["chess-egt", "--verify", "KQ_K", "--noverify"]).is_err());
        for flag in ["--generate-all-3", "--generate-all-4", "--generate-all-5"] {
            assert!(Cli::try_parse_from(["chess-egt", flag]).is_err());
        }
    }
}
