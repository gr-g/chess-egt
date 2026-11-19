use std::path::Path;

use shakmaty::{Chess, Position};

use crate::egt_file::{SharedFileCache, Source};
use crate::storage::Location;
use crate::{DtcOutcome, EgtError, EgtResult, ProbeWithMoves};

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

    pub(crate) fn source(&self) -> &Source {
        self.cache.source()
    }

    pub(crate) fn clear_probe_cache(&mut self) {
        self.cache.clear();
    }

    pub(crate) fn pool(&self, error: &'static str) -> EgtResult<rayon::ThreadPool> {
        rayon::ThreadPoolBuilder::new()
            .num_threads(self.threads)
            .build()
            .map_err(|_| EgtError::Internal(error))
    }

    /// Sets generation and verification workers. Zero (the default) uses Rayon's default.
    pub fn set_threads(&mut self, threads: usize) {
        self.threads = threads;
    }

    /// Sets the retained-frame cache budget in bytes and evicts frames if necessary.
    pub fn set_cache_size(&mut self, n: usize) -> EgtResult<()> {
        self.cache.set_cache_size(n);
        Ok(())
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
}
