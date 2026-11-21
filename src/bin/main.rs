use chess_egt::{DtcOutcome, EgtError, Tablebase, parse_position, resolve_selectors};
use clap::{Args, Parser, Subcommand};
use shakmaty::Position;
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(name = "chess-egt", about = "Chess Endgame Tablebase Tool")]
struct Cli {
    /// Base tablebase directory or object-store URL.
    #[arg(long, global = true, default_value = "./ggegt")]
    path: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate selected tables and corresponding twin tables (other dependencies must exist or be selected).
    Generate(WorkArgs),
    /// Verify the selected tables without generating them.
    Verify(WorkArgs),
    /// Print aggregate statistics for the selected tables as JSON.
    Stats(Selectors),
    /// Cache missing table metadata and rebuild selected hierarchy summaries.
    CacheStats(Selectors),
    /// List canonical table names by piece count, pawn count, then Q/R/B/N/P order.
    List(Selectors),
    /// Probe a quoted FEN position.
    Probe {
        #[arg(value_name = "FEN")]
        fen: String,
        #[command(flatten)]
        input: InputArgs,
    },
}

#[derive(Debug, Args)]
struct Selectors {
    #[arg(value_name = "SELECTOR", required = true)]
    values: Vec<String>,
}

impl Selectors {
    fn as_strs(&self) -> Vec<&str> {
        self.values.iter().map(String::as_str).collect()
    }
}

#[derive(Debug, Args)]
struct InputArgs {
    /// Search this input location before falling back to --path.
    #[arg(long)]
    input_path: Option<PathBuf>,
}

impl InputArgs {
    fn tablebase(&self, path: &Path) -> Result<Tablebase, EgtError> {
        let mut tablebase = Tablebase::new(path)?;
        if let Some(input) = &self.input_path {
            tablebase.set_input_path(input)?;
        }
        Ok(tablebase)
    }
}

#[derive(Debug, Args)]
struct WorkArgs {
    #[command(flatten)]
    selectors: Selectors,
    #[command(flatten)]
    input: InputArgs,

    /// Number of workers (0 uses Rayon's default).
    #[arg(long, default_value_t = 0)]
    threads: u16,
}

impl WorkArgs {
    fn tablebase(&self, path: &Path) -> Result<Tablebase, EgtError> {
        let mut tablebase = self.input.tablebase(path)?;
        tablebase.set_threads(usize::from(self.threads));
        Ok(tablebase)
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), EgtError> {
    let Cli { path, command } = Cli::parse();

    match command {
        Command::Generate(args) => {
            args.tablebase(&path)?.generate(&args.selectors.as_strs())?;
        }
        Command::Verify(args) => args.tablebase(&path)?.verify(&args.selectors.as_strs())?,
        Command::Stats(selectors) => {
            let stats = Tablebase::new(&path)?.stats(&selectors.as_strs())?;
            println!("{}", serde_json::to_string_pretty(&stats)?);
        }
        Command::CacheStats(selectors) => {
            let stats = Tablebase::new(&path)?.cache_stats(&selectors.as_strs())?;
            println!("{}", serde_json::to_string_pretty(&stats)?);
        }
        Command::List(selectors) => {
            for id in resolve_selectors(&selectors.as_strs())? {
                println!("{id}");
            }
        }
        Command::Probe { fen, input } => {
            let position = parse_position(&fen)?;
            let outcome = input.tablebase(&path)?.probe(&position)?;

            print!("{:?}", position.board());
            println!("({} to move)", position.turn());
            println!("{}", format_outcome(outcome));
        }
    }
    Ok(())
}

fn format_outcome(outcome: DtcOutcome) -> String {
    let (result, conversion, plies) = match outcome {
        DtcOutcome::Win(ct, n) => ("Win", ct, n),
        DtcOutcome::Loss(ct, n) => ("Loss", ct, n),
        DtcOutcome::Draw => return "Draw".into(),
    };
    let conversion = conversion.as_str();
    let unit = if plies == 1 { "ply" } else { "plies" };
    format!("{result} - {conversion}, DTC {plies} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chess_egt::ConversionType;
    use clap::error::ErrorKind;

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
    fn selector_commands_require_at_least_one_selector() {
        for command in ["generate", "verify", "stats", "cache-stats", "list"] {
            let err = Cli::try_parse_from(["chess-egt", command]).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
            assert!(Cli::try_parse_from(["chess-egt", command, "3/", "4/"]).is_ok());
        }
    }

    #[test]
    fn statistics_commands_reject_input_overlays() {
        for command in ["stats", "cache-stats"] {
            let err =
                Cli::try_parse_from(["chess-egt", command, "3/", "--input-path", "other"])
                    .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::UnknownArgument);
        }
    }
}
