use std::path::Path;

use shakmaty::{Chess, Position, Role};

use crate::egt_file::SharedFileCache;
use crate::storage::Location;
use crate::{DtcOutcome, EgtResult, ProbeWithMoves};

/// Probes endgame tables using a shared frame cache.
///
/// [`probe`](Self::probe) returns the position's [`DtcOutcome`].
/// [`probe_with_moves`](Self::probe_with_moves) returns a [`ProbeWithMoves`] containing:
///
/// - `outcome: DtcOutcome`: the root position's outcome.
/// - `moves: Vec<(Move, DtcOutcome)>`: all legal moves in
///   `Chess::legal_moves()` order, paired with each successor's outcome **from the
///   successor's side to move**. Outcomes are not inverted or distance-adjusted.
///
/// The root and cloned, played successors are probed in one batch. Terminal
/// positions still probe the root and return an empty move list. Lookup errors
/// are returned directly, as with `probe`; a failed lookup fails the whole call.
/// The API is synchronous; async callers should use `tokio::task::spawn_blocking`.
pub struct EgtProber {
    cache: SharedFileCache,
}

impl EgtProber {
    /// Resolves a local tablebase directory or object-store URL once.
    ///
    /// Returns an error for invalid locations or backend initialization failures.
    /// This synchronous API should run in `spawn_blocking` when called from an
    /// async application, as should subsequent probing.
    pub fn new(path: impl AsRef<Path>) -> EgtResult<Self> {
        Ok(Self::from_location(Location::parse(path)?))
    }

    pub(crate) fn from_location(location: Location) -> Self {
        let cache = SharedFileCache::new(&location, None, None);
        Self { cache }
    }

    /// Sets the retained-frame cache budget in bytes and evicts frames if necessary.
    pub fn with_cache_size(&mut self, n: usize) -> EgtResult<()> {
        self.cache.set_cache_size(n)
    }

    /// Resolves an additional local directory or object-store URL for table lookup.
    /// Input is searched first; the base location is tried only for missing files.
    /// If not set, only the base location is consulted.
    ///
    /// Returns an error if the location cannot be resolved, leaving the previous
    /// setting unchanged. This synchronous API should run in `spawn_blocking`
    /// when called from an async application.
    pub fn with_input_path(&mut self, path: impl AsRef<Path>) -> EgtResult<()> {
        self.cache.set_input_path(Some(Location::parse(path)?))
    }

    /// Probes a position using the internally synchronized shared cache.
    pub fn probe(&self, position: &Chess) -> EgtResult<DtcOutcome> {
        self.cache.probe_direct_outcome(position)
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
        let outcomes = self.cache.probe_direct_outcomes(&positions)?;
        Ok(ProbeWithMoves {
            outcome: outcomes[0],
            moves: moves
                .into_iter()
                .zip(outcomes.into_iter().skip(1))
                .collect(),
        })
    }
}

pub fn get_endgame(position: &Chess) -> String {
    let stm_color = position.turn();
    let sntm_color = !stm_color;

    let mut stm = String::new();
    let mut sntm = String::new();

    // We always have exactly 1 King for each side
    stm.push('K');
    sntm.push('K');

    // Count other pieces in order: Q, R, B, N, P
    let board = position.board();
    for &role in &[
        Role::Queen,
        Role::Rook,
        Role::Bishop,
        Role::Knight,
        Role::Pawn,
    ] {
        let stm_count = (board.by_role(role) & board.by_color(stm_color))
            .into_iter()
            .count();
        for _ in 0..stm_count {
            stm.push(role.upper_char());
        }

        let sntm_count = (board.by_role(role) & board.by_color(sntm_color))
            .into_iter()
            .count();
        for _ in 0..sntm_count {
            sntm.push(role.upper_char());
        }
    }

    format!("{}_{}", stm, sntm)
}
