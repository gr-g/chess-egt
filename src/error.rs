//! Error types for the chess-egt library.

use std::path::PathBuf;

/// A specialized result type for chess-egt operations.
pub type EgtResult<T> = Result<T, EgtError>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EgtError {
    /// The endgame name (e.g. "KQ_KPc") is malformed.
    #[error("invalid endgame name {name:?}: {reason}")]
    InvalidEndgameName {
        name: String,
        reason: &'static str,
    },

    /// A piece-set/table configuration is not supported (too many pieces,
    /// missing king, duplicate pawns on the same file, etc.).
    #[error("invalid piece configuration: {0}")]
    InvalidPieceConfig(&'static str),

    /// A FEN or position could not be parsed or is illegal.
    #[error("invalid position: {0}")]
    InvalidPosition(String),

    /// An index is out of range for the table/file.
    #[error("index {index} out of range [0, {range})")]
    IndexOutOfRange { index: usize, range: usize },

    /// A position does not map to a valid index in the table (e.g. pawns on
    /// unexpected files, no matching sub-table).
    #[error("position does not map to a valid index in table {table:?}")]
    PositionNotInTable { table: String },

    /// An outcome value read from disk is corrupted (invalid bit pattern).
    #[error("corrupted outcome value 0x{value:04x} at index {index}")]
    CorruptedOutcome { value: u16, index: usize },

    /// A file expected to exist was not found.
    #[error("egt file not found: {0}")]
    FileNotFound(PathBuf),

    /// A consistency check failed during verification.
    #[error("consistency check failed for {endgame:?} at index {index}: {reason}")]
    ConsistencyCheckFailed {
        endgame: String,
        index: usize,
        reason: String,
    },

    /// A dependency table could not be loaded or generated.
    #[error("dependency table {dependency:?} could not be resolved: {source}")]
    DependencyUnavailable {
        dependency: String,
        #[source]
        source: Box<EgtError>,
    },

    /// Wraps a std::io::Error.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Wraps a zeekstd compression/decompression error.
    #[error(transparent)]
    Zeekstd(#[from] zeekstd::Error),

    /// Wraps a serde_json error.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// A catch-all for an internal invariant violation that should never
    /// happen in correct usage. Carries a descriptive message so that
    /// bugs are debuggable without crashing the process.
    #[error("internal error: {0}")]
    Internal(&'static str),
}
