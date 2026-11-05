//! `echo-module-loader` — layered composition of module configuration.
//!
//! A *profile* file (`modules.toml`) names the bundles and patches that make
//! up a module tree and may carry inline `[[module]]` rows itself. The loader
//! composes them with dsh-style patch semantics — **locate by id, replace the
//! whole row**:
//!
//! 1. start from an empty list and apply every bundle in order;
//! 2. then apply every patch in order;
//! 3. finally apply the profile's own inline rows.
//!
//! A new id is appended; an existing id is replaced wholesale (no field-level
//! merge), so a patch can never leave stale `config` keys behind. Every applied
//! file is a *layer* (0-based) and each row keeps its provenance
//! ([`ModuleEntry::layer`] + [`ModuleEntry::layer_source`]). Bundle/patch names
//! resolve relative to the profile file's directory and get `.toml` appended
//! when the name has no extension.
//!
//! After composition the tree is validated (`stdio` rows need a `command`,
//! ids must be non-empty and whitespace-free, `requires` must resolve) and
//! stably topologically sorted by `requires`, keeping insertion order for rows
//! without dependency relations. Cycles are reported as readable chains
//! (`a -> b -> c -> a`).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod compose;
mod error;
mod model;
mod output;

pub use compose::load;
pub use error::LoadError;
pub use model::{EntryKind, LoadedTree, ModuleEntry, ModuleLimits};
pub use output::dump;
