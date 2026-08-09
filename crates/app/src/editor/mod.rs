//! The editor: document state, and the gpui view that renders it.

mod state;
mod view;

pub use state::Document;
pub use view::{FONT_FAMILY, EditorView};
