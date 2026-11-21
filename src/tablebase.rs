use std::{path::Path, time::Duration};

use shakmaty::{CastlingMode, Chess, Move, Position, PositionError, fen::Fen};

use crate::egt_file::{SharedFileCache, Source};
use crate::storage::Location;
use crate::{AggregateStats, DtcOutcome, EgtError, EgtResult, EgtStats};
use crate::{generator, stats, verification};

/// Parses a standard-chess FEN for probing, accepting impossible checks as indexing does.
pub fn parse_position(fen: &str) -> EgtResult<Chess> {
    fen.parse::<Fen>()
        .map_err(EgtError::InvalidFen)?
        .into_position(CastlingMode::Standard)
        .or_else(PositionError::ignore_impossible_check)
        .map_err(|e| EgtError::InvalidPosition(format!("invalid position: {e:?}")))
}

#[cfg(test)]
#[test]
fn parse_position_accepts_impossible_check() {
    let position = parse_position("r6b/7k/8/8/8/8/2n5/K7 w - - 0 1").unwrap();
    assert_eq!(position.checkers().count(), 3);
}

/// A position's outcome and its legal moves with successor-side-to-move outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeWithMoves {
    pub outcome: DtcOutcome,
    pub moves: Vec<(Move, DtcOutcome)>,
}

/// Generates, verifies, and probes endgame tables and queries their statistics.
pub struct Tablebase {
    cache: SharedFileCache,
    threads: usize,
}

impl Tablebase {
    /// Resolves a local tablebase directory or object-store URL once.
    ///
    /// Returns an error for invalid locations or backend initialization failures.
    pub fn new(path: impl AsRef<Path>) -> EgtResult<Self> {
        Ok(Self::from_location(Location::parse(path)?))
    }

    pub(crate) fn from_location(location: Location) -> Self {
        Self {
            cache: SharedFileCache::new(Source::new(location)),
            threads: 0,
        }
    }

    fn source(&self) -> &Source {
        self.cache.source()
    }

    /// Sets generation and verification workers. Zero (the default) uses Rayon's default.
    pub fn set_threads(&mut self, threads: usize) {
        self.threads = threads;
    }

    /// Sets the retained-frame cache budget in bytes and evicts frames if necessary.
    pub fn set_cache_size(&mut self, n: usize) {
        self.cache.set_cache_size(n);
    }

    /// Sets the idle age at which retained frames become eligible for eviction
    /// by [`Self::evict_idle_frames`], which the caller should run periodically.
    pub fn set_cache_idle_timeout(&mut self, timeout: Duration) {
        self.cache.set_cache_idle_timeout(timeout);
    }

    /// Drops idle frames if an idle timeout is configured. In-flight probes keep their frames.
    pub fn evict_idle_frames(&self) {
        self.cache.evict_idle_frames();
    }

    /// Sets an input overlay for probing, verification, and generation dependencies.
    /// Input is searched first; the base location is tried only for missing files.
    /// Generation writes and statistics always use the base location.
    ///
    /// Clears cached probe data on success. An invalid location leaves the previous
    /// setting unchanged.
    pub fn set_input_path(&mut self, path: impl AsRef<Path>) -> EgtResult<()> {
        self.cache.set_input_path(Some(Location::parse(path)?));
        Ok(())
    }

    /// Probes a position using the internally synchronized shared cache.
    pub fn probe(&self, position: &Chess) -> EgtResult<DtcOutcome> {
        self.cache.probe(position)
    }

    /// Probes the root and all legal successors in one batch.
    ///
    /// Moves retain `position.legal_moves()` order. Each move's outcome is from
    /// the successor's side to move, without inversion or distance adjustment.
    /// Terminal positions have an empty move list. Lookup errors are returned
    /// directly, as with `probe`.
    pub fn probe_with_moves(&self, position: &Chess) -> EgtResult<ProbeWithMoves> {
        let moves = position.legal_moves();
        let mut positions = Vec::with_capacity(1 + moves.len());
        positions.push(position.clone());
        for m in &moves {
            let mut successor = position.clone();
            successor.play_unchecked(*m);
            positions.push(successor);
        }
        let outcomes = self.cache.probe_outcomes(&positions)?;
        Ok(ProbeWithMoves {
            outcome: outcomes[0],
            moves: moves
                .into_iter()
                .zip(outcomes.into_iter().skip(1))
                .collect(),
        })
    }

    /// Generates twin-pair jobs in the base location, ordered by pieces then pawns.
    /// Capture/promotion dependencies must already exist or be explicitly selected.
    /// Working arrays and each pair's eagerly loaded dependencies must fit in memory.
    /// Clears the probe cache before any tables can be replaced.
    pub fn generate(&mut self, selectors: &[&str]) -> EgtResult<Vec<EgtStats>> {
        let pool = thread_pool(self.threads)?;
        self.cache.clear();
        generator::generate(self.source(), &pool, selectors)
    }

    /// Verifies the sorted, deduplicated selection against legal successors.
    /// Each selected table, its quiet twin, and its direct conversion dependencies
    /// must fit in memory together. Twins are loaded but not implicitly verified.
    pub fn verify(&self, selectors: &[&str]) -> EgtResult<()> {
        verification::verify(self.source(), &thread_pool(self.threads)?, selectors)
    }

    /// Reads metadata without modifying storage. Missing sidecars contribute
    /// theoretical index ranges; computed totals are null for incomplete selections.
    /// Hierarchy selections use cached summaries when available. Only the base
    /// location is consulted, even when an input overlay is configured.
    pub fn stats(&self, selectors: &[&str]) -> EgtResult<AggregateStats> {
        stats::query(self.source().base(), selectors)
    }

    /// Creates missing placeholders and rebuilds hierarchy summaries in the base location.
    /// Does not scan table contents or replace complete per-table metadata.
    /// Do not run concurrently with generation or another cache rebuild.
    pub fn cache_stats(&self, selectors: &[&str]) -> EgtResult<AggregateStats> {
        stats::cache(self.source().base(), selectors)
    }
}

/// A Rayon pool with `threads` workers, or Rayon's default for zero.
pub(crate) fn thread_pool(threads: usize) -> EgtResult<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|_| EgtError::Internal("failed to build thread pool"))
}
