//! Loader error type.

use std::path::PathBuf;

use thiserror::Error;

/// Everything that can go wrong while reading, composing, or validating a
/// module tree.
#[derive(Debug, Error)]
pub enum LoadError {
    /// Reading a profile/bundle/patch file failed.
    #[error("io error at `{path}`: {source}")]
    Io {
        /// File that could not be read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A file is not valid TOML or does not match the row schema.
    #[error("parse error at `{path}`: {message}")]
    Parse {
        /// File that failed to parse.
        path: PathBuf,
        /// Rendered `toml` error (includes line/column info).
        message: String,
    },
    /// The composed tree violates a row-level rule (bad id, `stdio` without
    /// `command`).
    #[error("validation error: {message}")]
    Validation {
        /// Human-readable explanation, naming the offending entry.
        message: String,
    },
    /// A bundle or patch file referenced by the profile does not exist.
    #[error("missing bundle or patch file: `{name}`")]
    MissingBundle {
        /// Reference exactly as written in the profile (no `.toml` appended).
        name: String,
    },
    /// `requires` references ids that no composed row defines.
    #[error("module `{id}` requires unknown module ids: {missing:?}")]
    UnknownRequires {
        /// Entry whose `requires` failed to resolve.
        id: String,
        /// All unresolved ids of that entry, in `requires` order, deduplicated.
        missing: Vec<String>,
    },
    /// The `requires` graph contains a cycle.
    #[error("dependency cycle detected: {chain}")]
    Cycle {
        /// Closed chain, e.g. `a -> b -> c -> a`.
        chain: String,
    },
}
