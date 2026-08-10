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
//!
//! **Fonts and text metrics cannot be tested here at all.** gpui's test platform installs
//! `NoopTextSystem`, whose `font_id` returns `FontId(1)` for *every* descriptor and whose
//! `advance` is a fixed formula. A test asserting "the editor font is monospaced" therefore
//! passes with `Helvetica` — I wrote that test, watched it pass under a proportional family,
//! and deleted it. Column alignment and whether `Menlo` actually resolves are verifiable only
//! on a real display, which is why the startup check in `main` logs a warning and why they
//! stay on issue #35's human list.

use std::sync::Arc;

use elle_core::{BUILTIN_COMMANDS, CommandRegistry};
use gpui::{TestAppContext, VisualTestContext, px, size};

use crate::editor::{Document, EditorView};
use crate::palette::{Palette, PaletteMode};
use crate::theme::Metrics;
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
async fn a_new_untitled_buffer_opens_dirty_free_pathless_and_renders(cx: &mut TestAppContext) {
    // ⌘N is the only way to reach `save_as`, so the properties that route it there are the
    // ones worth pinning: no path (or ⌘S writes straight through and never prompts) and not
    // dirty (or closing an untouched scratch buffer nags). Rendering it also covers the
    // empty-document-inside-the-chrome case, which the standalone editor test does not.
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));

    let editor = workspace.update(cx, |workspace, cx| {
        workspace.new_file(cx);
        workspace.active_editor_for_test().expect("⌘N must leave the new buffer active")
    });

    editor.read_with(cx, |editor, _cx| {
        assert!(editor.document.path.is_none(), "an untitled buffer must have no path");
        assert!(!editor.is_dirty(), "an untouched new buffer is not unsaved work");
        assert_eq!(editor.document.title(), "untitled");
    });

    draw(cx);

    // Typing into it dirties it, which is what makes the ⌘S that follows do anything.
    editor.update(cx, |editor, _cx| editor.document.insert("<?php\n"));
    editor.read_with(cx, |editor, _cx| assert!(editor.is_dirty()));

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
async fn the_editor_measures_its_text_origin_during_layout(cx: &mut TestAppContext) {
    // The click-to-column fix depends on `text_origin_x` being filled in by the layout
    // engine at prepaint — guessing it from the chrome constants is what produced the
    // 284 px error, where a window-relative x was treated as row-relative.
    //
    // This asserts the mechanism actually fires: after a real layout pass the field must
    // hold the gutter's width relative to the row, not `None` and not zero.
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let document = Document::new(
            Some(std::path::PathBuf::from("origin.php")),
            "<?php\n$a = 1;\n$b = 2;\n",
            true,
        )
        .expect("php grammar loads");
        EditorView::new(document, cx)
    });

    draw(cx);

    let origin = view.read_with(cx, |editor, _cx| editor.text_origin_x_for_test());
    let origin = origin.expect("prepaint must record where the text column starts");

    // The editor is rendered standalone here, so the text begins after the gutter and
    // nothing else. If this ever reads ~0, `on_children_prepainted` stopped firing and the
    // click handler is back to guessing.
    assert!(
        origin >= Metrics::GUTTER_WIDTH,
        "text should start at or after the gutter, measured {origin:?}"
    );
}

#[gpui::test]
async fn a_click_inside_the_workspace_chrome_lands_on_the_right_column(cx: &mut TestAppContext) {
    // End-to-end on the bug PR #39 fixed. It must run **inside the workspace**, not against a
    // standalone editor: the defect was that a window-relative x had only the gutter
    // subtracted, ignoring the 44 px activity bar and 240 px sidebar the row sits inside.
    //
    // A first attempt at this test rendered the editor alone, where there is no chrome — so
    // the buggy guess and the measured origin were the same number and the test passed even
    // with the bug reintroduced. Verified by mutation: with the editor standalone the old
    // arithmetic survives; inside the chrome it does not.
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));

    let editor = workspace.update(cx, |workspace, cx| {
        let document = Document::new(
            Some(std::path::PathBuf::from("click.php")),
            "<?php\n$first = 1;\n$second = 2;\n$third = 3;\n",
            true,
        )
        .expect("php grammar loads");
        workspace.open_document_for_test(document, cx);
        workspace.active_editor_for_test().expect("the opened document is active")
    });

    draw(cx);

    // The measured origin now includes the chrome, so it is far to the right of the gutter.
    // That gap *is* the bug: anything that guesses `GUTTER_WIDTH` lands ~284 px into the line.
    let origin = editor
        .read_with(cx, |editor, _cx| editor.text_origin_x_for_test())
        .expect("prepaint records the text origin");
    assert!(
        origin > Metrics::GUTTER_WIDTH + px(100.0),
        "inside the chrome the text origin must be well right of the gutter; got {origin:?}"
    );

    // Click a few pixels into the text of a row, which must resolve to an early column.
    let y = Metrics::TAB_HEIGHT + Metrics::LINE_HEIGHT * 2.5;
    cx.simulate_click(gpui::point(origin + px(4.0), y), gpui::Modifiers::default());

    let after = editor.read_with(cx, |editor, _cx| editor.document.cursor_point());
    assert!(
        after.column <= 2,
        "a click at the left edge of the text must give an early column, got {} — the old \
         arithmetic put it tens of columns in",
        after.column
    );
}

#[gpui::test]
async fn typing_reaches_the_document(cx: &mut TestAppContext) {
    // The full input path: keystroke → gpui dispatch → key_char → Document::insert. Every
    // piece of that is covered in isolation; nothing until now proved they are connected.
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let document = Document::new(None, "", false).expect("plain text needs no grammar");
        EditorView::new(document, cx)
    });

    draw(cx);
    cx.update(|window, cx| {
        window.focus(&gpui::Focusable::focus_handle(view.read(cx), cx));
    });

    cx.simulate_input("ação");

    let text = view.read_with(cx, |editor, _cx| editor.document.buffer.text());
    assert_eq!(text, "ação", "typed multibyte text must reach the buffer intact");
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
