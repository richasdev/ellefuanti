//! Headless render tests: do the real views survive a full layout and paint pass?
//!
//! Everything else in this crate tests the *inputs* to rendering — which bytes get which
//! colour, which offset a click maps to. That is the largest slice a machine can check
//! cheaply, but it cannot catch a view that panics during layout, a `uniform_list` asked for
//! a row that does not exist, or an element tree that never paints at all.
//!
//! gpui's `test-support` feature opens a real window and runs a real layout/paint pass with
//! no display attached, which closes most of that gap. It was previously assumed unusable on
//! macOS because the feature enables `wayland` and `x11`; it does, and it works regardless.
//!
//! **What this still cannot tell you** is whether the result *looks* right — geometry,
//! alignment, whether the cursor is where the user clicked. Those need a human at a screen
//! and remain issue #35. A passing test here means "it renders without crashing", not "it
//! renders correctly".
//!
//! That boundary was measured, not assumed. Mutating `EditorView`'s row count to 500 rows
//! past the end of the document — a textbook virtualised-list off-by-one — does **not** fail
//! any test below, because `Buffer::line()` deliberately returns `""` past EOF so the
//! renderer can ask for rows beyond the end while scrolling. The tests catch panics and
//! layout failures; they are blind to a wrong-but-well-formed element tree. Anything
//! asserting *which* rows appear has to assert on the row range directly, which is what the
//! unit tests beside `line_runs` do.

use std::sync::Arc;

use elle_core::{BUILTIN_COMMANDS, CommandRegistry};
use gpui::{TestAppContext, VisualTestContext, px, size};

use crate::editor::{Document, EditorView};
use crate::palette::{Palette, PaletteMode};
use crate::workspace_view::WorkspaceView;

/// A window big enough that the sidebar, tab bar and status bar all have room; a cramped
/// window can hide a layout panic behind a zero-sized element that never lays out children.
fn draw(cx: &mut VisualTestContext) {
    cx.draw(gpui::point(px(0.), px(0.)), size(px(1180.), px(760.)), |_window, _cx| gpui::div());
}

fn registry() -> Arc<CommandRegistry> {
    let mut registry = CommandRegistry::new();
    registry.register_all(BUILTIN_COMMANDS.iter().copied());
    Arc::new(registry)
}

#[gpui::test]
async fn the_workspace_renders_with_no_folder_open(cx: &mut TestAppContext) {
    // The startup state: no project, no tabs. It still has to paint the chrome — activity
    // bar, empty sidebar, status bar — and the empty-state hint.
    let registry = registry();
    let (_view, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    draw(cx);
}

#[gpui::test]
async fn the_workspace_renders_with_a_file_open(cx: &mut TestAppContext) {
    let registry = registry();
    let (view, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));

    // Open a real PHP document through the same path the UI uses.
    view.update(cx, |workspace, cx| {
        let document = Document::new(
            Some(std::path::PathBuf::from("User.php")),
            "<?php\n\nclass User extends Model\n{\n    protected $table = 'users';\n}\n",
            true,
        )
        .expect("php grammar loads");
        workspace.open_document_for_test(document, cx);
    });

    draw(cx);
}

#[gpui::test]
async fn the_editor_renders_a_large_file(cx: &mut TestAppContext) {
    // A virtualised list asked to render 20k rows is where an off-by-one in the visible
    // range surfaces as a panic rather than a wrong pixel.
    let mut text = String::from("<?php\n");
    for i in 0..20_000 {
        text.push_str(&format!("$var{i} = 'value';\n"));
    }

    let (_view, cx) = cx.add_window_view(|_window, cx| {
        let document = Document::new(Some(std::path::PathBuf::from("big.php")), &text, true)
            .expect("php grammar loads");
        EditorView::new(document, cx)
    });

    draw(cx);
}

#[gpui::test]
async fn the_editor_renders_multibyte_text_with_a_cursor_on_it(cx: &mut TestAppContext) {
    // Byte-vs-char confusion in the highlight or cursor spans shows up here as a panic from
    // `StyledText`, which debug-asserts on non-boundary indices.
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let document = Document::new(
            Some(std::path::PathBuf::from("acao.php")),
            "<?php\n$mensagem = 'ação não configurada';\n// 日本語のコメント\n",
            true,
        )
        .expect("php grammar loads");
        EditorView::new(document, cx)
    });

    // Put the cursor inside the multibyte string rather than at offset 0.
    view.update(cx, |editor, _cx| {
        let offset = editor.document.buffer.text().find("ção").expect("fixture contains ção");
        editor.document.move_to(offset, false);
    });

    draw(cx);
}

#[gpui::test]
async fn the_editor_renders_an_empty_document(cx: &mut TestAppContext) {
    // Zero rows, cursor at 0, no highlight spans: the degenerate case that a renderer
    // written against a non-empty fixture tends to divide by.
    let (_view, cx) = cx.add_window_view(|_window, cx| {
        let document = Document::new(None, "", false).expect("plain text needs no grammar");
        EditorView::new(document, cx)
    });

    draw(cx);
}

#[gpui::test]
async fn the_palette_renders_in_both_modes(cx: &mut TestAppContext) {
    let items: Vec<(String, String)> =
        BUILTIN_COMMANDS.iter().map(|c| (c.title.to_string(), c.id.0.to_string())).collect();

    let (_commands, cx) =
        cx.add_window_view(|_window, cx| Palette::new(PaletteMode::Commands, items.clone(), cx));
    draw(cx);

    // Files mode opens empty while the background walk runs — the state a user actually
    // sees first, and the one where an empty-list renderer would panic.
    let (_files, cx) =
        cx.add_window_view(|_window, cx| Palette::new(PaletteMode::Files, Vec::new(), cx));
    draw(cx);
}
