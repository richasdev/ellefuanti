//! Incremental parsing and syntax highlighting.
//!
//! Depends on `elle-text`, never on the UI: highlight spans come out as plain data
//! that any renderer can consume (ADR-0005).

mod highlight;
mod language;
mod tree;

pub use highlight::{HighlightSpan, HighlightStyle};
pub use language::{ALL_LANGUAGES, Language, language_for_path};
pub use tree::SyntaxTree;
