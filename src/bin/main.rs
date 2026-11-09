use chess_egt::{
    ConversionType, DtcOutcome, EgtError, EgtGenerator, EgtProber, EgtStatistics, EgtVerifier,
    resolve_selectors,
};
use clap::Parser;
use shakmaty::fen::Fen;
use shakmaty::{CastlingMode, Position, PositionError};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "chess-egt")]
#[command(about = "Chess Endgame Tablebase Tool", long_about = None)]
#[command(group(clap::ArgGroup::new("action")
    .args(["generate", "verify", "stats", "cache_stats", "list", "position"])
    .multiple(false)))]
struct Cli {
    #[arg(long, default_value = "./ggegt")]
    path: PathBuf,

    /// Additional path used to look up dependency tables, in addition to --path.
    /// If not given, --path is used for dependencies as well.
    #[arg(long, conflicts_with_all = ["stats", "cache_stats"])]
    input_path: Option<PathBuf>,

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

    /// Cache missing table metadata and rebuild selected hierarchy summaries.
    #[arg(long, value_name = "SELECTOR", num_args = 1..)]
    cache_stats: Vec<String>,

    /// List canonical table names by piece count, pawn count, then Q/R/B/N/P order.
    #[arg(long, value_name = "SELECTOR", num_args = 1..)]
    list: Vec<String>,

    /// Skip the internal consistency check performed after generation.
    #[arg(long, requires = "generate", conflicts_with_all = ["verify", "stats", "cache_stats", "list", "position"])]
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
        generator.generate_selected(&selectors)?;
    } else if !cli.verify.is_empty() {
        let selectors: Vec<_> = cli.verify.iter().map(String::as_str).collect();
        let mut verifier = EgtVerifier::new(&cli.path)?;
        verifier.set_threads(usize::from(cli.threads));
        if let Some(ref input) = cli.input_path {
            verifier.set_input_path(input)?;
        }
        verifier.verify_selected(&selectors)?;
    } else if !cli.stats.is_empty() {
        let selectors: Vec<_> = cli.stats.iter().map(String::as_str).collect();
        let stats = EgtStatistics::new(&cli.path)?.stats(&selectors)?;
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else if !cli.cache_stats.is_empty() {
        let selectors: Vec<_> = cli.cache_stats.iter().map(String::as_str).collect();
        let stats = EgtStatistics::new(&cli.path)?.cache_stats(&selectors)?;
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else if let Some(ref fen) = cli.position {
        let fen_obj: Fen = fen
            .parse()
            .map_err(|e| EgtError::InvalidPosition(format!("invalid FEN: {e:?}")))?;
        let position = fen_obj
            .into_position(CastlingMode::Standard)
            .or_else(PositionError::ignore_impossible_check)
            .map_err(|e| EgtError::InvalidPosition(format!("invalid position: {e:?}")))?;
        let prober = configured_prober(&cli)?;
        let outcome = prober.probe(&position)?;

        print!("{:?}", position.board());
        println!("({} to move)", position.turn());
        println!("{}", format_outcome(outcome));
    } else {
        println!("No action specified. Use --help for options.");
    }
    Ok(())
}

fn format_outcome(outcome: DtcOutcome) -> String {
    let (result, conversion, plies) = match outcome {
        DtcOutcome::Win(ct, n) => ("Win", ct, n),
        DtcOutcome::Loss(ct, n) => ("Loss", ct, n),
        DtcOutcome::Draw => return "Draw".into(),
    };
    let conversion = match conversion {
        ConversionType::Checkmate => "checkmate",
        ConversionType::Capture => "capture",
        ConversionType::Promotion => "promotion",
    };
    let unit = if plies == 1 { "ply" } else { "plies" };
    format!("{result} - {conversion}, DTC {plies} {unit}")
}

fn configured_prober(cli: &Cli) -> Result<EgtProber, EgtError> {
    let mut prober = EgtProber::new(&cli.path)?;
    if let Some(ref input) = cli.input_path {
        prober.set_input_path(input)?;
    }
    Ok(prober)
}

fn configure_generator(generator: &mut EgtGenerator, cli: &Cli) -> Result<(), EgtError> {
    generator.set_threads(usize::from(cli.threads));
    if let Some(ref input) = cli.input_path {
        generator.set_input_path(input)?;
    }
    generator.set_verify(!cli.noverify);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    const ACTIONS: [&str; 5] = [
        "--generate",
        "--verify",
        "--stats",
        "--cache-stats",
        "--list",
    ];
    const FEN: &str = "8/8/8/8/8/8/3K4/k7 w - - 0 1";

    #[test]
    fn outcomes_report_conversion_and_exact_plies() {
        use ConversionType::*;
        use DtcOutcome::*;
        for (outcome, expected) in [
            (Draw, "Draw"),
            (Win(Capture, 1), "Win - capture, DTC 1 ply"),
            (Win(Capture, 2), "Win - capture, DTC 2 plies"),
            (Win(Promotion, 8), "Win - promotion, DTC 8 plies"),
            (Win(Checkmate, 3), "Win - checkmate, DTC 3 plies"),
            (Loss(Checkmate, 0), "Loss - checkmate, DTC 0 plies"),
            (Loss(Capture, 2), "Loss - capture, DTC 2 plies"),
            (Loss(Promotion, 1), "Loss - promotion, DTC 1 ply"),
        ] {
            assert_eq!(format_outcome(outcome), expected);
        }
    }

    #[test]
    fn selector_actions_accept_multiple_selectors_without_a_path() {
        for action in ACTIONS {
            let cli = Cli::try_parse_from(["chess-egt", action, "3", "4"]).unwrap();
            assert_eq!(cli.path, PathBuf::from("./ggegt"));
            let selectors = match action {
                "--generate" => cli.generate,
                "--verify" => cli.verify,
                "--stats" => cli.stats,
                "--cache-stats" => cli.cache_stats,
                "--list" => cli.list,
                _ => unreachable!(),
            };
            assert_eq!(selectors, ["3", "4"]);
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
    fn statistics_actions_reject_input_overlays() {
        for action in ["--stats", "--cache-stats"] {
            let error = Cli::try_parse_from(["chess-egt", action, "3", "--input-path", "other"])
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
            assert!(Cli::try_parse_from(["chess-egt", action, "3", "--noverify"]).is_err());
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
