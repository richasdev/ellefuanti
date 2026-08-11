//! The editor: document state, and the gpui view that renders it.

mod find;
pub mod folds;
pub(crate) mod line;
mod project_search;
mod state;
mod view;

pub use find::SearchQuery;
/// `FileMatches` and `LineMatch` are reached through `ProjectResults`'s public fields
/// everywhere except `search_panel`'s tests, which build a result set by hand rather than
/// running a real scan against a temp directory. Naming them only there keeps the module's
/// public surface honest about what production code actually constructs.
#[cfg(test)]
pub use project_search::{FileMatches, LineMatch};
pub use project_search::{ProjectResults, search_project};
pub use state::Document;
pub use view::{EditorEvent, EditorView};
