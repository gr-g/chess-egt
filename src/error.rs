//! Error types for the chess-egt library.

/// A specialized result type for chess-egt operations.
pub type EgtResult<T> = Result<T, EgtError>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EgtError {
    /// The endgame name (e.g. "KQ_KPc") is malformed.
    #[error("invalid endgame name {name:?}: {reason}")]
    InvalidEndgameName { name: String, reason: &'static str },

    /// A position is not supported (too many pieces, position has
    /// castling rights).
    #[error("invalid piece configuration: {0}")]
    UnsupportedPosition(&'static str),

    /// The input could not be parsed as FEN.
    #[error("invalid position: invalid FEN: {0:?}")]
    InvalidFen(#[source] shakmaty::fen::ParseFenError),

    /// The chess position is invalid.
    #[error("invalid position: {0}")]
    InvalidPosition(String),

    /// An index is out of range for the table/file.
    #[error("index {index} out of range [0, {range})")]
    IndexOutOfRange { index: usize, range: usize },

    /// An outcome value read from disk is corrupted (invalid bit pattern).
    #[error("corrupted outcome value 0x{value:04x} at index {index}")]
    CorruptedOutcome { value: u16, index: usize },

    /// A file expected to exist was not found.
    #[error("egt file not found: {0}")]
    FileNotFound(String),

    /// A storage location could not be parsed or is unsupported.
    #[error("invalid storage location: {0}")]
    InvalidStorageLocation(String),

    /// The storage backend failed to access or update an artifact.
    #[error("storage operation failed for {location}: {source}")]
    Storage {
        location: String,
        #[source]
        source: object_store::Error,
    },

    /// A stored binary or JSON artifact is malformed. The source retains parse
    /// or validation details; location may also identify a frame or index range.
    #[error("invalid data at {location}: {source}")]
    InvalidData {
        location: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Memory for working values or decompressed table data could not be allocated.
    #[error("failed to allocate {bytes} bytes for {context}: {reason}")]
    WorkingMemoryAllocation {
        context: String,
        bytes: u128,
        reason: String,
    },

    /// A consistency check failed during verification.
    #[error("consistency check failed for {endgame:?} at index {index}: {reason}")]
    ConsistencyCheckFailed {
        endgame: String,
        index: usize,
        reason: String,
    },

    /// A required dependency table is missing.
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

impl EgtError {
    pub(crate) fn invalid_data(
        location: impl Into<String>,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        Self::InvalidData {
            location: location.into(),
            source: source.into(),
        }
    }

    pub(crate) fn into_data_source(self) -> Box<dyn std::error::Error + Send + Sync> {
        match self {
            Self::InvalidData { source, .. } => source,
            other => Box::new(other),
        }
    }
}
