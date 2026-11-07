use std::path::Path;

use crate::egt_file::SharedFileCache;
use crate::storage::Location;
use crate::{
    EgtError, EgtFileReader, EgtResult, EgtStats, GenerationPlan, TablePair, resolve_selectors,
    retrograde, verification,
};

pub struct EgtGenerator {
    base_path: Location,
    cache_size: Option<usize>,
    input_path: Option<Location>,
    verify: bool,
    threads: usize,
}

impl EgtGenerator {
    /// Resolves a local output directory or object-store URL once.
    ///
    /// Returns an error for invalid locations or backend initialization failures.
    /// Read-only outputs are rejected when generation starts. This is a
    /// synchronous API; async applications should use `spawn_blocking`.
    pub fn new(path: impl AsRef<Path>) -> EgtResult<Self> {
        Ok(Self::from_location(Location::parse(path)?))
    }

    pub(crate) fn from_location(location: Location) -> Self {
        Self {
            base_path: location,
            cache_size: None,
            input_path: None,
            verify: true,
            threads: 0,
        }
    }

    /// Sets generation and verification workers. Zero (the default) uses Rayon's default.
    pub fn with_threads(&mut self, threads: usize) {
        self.threads = threads;
    }

    /// Sets the shared dependency-frame cache budget in bytes.
    /// Generation working arrays are allocated separately.
    pub fn with_cache_size(&mut self, n: usize) {
        self.cache_size = Some(n);
    }

    /// Resolves an additional local directory or object-store URL for dependencies.
    /// Input is searched first; output is tried only if the input file is missing.
    /// If not set, only the output location is consulted.
    ///
    /// Returns an error if the location cannot be resolved, leaving the previous
    /// setting unchanged. This synchronous API should run in `spawn_blocking`
    /// when called from an async application.
    pub fn with_input_path(&mut self, path: impl AsRef<Path>) -> EgtResult<()> {
        self.input_path = Some(Location::parse(path)?);
        Ok(())
    }

    /// Enables or disables the internal consistency check performed after
    /// generation. Enabled by default.
    pub fn with_verify(&mut self, verify: bool) {
        self.verify = verify;
    }

    /// Generates a logical selector and its quiet-move twins.
    /// Capture/promotion dependencies must already exist or be explicitly selected.
    pub fn generate(&self, selector: &str) -> EgtResult<Vec<EgtStats>> {
        self.generate_selected(&[selector])
    }

    /// Deduplicates selectors into twin-pair jobs, ordered by pieces then pawns.
    /// Reuses one Rayon pool and shares dependency frames with verification.
    /// The cache is released between material families, with LRU eviction when
    /// an assigned cache-size limit is configured. Completed pairs are
    /// published and optionally verified before the next job.
    pub fn generate_selected(&self, selectors: &[&str]) -> EgtResult<Vec<EgtStats>> {
        let plan = GenerationPlan::new(resolve_selectors(selectors)?)?;
        self.base_path.ensure_format()?;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(self.threads)
            .build()
            .map_err(|_| EgtError::Internal("failed to build generation thread pool"))?;
        let reader =
            SharedFileCache::new(&self.base_path, self.input_path.as_ref(), self.cache_size);
        let mut results = Vec::new();
        let mut previous_material = None;
        println!(
            "Generating {} twin-pair jobs using {} threads.",
            plan.pairs.len(),
            pool.current_num_threads()
        );
        pool.install(|| -> EgtResult<()> {
            for pair in plan.pairs {
                let material = material_family(&pair);
                if previous_material.as_ref() != Some(&material) {
                    reader.clear()?;
                    previous_material = Some(material);
                }
                let start = std::time::Instant::now();
                let pair_stats = retrograde::generate_pair(&self.base_path, &pair, &reader)?;
                if self.verify {
                    let names: Vec<_> = pair_stats
                        .iter()
                        .map(|stats| stats.tablename.clone())
                        .collect();
                    let mut verification = Vec::new();
                    // Generation and verification share dependency frames. Pin
                    // both outputs so input artifacts cannot shadow either one.
                    for name in &names {
                        let file = EgtFileReader::open_in(&self.base_path, name)?;
                        verification.push(file.egt().clone());
                        reader.insert(file)?;
                    }
                    for (name, egt) in names.iter().zip(verification) {
                        verification::verify_egt(&reader, name, &egt)?;
                    }
                    reader.remove(&names)?;
                }
                print_pair_stats(&pair_stats[0], pair_stats.get(1), start.elapsed());
                results.extend(pair_stats);
            }
            Ok(())
        })?;
        Ok(results)
    }
}

/// An unordered material family, independent of the oriented representative.
fn material_family(pair: &TablePair) -> String {
    let material = pair.table.material();
    let twin = pair.twin.as_ref().unwrap_or(&pair.table).material();
    material.min(twin)
}

/// Prints table-specific statistics (wins, draws, losses, compression).
pub fn print_table_stats(stats: &EgtStats) {
    let unique_positions = stats.unique_positions;
    let compressed_size_mb = stats.bytes as f64 / (1024.0 * 1024.0);
    let bits_per_pos = if unique_positions > 0 {
        (stats.bytes as f64 * 8.0) / unique_positions as f64
    } else {
        0.0
    };

    println!(
        "Generated endgame {} with {} unique positions: {} wins, {} draws, {} losses. Compressed size: {:.2}MiB ({:.2} bits/pos).",
        stats.tablename,
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
    stats_a: &EgtStats,
    stats_b: Option<&EgtStats>,
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

    println!("Time used {} ({:.2} μs/pos).", format_dur, us_per_pos,);
}
