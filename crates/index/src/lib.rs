//! The persistent project index: files, symbols, and the dependency graph between them.
//!
//! Backed by SQLite in a single file (ADR-0008). Every function here is synchronous and
//! blocking *on purpose* — this crate does not know which executor runs it, and the caller
//! wraps it in `cx.background_spawn` (ADR-0007). That is also what makes it testable
//! without a runtime.
//!
//! **The index is a cache, never a source of truth.** It can be deleted at any time and
//! rebuilt from the filesystem, and that is the recovery path for every way it can break:
//! a schema version it does not recognise, a corrupt file, a half-written transaction.
//! Nothing here repairs a database in place, because a subtly-wrong index produces
//! subtly-wrong autocomplete, which is worse than having none (§24).
//!
//! What is *not* here yet: the analysers that populate it (#22, #23, #24), the file
//! watcher, and the incremental reanalysis pass that walks the graph. This is the
//! foundation those build on.

mod db;
mod records;
mod schema;

pub use db::{Index, Opened};
pub use records::{
    FileId, Symbol, clear_dependencies_from, clear_symbols, dependencies_of, dependents_of,
    file_id, file_stamp, files_declaring, insert_dependency, insert_file, insert_symbol,
    remove_file, symbols_in_file,
};
pub use schema::SCHEMA_VERSION;
