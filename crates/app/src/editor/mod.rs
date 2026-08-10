//! The editor: document state, and the gpui view that renders it.

mod caret;
mod find;
mod state;
mod view;

pub use find::SearchQuery;
pub use state::Document;
pub use view::{EditorEvent, EditorView};
