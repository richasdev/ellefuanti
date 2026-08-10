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
//! `advance` is a fixed formula — `600.0 * ch.len_utf16()`, so every BMP character has the
//! same advance and the noop text system measures as a *perfect monospace*. A test asserting
//! "the editor font is monospaced" therefore passes with `Helvetica` — I wrote that test,
//! watched it pass under a proportional family, and deleted it.
//!
//! #49 made the family configurable and did not change that. The monospace check it added
//! queries real glyph advances through `cx.text_system()` and is correspondingly untestable
//! from here; `crate::fonts` says so at the point where someone would be tempted. Column
//! alignment and whether the chosen family actually resolves stay on issue #35's human list.

use std::sync::Arc;

use elle_core::{BUILTIN_COMMANDS, CommandRegistry};
use gpui::{Focusable, TestAppContext, VisualTestContext, px, size};

use crate::completion::{CompletionItem, CompletionSource};
use crate::editor::{Document, EditorView};
use crate::find_bar::{FindEvent, Status};
use crate::fonts::Fonts;
use crate::palette::{Palette, PaletteMode};
use crate::search_panel::SearchState;
use crate::terminal_view::TerminalView;
use crate::theme::{Metrics, ThemeVariant, Themed, set_theme};
use crate::workspace_view::WorkspaceView;

/// Installs a theme, because `cx.theme()` panics without one.
///
/// Every test here calls this before building a view, for the same reason `main` does it
/// before opening the window: the theme is the one piece of state a view cannot construct
/// for itself, which is the entire point of #48's step 1.
fn install_theme(cx: &mut TestAppContext) {
    cx.update(|cx| set_theme(ThemeVariant::default(), cx));
}

/// Lays out and paints the window — once per compiled-in theme.
///
/// Painting under every variant is deliberate and is the closest thing to the acceptance
/// test #48 describes. A view that still built its own `Theme` would render identically in
/// both passes and this would not catch it, so the grep test in `tests/theming.rs` is what
/// enforces *that*. What this catches is the other half: a theme missing a colour, or a
/// variant whose values break layout, showing up as a panic in every render test rather
/// than in whichever one someone thought to write.
///
/// The window is 1180x760 so the sidebar, tab bar and status bar all have room; a cramped
/// window can hide a layout panic behind a zero-sized element that never lays out children.
fn draw(cx: &mut VisualTestContext) {
    for variant in [ThemeVariant::Dark, ThemeVariant::Light] {
        cx.update(|_window, cx| set_theme(variant, cx));
        cx.draw(gpui::point(px(0.), px(0.)), size(px(1180.), px(760.)), |_window, _cx| gpui::div());
    }
}

fn registry() -> Arc<CommandRegistry> {
    let mut registry = CommandRegistry::new();
    registry.register_all(BUILTIN_COMMANDS.iter().copied());
    Arc::new(registry)
}

#[gpui::test]
async fn the_workspace_renders_with_no_folder_open(cx: &mut TestAppContext) {
    install_theme(cx);
    // The startup state: no project, no tabs. It still has to paint the chrome — activity
    // bar, empty sidebar, status bar — and the empty-state hint.
    let registry = registry();
    let (_view, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    draw(cx);
}

#[gpui::test]
async fn the_workspace_renders_with_a_file_open(cx: &mut TestAppContext) {
    install_theme(cx);
    let registry = registry();
    let (view, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));

    // Open a real PHP document through the same path the UI uses.
    view.update_in(cx, |workspace, window, cx| {
        let document = Document::new(
            Some(std::path::PathBuf::from("User.php")),
            "<?php\n\nclass User extends Model\n{\n    protected $table = 'users';\n}\n",
            true,
        )
        .expect("php grammar loads");
        workspace.open_document_for_test(document, window, cx);
    });

    draw(cx);
}

#[gpui::test]
async fn a_new_untitled_buffer_opens_dirty_free_pathless_and_renders(cx: &mut TestAppContext) {
    install_theme(cx);
    // ⌘N is the only way to reach `save_as`, so the properties that route it there are the
    // ones worth pinning: no path (or ⌘S writes straight through and never prompts) and not
    // dirty (or closing an untouched scratch buffer nags). Rendering it also covers the
    // empty-document-inside-the-chrome case, which the standalone editor test does not.
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));

    let editor = workspace.update_in(cx, |workspace, window, cx| {
        workspace.new_file(window, cx);
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
    install_theme(cx);
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
    install_theme(cx);
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
    install_theme(cx);
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
    install_theme(cx);
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
    let gutter = cx.update(|_window, cx| Fonts::get(cx).gutter_width());
    assert!(origin >= gutter, "text should start at or after the gutter, measured {origin:?}");
}

#[gpui::test]
async fn an_unfocused_editor_runs_no_blink_timer(cx: &mut TestAppContext) {
    install_theme(cx);
    // **This is the perf property, and it is the one worth a test.** A blink is a repaint
    // on a timer; #79 spent three wrong conclusions on exactly that class of cost and #93's
    // gate now bounds idle CPU at 2%. gpui has no partial repaint, so the only thing that
    // makes the blink affordable is that it *stops* — and "stops" here means the `Task` is
    // dropped, not that a flag is checked inside a still-running loop.
    //
    // The editor is created but never focused, which is the state a background tab is in.
    //
    // **Read this before trusting the test.** gpui's test platform hardcodes
    // `is_active() -> false` (`platform/test/window.rs`), so no headless window is ever
    // active. This therefore proves the teardown path runs and leaves no task behind — a
    // real regression if someone makes `render` start a timer unconditionally — but it
    // cannot prove the *inverse*, that a focused editor does start one, because focus can
    // never be satisfied here. That half was verified by measuring idle CPU on the real
    // binary with `scripts/perf-gate.sh`, which is the only place it means anything.
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let document =
            Document::new(Some(std::path::PathBuf::from("idle.php")), "<?php\n$a = 1;\n", true)
                .expect("php grammar loads");
        EditorView::new(document, cx)
    });

    draw(cx);

    assert!(
        !view.read_with(cx, |editor, _cx| editor.is_blinking_for_test()),
        "an editor without focus must not hold a blink timer"
    );
}

#[gpui::test]
async fn typing_holds_the_caret_solid(cx: &mut TestAppContext) {
    install_theme(cx);
    // A caret that blinks mid-keystroke is worse than one that does not blink at all: the
    // motion competes with the character appearing under it, and a keystroke landing during
    // the dark half reads as a dropped character. Every edit therefore forces the caret
    // visible and restarts the pause before blinking resumes.
    //
    // **What this test cannot do, and why it is shaped like this.** gpui's test platform
    // hardcodes `is_active() -> false` (`platform/test/window.rs`), so a headless window is
    // never active and `render` — correctly — tears the blink down every frame. There is
    // therefore no way to observe the blink *through a draw* from here. An earlier version
    // of this test focused the view and drew first; it failed, and it failed for the right
    // reason. So this drives the edit path directly and asserts its post-condition, which
    // is the part that carries the behaviour anyway.
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let document = Document::new(Some(std::path::PathBuf::from("type.php")), "<?php\n", true)
            .expect("php grammar loads");
        EditorView::new(document, cx)
    });

    view.update(cx, |editor, cx| {
        // Force the caret into its hidden half, the way a timer tick would.
        editor.set_caret_hidden_for_test();
        // An edit through the same path a keystroke takes.
        editor.document.insert("$");
        editor.after_edit_for_test(cx);

        assert!(
            editor.caret_visible_for_test(),
            "an edit must force the caret visible rather than leave it mid-cycle"
        );
        assert!(
            editor.is_blinking_for_test(),
            "and must schedule the resume, not stop blinking altogether"
        );
    });
}

#[gpui::test]
async fn a_click_inside_the_workspace_chrome_lands_on_the_right_column(cx: &mut TestAppContext) {
    install_theme(cx);
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

    let editor = workspace.update_in(cx, |workspace, window, cx| {
        let document = Document::new(
            Some(std::path::PathBuf::from("click.php")),
            "<?php\n$first = 1;\n$second = 2;\n$third = 3;\n",
            true,
        )
        .expect("php grammar loads");
        workspace.open_document_for_test(document, window, cx);
        workspace.active_editor_for_test().expect("the opened document is active")
    });

    draw(cx);

    // The measured origin now includes the chrome, so it is far to the right of the gutter.
    // That gap *is* the bug: anything that guesses `GUTTER_WIDTH` lands ~284 px into the line.
    let origin = editor
        .read_with(cx, |editor, _cx| editor.text_origin_x_for_test())
        .expect("prepaint records the text origin");
    let fonts = cx.update(|_window, cx| Fonts::get(cx));
    assert!(
        origin > fonts.gutter_width() + px(100.0),
        "inside the chrome the text origin must be well right of the gutter; got {origin:?}"
    );

    // Click a few pixels into the text of a row, which must resolve to an early column.
    let y = Metrics::TAB_HEIGHT + fonts.line_height() * 2.5;
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
async fn a_double_and_triple_click_reach_word_and_line_selection(cx: &mut TestAppContext) {
    install_theme(cx);
    // The *wiring*, not the rule. `Document::select_word_at` and `select_line_at` are unit
    // tested in `editor/state.rs` against real text, where the answers are exact. What no
    // unit test can show is that gpui's `click_count` reaches the match in
    // `on_row_mouse_down` at all — a handler that ignored it would leave every one of those
    // unit tests green while double-click did nothing in the app.
    //
    // **What this cannot check** is *which* word: the click x has to be resolved through
    // `closest_index_for_x` against the headless text system, which is a fake perfect
    // monospace (`600.0 * len_utf16`, see the module docs above). So the assertions are on
    // the *shape* of the selection — empty, a word-sized run on one row, a whole row —
    // which the fake metrics cannot fake away. Whether the word under the pointer is the
    // one selected stays on #35's human list.
    //
    // gpui's `simulate_click` hardcodes `click_count: 1`
    // (`gpui-0.2.2/src/app/test_context.rs:776`), so the events are built by hand.
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let document = Document::new(
            Some(std::path::PathBuf::from("click.php")),
            "<?php\n$first = 1;\n$second = 2;\n",
            true,
        )
        .expect("php grammar loads");
        EditorView::new(document, cx)
    });

    draw(cx);

    let fonts = cx.update(|_window, cx| Fonts::get(cx));
    let origin = view
        .read_with(cx, |editor, _cx| editor.text_origin_x_for_test())
        .expect("prepaint records the text origin");
    // Row 1 (`$first = 1;`), a few pixels into its text.
    let position = gpui::point(origin + px(4.0), fonts.line_height() * 1.5);

    let click = |cx: &mut VisualTestContext, count: usize| {
        cx.simulate_event(gpui::MouseDownEvent {
            position,
            button: gpui::MouseButton::Left,
            modifiers: gpui::Modifiers::default(),
            click_count: count,
            first_mouse: false,
        });
    };

    click(cx, 1);
    view.read_with(cx, |editor, _cx| {
        assert!(
            editor.document.selection.is_empty(),
            "a single click places the cursor and selects nothing"
        );
    });

    click(cx, 2);
    let (word, word_rows) = view.read_with(cx, |editor, _cx| {
        let text = editor.document.selected_text().unwrap_or_default();
        let range = editor.document.selection.range();
        let rows = editor.document.buffer.offset_to_point(range.end).row
            - editor.document.buffer.offset_to_point(range.start).row;
        (text, rows)
    });
    assert!(!word.is_empty(), "a double click must select something, got nothing");
    assert!(!word.contains('\n'), "a double click must not cross a line break, got {word:?}");
    assert_eq!(word_rows, 0, "and must stay on one row");

    click(cx, 3);
    let line =
        view.read_with(cx, |editor, _cx| editor.document.selected_text().unwrap_or_default());
    assert_eq!(
        line, "$first = 1;\n",
        "a triple click takes the whole row including its ending, whatever x it landed on"
    );

    click(cx, 4);
    let all = view.read_with(cx, |editor, _cx| editor.document.selected_text().unwrap_or_default());
    let text = view.read_with(cx, |editor, _cx| editor.document.buffer.text());
    assert_eq!(all, text, "a fourth click selects the document, the way Zed's `_ =>` arm does");
}

#[gpui::test]
async fn typing_reaches_the_document(cx: &mut TestAppContext) {
    install_theme(cx);
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
    install_theme(cx);
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

/// The test panel paints in every state it can be in, under every theme (#25).
///
/// The states are the point, not the coverage: an empty panel, a run with passes, failures
/// and skips, and — the one most likely to be forgotten — a run whose output could not be
/// parsed at all, which renders the raw text instead of results. A renderer that indexed
/// into an empty list, or that assumed every failure carries a location, panics here.
#[gpui::test]
async fn the_test_panel_renders_in_every_state(cx: &mut TestAppContext) {
    use elle_test_runner::{Event, Location};

    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));

    // Opened, with no run yet — the first thing a user sees.
    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| workspace.toggle_test_panel_for_test(window, cx))
    });
    draw(cx);

    // A finished run with one of everything, including a failure with no readable location
    // and one with a multi-line message.
    workspace.update(cx, |workspace, cx| {
        workspace.seed_test_results_for_test(
            vec![
                Event::Started { name: "it passes".to_string() },
                Event::Finished { name: "it passes".to_string(), duration_ms: Some(2) },
                Event::Started { name: "it fails".to_string() },
                Event::Failed {
                    name: "it fails".to_string(),
                    message: "Failed asserting that 2 is identical to 3.\nsecond line".to_string(),
                    location: Some(Location {
                        path: "tests/Unit/ExampleTest.php".to_string(),
                        line: 8,
                    }),
                },
                Event::Started { name: "it fails nowhere".to_string() },
                Event::Failed {
                    name: "it fails nowhere".to_string(),
                    message: "no location for this one".to_string(),
                    location: None,
                },
                Event::Started { name: "it is skipped".to_string() },
                Event::Ignored {
                    name: "it is skipped".to_string(),
                    message: "not today".to_string(),
                },
            ],
            crate::test_view::RunState::Finished {
                command: "./vendor/bin/pest --teamcity --colors=never".to_string(),
                code: Some(1),
            },
            cx,
        );
    });
    draw(cx);

    // The degradation path: a runner that printed something we could not read at all.
    workspace.update(cx, |workspace, cx| {
        workspace.seed_test_results_for_test(
            vec![Event::Unparsed { line: "PHP Fatal error: out of memory".to_string() }],
            crate::test_view::RunState::Failed {
                message: "No Pest or PHPUnit found in vendor/bin".to_string(),
            },
            cx,
        );
    });
    draw(cx);
}

#[gpui::test]
async fn switching_the_theme_at_runtime_repaints_every_surface(cx: &mut TestAppContext) {
    // #48's second "done when": switching updates every surface, including the terminal.
    //
    // The point of the plumbing is that this needs no per-view bookkeeping — so the test
    // opens a workspace with an editor, a terminal panel and a palette all live, switches
    // once, and paints. Under the old code the editor, terminal and palette would each
    // have kept building `Theme::dark()`; here there is one theme and it moved.
    //
    // What this proves is that the switch reaches every view without panicking, and that
    // the colours they read afterwards are the new theme's. It does **not** prove the
    // result looks right — that is #35, and it needs a person.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));

    workspace.update_in(cx, |workspace, window, cx| {
        let document = Document::new(
            Some(std::path::PathBuf::from("User.php")),
            "<?php\n\nclass User extends Model\n{\n    protected $table = 'users';\n}\n",
            true,
        )
        .expect("php grammar loads");
        workspace.open_document_for_test(document, window, cx);
    });

    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_terminal_for_test(window, cx);
            workspace.toggle_command_palette_for_test(window, cx);
        })
    });
    cx.draw(gpui::point(px(0.), px(0.)), size(px(1180.), px(760.)), |_window, _cx| gpui::div());

    let before = cx.update(|_window, cx| cx.theme().background);

    cx.update(|window, cx| {
        workspace.update(cx, |workspace, cx| workspace.toggle_theme_for_test(window, cx))
    });
    cx.draw(gpui::point(px(0.), px(0.)), size(px(1180.), px(760.)), |_window, _cx| gpui::div());

    let after = cx.update(|_window, cx| cx.theme().background);
    assert_ne!(before, after, "toggling the theme must actually change the active theme");
    // Against `next()` rather than a named variant: the cycle grew from two themes to
    // five in #53, and what this test is actually about is that a toggle advances the
    // cycle and repaints — not which theme happens to be second.
    assert_eq!(
        cx.update(|_window, cx| cx.theme_variant()),
        ThemeVariant::default().next(),
        "one toggle must advance the cycle by exactly one"
    );
}

// --- diagnostics (#59 step 3) -----------------------------------------------------------

/// The case almost every user is in, and the one §24 is really about.
///
/// No language server is installed — nothing was started, nothing is running — and the
/// workspace must render exactly as it did before diagnostics existed. Not "renders an
/// empty problems area": renders with the status bar saying nothing about the LSP at all.
///
/// A render test rather than a unit test because the failure it guards against is visual:
/// a stray icon, a "0 problems", a `Starting…` that never clears. The unit test in
/// `workspace_view` pins the label; this pins that a real layout pass agrees.
#[gpui::test]
async fn the_workspace_renders_normally_with_no_language_server(cx: &mut TestAppContext) {
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));

    workspace.update_in(cx, |workspace, window, cx| {
        let document = Document::new(
            Some(std::path::PathBuf::from("User.php")),
            "<?php\n\nclass User\n{\n    public $name;\n}\n",
            true,
        )
        .expect("php grammar loads");
        workspace.open_document_for_test(document, window, cx);
    });

    workspace.read_with(cx, |workspace, _cx| {
        assert_eq!(
            workspace.lsp_label_for_test(),
            "",
            "an editor with no language server must say nothing about it"
        );
    });

    draw(cx);
}

/// Diagnostics reach a real layout pass, in every theme.
///
/// What this proves that a `line_runs` unit test cannot: the underline styles survive
/// `StyledText`, `uniform_list` and a full prepaint/paint without gpui rejecting a run.
/// Overlapping and multi-line ranges are in the fixture because those are the ones that
/// produce the malformed run lists gpui debug-asserts on.
#[gpui::test]
async fn an_editor_with_diagnostics_renders(cx: &mut TestAppContext) {
    use elle_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

    fn at(line: u32, start: u32, end: u32, severity: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position { line, character: start },
                end: Position { line, character: end },
            },
            severity: Some(severity),
            message: "something is wrong".into(),
            ..Default::default()
        }
    }

    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));

    let path = std::path::PathBuf::from("User.php");
    workspace.update_in(cx, |workspace, window, cx| {
        let document = Document::new(
            Some(path.clone()),
            // Line 4 carries multibyte characters, so a UTF-16 range over it is the case
            // that would land mid-codepoint if the conversion were wrong.
            "<?php\n\nclass User\n{\n    public $ação = 'não';\n}\n",
            true,
        )
        .expect("php grammar loads");
        workspace.open_document_for_test(document, window, cx);
    });

    workspace.update(cx, |workspace, cx| {
        workspace.publish_diagnostics_for_test(
            &path,
            &[
                at(2, 6, 10, DiagnosticSeverity::ERROR),
                // Overlapping the one above, which is what forces the run splitting.
                at(2, 0, 8, DiagnosticSeverity::WARNING),
                // Over the multibyte identifier.
                at(4, 11, 16, DiagnosticSeverity::INFORMATION),
                // Spanning more than one line.
                at(3, 0, 2, DiagnosticSeverity::HINT),
            ],
            cx,
        );
    });

    workspace.read_with(cx, |workspace, _cx| {
        assert_eq!(
            workspace.lsp_label_for_test(),
            "1 ✕  1 ⚠",
            "the status bar counts errors and warnings, not hints"
        );
    });

    draw(cx);
}

// --- find and replace (#80) ---------------------------------------------------------
//
// The same boundary applies as everywhere else in this file: these prove the bar builds,
// lays out and paints inside the real workspace, and that the wiring between the bar and
// the document is connected. They do **not** prove the bar looks right, or that a match
// highlight lands on the correct pixels — that is #35.

/// Opens a workspace with one PHP document, the fixture every find test below wants.
fn workspace_with_php<'a>(
    cx: &'a mut TestAppContext,
    text: &str,
) -> (gpui::Entity<WorkspaceView>, &'a mut VisualTestContext) {
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    let text = text.to_string();
    workspace.update_in(cx, |workspace, window, cx| {
        let document = Document::new(Some(std::path::PathBuf::from("User.php")), &text, true)
            .expect("php grammar loads");
        workspace.open_document_for_test(document, window, cx);
    });
    (workspace, cx)
}

#[gpui::test]
async fn the_find_bar_renders_and_highlights_matches(cx: &mut TestAppContext) {
    let (workspace, cx) =
        workspace_with_php(cx, "<?php\n$user = 1;\n$user = 2;\n$other = $user;\n");

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.find_for_test(false, window, cx);
    });

    let bar = workspace
        .read_with(cx, |workspace, _cx| workspace.find_bar_for_test())
        .expect("⌘F opens the bar");

    bar.update(cx, |bar, cx| bar.type_query_for_test("$user", cx));

    // The matches reached the document, which is what the highlight reads from.
    workspace.read_with(cx, |workspace, cx| {
        let editor = workspace.active_editor_for_test().expect("a tab is open");
        assert_eq!(editor.read(cx).document.search.matches().len(), 3);
    });
    bar.read_with(cx, |bar, _cx| {
        assert_eq!(bar.status_for_test(), Status::Counted { current: None, total: 3 });
    });

    // And the whole thing paints, in both themes, with the highlights live.
    draw(cx);
}

#[gpui::test]
async fn the_replace_row_renders_and_replace_all_edits_the_buffer(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_with_php(cx, "<?php\n$a = 1;\n$a = 2;\n");

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.find_for_test(true, window, cx);
    });
    let bar = workspace
        .read_with(cx, |workspace, _cx| workspace.find_bar_for_test())
        .expect("⌘⌥F opens the bar");

    bar.update(cx, |bar, cx| {
        bar.set_replacement_for_test("$b");
        bar.type_query_for_test("$a", cx);
    });
    draw(cx);

    bar.update(cx, |_bar, cx| cx.emit(FindEvent::ReplaceAll));

    workspace.read_with(cx, |workspace, cx| {
        let editor = workspace.active_editor_for_test().expect("a tab is open");
        assert_eq!(editor.read(cx).document.buffer.text(), "<?php\n$b = 1;\n$b = 2;\n");
    });
    draw(cx);
}

#[gpui::test]
async fn reopening_find_keeps_the_query_and_escape_clears_it(cx: &mut TestAppContext) {
    // The two constraints the issue spells out: ⌘F while open refocuses rather than
    // reopening, and escape dismisses the bar without closing the tab.
    let (workspace, cx) = workspace_with_php(cx, "<?php\n$user = 1;\n");

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.find_for_test(false, window, cx);
    });
    let bar = workspace
        .read_with(cx, |workspace, _cx| workspace.find_bar_for_test())
        .expect("the bar opened");
    bar.update(cx, |bar, cx| bar.type_query_for_test("$user", cx));

    // ⌘F again: same entity, same query, and now with the replace row after ⌘⌥F.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.find_for_test(false, window, cx);
        workspace.find_for_test(true, window, cx);
    });
    let again = workspace
        .read_with(cx, |workspace, _cx| workspace.find_bar_for_test())
        .expect("the bar is still open");
    assert_eq!(again.entity_id(), bar.entity_id(), "⌘F must refocus, not build a second bar");
    again.read_with(cx, |bar, _cx| {
        assert_eq!(bar.query().pattern, "$user", "the typed query survived");
        assert!(bar.replacing, "⌘⌥F revealed the replace row on the open bar");
    });
    draw(cx);

    // Escape: the bar goes, the tab stays, and the highlights clear.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.dismiss_find_for_test(window, cx);
    });
    workspace.read_with(cx, |workspace, cx| {
        assert!(workspace.find_bar_for_test().is_none(), "escape closed the bar");
        let editor = workspace.active_editor_for_test().expect("escape must not close the tab");
        assert!(editor.read(cx).document.search.matches().is_empty(), "highlights cleared");
    });
    draw(cx);
}

#[gpui::test]
async fn find_seeds_from_the_selection_and_navigates(cx: &mut TestAppContext) {
    let (workspace, cx) = workspace_with_php(cx, "<?php\n$user = 1;\n$user = 2;\n");

    // Select `$user` on line 2, the way a double-click would.
    let editor = workspace
        .read_with(cx, |workspace, _cx| workspace.active_editor_for_test())
        .expect("a tab is open");
    editor.update(cx, |editor, _cx| {
        editor.document.select_range_for_test(6..11);
    });

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.find_for_test(false, window, cx);
    });
    let bar = workspace
        .read_with(cx, |workspace, _cx| workspace.find_bar_for_test())
        .expect("the bar opened");
    bar.read_with(cx, |bar, _cx| {
        assert_eq!(bar.query().pattern, "$user", "seeded from selection");
    });

    editor.read_with(cx, |editor, _cx| {
        assert_eq!(editor.document.search.matches().len(), 2, "the seed searched immediately");
    });

    // ⌘G walks to the second, and the count follows.
    bar.update(cx, |_bar, cx| cx.emit(FindEvent::Navigate { forward: true }));
    editor.read_with(cx, |editor, _cx| {
        assert_eq!(editor.document.selection.range(), 17..22, "the second match is selected");
    });
    bar.read_with(cx, |bar, _cx| {
        assert_eq!(bar.status_for_test(), Status::Counted { current: Some(2), total: 2 });
    });
    draw(cx);
}

#[gpui::test]
async fn find_renders_over_multibyte_text(cx: &mut TestAppContext) {
    // The accented corpus this repo already uses elsewhere. A match boundary landing
    // mid-codepoint is a debug-build panic during paint, so this is the test that would
    // actually catch it — the unit tests catch the offsets, this catches the paint.
    let (workspace, cx) = workspace_with_php(cx, "<?php\n$ação = 'não';\n$função = $ação;\n");

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.find_for_test(false, window, cx);
    });
    let bar = workspace
        .read_with(cx, |workspace, _cx| workspace.find_bar_for_test())
        .expect("the bar opened");
    bar.update(cx, |bar, cx| bar.type_query_for_test("ção", cx));

    workspace.read_with(cx, |workspace, cx| {
        let editor = workspace.active_editor_for_test().expect("a tab is open");
        assert_eq!(editor.read(cx).document.search.matches().len(), 3);
    });
    draw(cx);
}

#[gpui::test]
async fn switching_tabs_with_the_bar_open_searches_the_new_file(cx: &mut TestAppContext) {
    // The bar belongs to the window, not the tab, so the query has to follow the user to
    // whatever file is now on screen. `apply_search` runs from `render` for exactly this,
    // and painting twice here is also what would hang if that call notified unconditionally.
    let (workspace, cx) = workspace_with_php(cx, "<?php\n$user = 1;\n");

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.find_for_test(false, window, cx);
    });
    let bar = workspace
        .read_with(cx, |workspace, _cx| workspace.find_bar_for_test())
        .expect("the bar opened");
    bar.update(cx, |bar, cx| bar.type_query_for_test("$user", cx));
    draw(cx);

    // A second tab, with three hits instead of one.
    workspace.update_in(cx, |workspace, window, cx| {
        let document = Document::new(
            Some(std::path::PathBuf::from("Post.php")),
            "<?php\n$user = 1;\n$user = 2;\n$user = 3;\n",
            true,
        )
        .expect("php grammar loads");
        workspace.open_document_for_test(document, window, cx);
    });
    draw(cx);

    workspace.read_with(cx, |workspace, cx| {
        let editor = workspace.active_editor_for_test().expect("the new tab is active");
        assert_eq!(
            editor.read(cx).document.search.matches().len(),
            3,
            "the query followed the user to the newly active file"
        );
    });
}

#[gpui::test]
async fn navigating_by_command_leaves_the_keyboard_in_the_editor(cx: &mut TestAppContext) {
    // #95, reported from real use: a command-driven jump landed on the right line and then
    // ignored the keyboard until the user clicked. `open_path_at` had no `Window` and so
    // could not focus anything, while the file tree's `open_path` did — the same action
    // through two doors, one of which left focus behind.
    //
    // The assertion is typing, not `window.focused()`. Focus is only interesting here
    // because keystrokes follow it, and a test that checks the handle would still pass if
    // `EditorView` stopped tracking that handle in `render`.
    let (workspace, cx) = workspace_with_php(cx, "<?php\n$user = 1;\n$user = 2;\n");
    draw(cx);

    // Focus starts on the workspace root, not the editor — the state after dismissing a
    // palette, which is exactly how ⌘⇧O and the route palette reach an open. Without this
    // the editor would already hold focus from the open that built the tab, and the test
    // would pass no matter what `open_path_at` did.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_root_for_test(window, cx);
    });
    workspace.update_in(cx, |workspace, window, cx| {
        let editor = workspace.active_editor_for_test().expect("a tab is open");
        assert!(
            !gpui::Focusable::focus_handle(editor.read(cx), cx).is_focused(window),
            "the editor must start unfocused or this test proves nothing"
        );
    });

    // The already-open branch: go-to-definition within the file you are reading, which is
    // the common case and the one that looks most like the command simply worked.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_path_at(
            std::path::PathBuf::from("User.php"),
            Some(elle_text::Point { row: 2, column: 0 }),
            window,
            cx,
        );
    });

    let editor = workspace
        .read_with(cx, |workspace, _cx| workspace.active_editor_for_test())
        .expect("the jump kept the tab active");
    editor.read_with(cx, |editor, _cx| {
        assert_eq!(editor.document.cursor_point().row, 2, "the jump moved the cursor");
    });

    cx.simulate_input("x");
    editor.read_with(cx, |editor, _cx| {
        assert!(
            editor.document.buffer.text().contains('x'),
            "typing straight after a command-driven jump must reach the buffer — it did not, \
             so focus was left behind and the user has to click first (#95)"
        );
    });
}

#[gpui::test]
async fn opening_a_file_from_disk_also_lands_the_keyboard_in_it(cx: &mut TestAppContext) {
    // The other half of #95. The freshly-loaded branch focuses *after* an await, so it needs
    // `spawn_in`/`update_in` rather than the plain `spawn` it had — a different mechanism
    // from the already-open branch above, and so worth its own test rather than assuming
    // one fix covered both.
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("Post.php");
    std::fs::write(&path, "<?php\nclass Post {}\n").expect("write the fixture");

    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_path(path.clone(), window, cx);
    });
    // The read happens on the background executor, so the tab does not exist yet.
    cx.run_until_parked();

    let editor = workspace
        .read_with(cx, |workspace, _cx| workspace.active_editor_for_test())
        .expect("the file loaded into a tab");
    workspace.update_in(cx, |_workspace, window, cx| {
        assert!(
            gpui::Focusable::focus_handle(editor.read(cx), cx).is_focused(window),
            "a file opened from disk must hold the keyboard without a click (#95)"
        );
    });
    draw(cx);
}

#[gpui::test]
async fn find_on_an_empty_workspace_does_nothing(cx: &mut TestAppContext) {
    // No tab means nothing to search; opening a bar over the empty-state hint would be a
    // control that cannot do anything.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.find_for_test(false, window, cx);
    });
    workspace.read_with(cx, |workspace, _cx| {
        assert!(workspace.find_bar_for_test().is_none(), "⌘F with no tab must not open a bar");
    });
    draw(cx);
}

// --- terminal close and split (#97) -----------------------------------------------------

/// A split panel survives a real layout and paint pass, in every theme.
///
/// The reason this is a render test and not a unit test: splitting is layout. The bug it
/// guards against is a pane laid out at zero width, a `flex_1` that never resolves, or a
/// grid whose canvas measures itself into a division by zero — none of which a test on the
/// split *state* would see.
///
/// It does **not** prove the two halves look right or sit side by side; per this file's
/// header, geometry needs a human and stays on #35.
#[gpui::test]
async fn a_split_terminal_renders_in_every_theme(cx: &mut TestAppContext) {
    install_theme(cx);
    let (terminal, cx) = cx.add_window_view(|_window, cx| TerminalView::new(cx));

    terminal.update(cx, |terminal, cx| {
        terminal.open_shell_for_test("cat", cx);
    });
    terminal.update_in(cx, |terminal, window, cx| {
        terminal.split_for_test(window, cx);
    });

    terminal.read_with(cx, |terminal, _cx| {
        assert!(terminal.is_split(), "⌘D with one session open must produce a split");
        assert_eq!(terminal.session_count(), 2, "a split spawns a second shell, not a second view");
    });

    draw(cx);
}

/// ⌘D toggles: a second press returns to one pane and leaves both shells alone.
///
/// Unsplitting deliberately does not close anything — the session stays in the tab strip.
/// Killing a shell is destructive and must go through the confirm prompt, never through a
/// layout command.
#[gpui::test]
async fn unsplitting_keeps_both_sessions(cx: &mut TestAppContext) {
    install_theme(cx);
    let (terminal, cx) = cx.add_window_view(|_window, cx| TerminalView::new(cx));

    terminal.update(cx, |terminal, cx| terminal.open_shell_for_test("cat", cx));
    terminal.update_in(cx, |terminal, window, cx| terminal.split_for_test(window, cx));
    terminal.update_in(cx, |terminal, window, cx| terminal.split_for_test(window, cx));

    terminal.read_with(cx, |terminal, _cx| {
        assert!(!terminal.is_split(), "a second ⌘D must collapse back to one pane");
        assert_eq!(
            terminal.session_count(),
            2,
            "unsplitting is a layout change: it must not kill a shell"
        );
    });

    draw(cx);
}

/// Closing the session a split was showing must drop the split, not leave an empty half.
///
/// The regression this pins is a stale `SessionId` in `split`: the pane would keep asking
/// the manager for a session that no longer exists and render nothing beside a live grid.
#[gpui::test]
async fn closing_a_split_pane_collapses_the_split(cx: &mut TestAppContext) {
    install_theme(cx);
    let (terminal, cx) = cx.add_window_view(|_window, cx| TerminalView::new(cx));

    terminal.update(cx, |terminal, cx| terminal.open_shell_for_test("cat", cx));
    terminal.update_in(cx, |terminal, window, cx| terminal.split_for_test(window, cx));

    // `close_active` rather than the prompt: the prompt is the *question*, and asking it
    // needs a real dialog. What this test is about is the state after the answer.
    terminal.update(cx, |terminal, cx| terminal.close_active(cx));

    terminal.read_with(cx, |terminal, _cx| {
        assert_eq!(terminal.session_count(), 1, "one shell closed");
        assert!(!terminal.is_split(), "a split with only one session left is not a split");
    });

    draw(cx);
}

/// Splitting with nothing open leaves one pane rather than a split with an empty half.
#[gpui::test]
async fn splitting_an_empty_panel_opens_a_single_session(cx: &mut TestAppContext) {
    install_theme(cx);
    let (terminal, cx) = cx.add_window_view(|_window, cx| TerminalView::new(cx));

    terminal.update_in(cx, |terminal, window, cx| terminal.split_for_test(window, cx));

    terminal.read_with(cx, |terminal, _cx| {
        assert!(
            !terminal.is_split(),
            "there was nothing to split from, so one pane is the honest result"
        );
    });

    draw(cx);
}

/// The perf constraint from #97: two terminals must not mean two timers.
///
/// The panel drives repaints from a 16ms poll, and the idle-CPU gate sits at 2% over gpui's
/// own ~0.5-0.9% display-link floor (#93) — so a timer per pane would be a real, measurable
/// regression rather than a theoretical one.
///
/// It counts timer *spawns* rather than checking `poll.is_some()`. The field is an
/// `Option`, so "is it set" can only ever answer one, and that version of this test passes
/// even with `ensure_polling`'s early return deleted — I made that mutation and watched it
/// stay green before rewriting it this way. The cost being guarded is the spawn, so the
/// spawn is what is counted.
///
/// Honest about its limit: this pins the *invariant*, not a CPU number. Measuring the real
/// idle cost of a split panel needs the app on a screen with keystrokes driven into it, and
/// keystroke injection is denied in this environment — so the wall-clock figure stays on
/// #35's human list alongside the rest of the geometry.
#[gpui::test]
async fn splitting_does_not_start_a_second_timer(cx: &mut TestAppContext) {
    install_theme(cx);
    let (terminal, cx) = cx.add_window_view(|_window, cx| TerminalView::new(cx));

    terminal.update(cx, |terminal, cx| terminal.open_shell_for_test("cat", cx));
    terminal.read_with(cx, |terminal, _cx| {
        assert_eq!(terminal.polls_started_for_test(), 1, "one session starts one timer");
    });

    terminal.update_in(cx, |terminal, window, cx| terminal.split_for_test(window, cx));

    terminal.read_with(cx, |terminal, _cx| {
        assert_eq!(terminal.session_count(), 2, "the split spawned a second shell");
        assert!(terminal.is_split());
        assert_eq!(
            terminal.polls_started_for_test(),
            1,
            "a split must reuse the existing timer, not start a second"
        );
    });

    // Closing back down to nothing stops it, rather than leaving a timer behind for a panel
    // with no sessions.
    terminal.update(cx, |terminal, cx| {
        terminal.close_active(cx);
        terminal.close_active(cx);
    });
    terminal.read_with(cx, |terminal, _cx| {
        assert_eq!(terminal.session_count(), 0);
        assert!(!terminal.is_polling_for_test(), "an empty panel must not keep polling");
    });
}

// --- find in project (#80) ---------------------------------------------------------

/// A small on-disk project, so a project search has something real to walk.
///
/// A temp directory rather than a fixture in the repo: the search's rules are about
/// `.gitignore`, hidden files and `vendor/`, and none of those can be exercised against a
/// tree that this repo's own `.gitignore` is already governing.
fn project_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = dir.path();

    std::fs::create_dir_all(root.join("app/Models")).unwrap();
    std::fs::create_dir_all(root.join("vendor/laravel")).unwrap();
    std::fs::write(root.join(".gitignore"), "/vendor\n").unwrap();
    std::fs::write(
        root.join("app/Models/User.php"),
        "<?php\nclass User\n{\n    public $needle = 1;\n    // needle again\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("app/Models/Post.php"), "<?php\n// nothing to see\n").unwrap();
    // Accented text on disk, which is where a byte/char confusion actually bites.
    std::fs::write(root.join("notas.txt"), "a função needle\nsem nada\n").unwrap();
    std::fs::write(root.join("vendor/laravel/Str.php"), "<?php $needle;").unwrap();

    dir
}

/// A workspace pointed at `root`, with the search panel open.
fn workspace_searching(
    cx: &mut TestAppContext,
    root: std::path::PathBuf,
) -> (gpui::Entity<WorkspaceView>, &mut VisualTestContext) {
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));

    workspace.update(cx, |workspace, cx| workspace.open_folder_for_test(root, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.find_in_project_for_test(window, cx);
    });
    (workspace, cx)
}

/// Lets the debounce fire and the background sweep land.
///
/// `advance_clock` past `SEARCH_DEBOUNCE` and then park: the timer and the walk are both
/// on gpui's executor (ADR-0007), so the test controls time rather than sleeping — a real
/// sleep here would be a flaky test that measures the machine.
fn finish_search(cx: &mut VisualTestContext) {
    cx.executor().advance_clock(std::time::Duration::from_millis(400));
    cx.run_until_parked();
}

#[gpui::test]
async fn the_search_panel_opens_finds_hits_and_renders_them(cx: &mut TestAppContext) {
    let dir = project_fixture();
    let (workspace, cx) = workspace_searching(cx, dir.path().to_path_buf());

    let panel = workspace
        .read_with(cx, |workspace, _cx| workspace.search_panel_for_test())
        .expect("⌘⇧F opens the panel");

    // The empty panel paints before any query — the state a user sees first.
    draw(cx);

    panel.update(cx, |panel, cx| panel.type_query_for_test("needle", cx));
    finish_search(cx);

    panel.read_with(cx, |panel, _cx| {
        let SearchState::Done(results) = panel.state() else {
            panic!("the search should have finished: {:?}", panel.state());
        };
        // User.php has two, notas.txt has one. vendor/ is gitignored and must not appear.
        assert_eq!(results.file_count(), 2, "{:?}", results.files);
        assert_eq!(results.match_count(), 3);
        assert!(!results.files.iter().any(|f| f.relative.starts_with("vendor/")));
        // Three lines plus two file headers.
        assert_eq!(panel.row_count_for_test(), 5);
    });

    // And a full layout/paint pass with the results in the list, in both themes.
    draw(cx);
}

#[gpui::test]
async fn clicking_a_result_opens_the_file_at_the_line(cx: &mut TestAppContext) {
    // The whole point of the feature: a result is a jump. This asserts the row's `Point`
    // reaches `open_path_at` and lands where the hit is, which is the plumbing #88 built
    // and this must not reimplement.
    let dir = project_fixture();
    let (workspace, cx) = workspace_searching(cx, dir.path().to_path_buf());

    let panel = workspace
        .read_with(cx, |workspace, _cx| workspace.search_panel_for_test())
        .expect("the panel is open");
    panel.update(cx, |panel, cx| panel.type_query_for_test("needle", cx));
    finish_search(cx);

    // The User.php row whose line is `    // needle again` — row 4, zero-based.
    let (file, line, path, row, column) = panel.read_with(cx, |panel, _cx| {
        let SearchState::Done(results) = panel.state() else { panic!("not finished") };
        let file = results
            .files
            .iter()
            .position(|f| f.relative.ends_with("User.php"))
            .expect("User.php has hits");
        let matches = &results.files[file];
        let line = matches
            .lines
            .iter()
            .position(|l| l.text.starts_with("// needle"))
            .expect("the comment line is a hit");
        let hit = &matches.lines[line];
        (file, line, matches.path.clone(), hit.row, hit.column)
    });
    assert!(path.ends_with("User.php"), "{path:?}");
    assert_eq!(row, 4, "`// needle again` is the fifth line, zero-based 4");
    assert_eq!(column, 7, "byte column in the *untrimmed* line: four spaces plus `// `");

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_search_result_for_test(file, line, window, cx)
    });
    cx.run_until_parked();

    workspace.read_with(cx, |workspace, cx| {
        let editor = workspace.active_editor_for_test().expect("the click opened a tab");
        let point = editor.read(cx).document.cursor_point();
        assert_eq!(point.row, 4, "the cursor landed on the matching line");
        assert_eq!(point.column, 7);
    });
    draw(cx);
}

#[gpui::test]
async fn typing_supersedes_the_search_in_flight_rather_than_queueing_it(cx: &mut TestAppContext) {
    // ADR-0007's rule, and the reason there is one `Job::ProjectSearch` slot rather than
    // two. Three queries typed inside the debounce window must produce exactly one search,
    // for the *last* query — not three searches racing to overwrite each other.
    let dir = project_fixture();
    let (workspace, cx) = workspace_searching(cx, dir.path().to_path_buf());

    let panel = workspace
        .read_with(cx, |workspace, _cx| workspace.search_panel_for_test())
        .expect("the panel is open");

    for pattern in ["n", "ne", "needle"] {
        panel.update(cx, |panel, cx| panel.type_query_for_test(pattern, cx));
        // Less than the debounce, so each keystroke drops the timer before it fires.
        cx.executor().advance_clock(std::time::Duration::from_millis(50));
    }
    finish_search(cx);

    panel.read_with(cx, |panel, _cx| {
        let SearchState::Done(results) = panel.state() else { panic!("not finished") };
        // If an earlier query's search had landed, this would be the hit count for `n` or
        // `ne`, both of which match far more than three times in the fixture.
        assert_eq!(results.match_count(), 3, "the last query is the one that ran");
    });
    draw(cx);
}

#[gpui::test]
async fn an_emptied_query_returns_the_panel_to_idle_without_walking(cx: &mut TestAppContext) {
    let dir = project_fixture();
    let (workspace, cx) = workspace_searching(cx, dir.path().to_path_buf());

    let panel = workspace
        .read_with(cx, |workspace, _cx| workspace.search_panel_for_test())
        .expect("the panel is open");
    panel.update(cx, |panel, cx| panel.type_query_for_test("needle", cx));
    finish_search(cx);
    panel.read_with(cx, |panel, _cx| assert!(matches!(panel.state(), SearchState::Done(_))));

    // Backspacing to nothing must not say "Searching…" and must not walk the project.
    panel.update(cx, |panel, cx| panel.type_query_for_test("", cx));
    panel.read_with(cx, |panel, _cx| {
        assert!(matches!(panel.state(), SearchState::Idle), "{:?}", panel.state());
        assert_eq!(panel.state().summary().0, "");
        assert_eq!(panel.row_count_for_test(), 0);
    });
    draw(cx);
}

#[gpui::test]
async fn an_accented_hit_renders_without_slicing_a_codepoint(cx: &mut TestAppContext) {
    // A match landing mid-codepoint is a debug-build panic the moment the row is laid out,
    // and this repo's own corpus is Portuguese. Searching for the accented word itself is
    // the case where a byte/char confusion produces one.
    let dir = project_fixture();
    let (workspace, cx) = workspace_searching(cx, dir.path().to_path_buf());

    let panel = workspace
        .read_with(cx, |workspace, _cx| workspace.search_panel_for_test())
        .expect("the panel is open");
    panel.update(cx, |panel, cx| panel.type_query_for_test("função", cx));
    finish_search(cx);

    panel.read_with(cx, |panel, _cx| {
        let SearchState::Done(results) = panel.state() else { panic!("not finished") };
        assert_eq!(results.match_count(), 1);
        let hit = &results.files[0].lines[0];
        assert_eq!(&hit.text[hit.ranges[0].clone()], "função");
    });
    // The paint pass is the assertion: it slices `text` with `ranges` for real.
    draw(cx);
}

#[gpui::test]
async fn the_search_panel_toggles_with_the_file_tree_and_keeps_its_results(
    cx: &mut TestAppContext,
) {
    // ⌘⇧F is a toggle over the sidebar #64 introduced: pressing it again goes back to the
    // tree. What it must *not* do is destroy the results — the panel outlives being hidden,
    // for the same reason the git panel does, because re-running a project walk to show a
    // list you already had is the cost this whole feature is trying to avoid.
    let dir = project_fixture();
    let (workspace, cx) = workspace_searching(cx, dir.path().to_path_buf());

    let panel = workspace
        .read_with(cx, |workspace, _cx| workspace.search_panel_for_test())
        .expect("⌘⇧F opens the panel");
    panel.update(cx, |panel, cx| panel.type_query_for_test("needle", cx));
    finish_search(cx);

    workspace.read_with(cx, |workspace, _cx| {
        assert!(workspace.search_panel_is_showing_for_test(), "the sidebar is showing Search");
    });
    draw(cx);

    // ⌘⇧F again, with the panel focused, returns the tree.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.find_in_project_for_test(window, cx);
    });
    workspace.read_with(cx, |workspace, _cx| {
        assert!(
            !workspace.search_panel_is_showing_for_test(),
            "the second press went back to the tree"
        );
    });
    draw(cx);

    // The results survived being hidden. Asserting on the panel rather than on the
    // sidebar, because this is the half a `sidebar == Explorer` check cannot see.
    panel.read_with(cx, |panel, _cx| {
        let SearchState::Done(results) = panel.state() else {
            panic!("hiding the panel threw the results away: {:?}", panel.state());
        };
        assert_eq!(results.match_count(), 3, "the same results are still there");
    });

    // And a third press brings them back without re-searching.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.find_in_project_for_test(window, cx);
    });
    workspace.read_with(cx, |workspace, _cx| {
        assert!(workspace.search_panel_is_showing_for_test());
    });
    draw(cx);
}

#[gpui::test]
async fn searching_with_no_folder_open_renders_a_hint_rather_than_no_results(
    cx: &mut TestAppContext,
) {
    // There is nothing to walk, so every query would answer "No results" — which reads as
    // a broken search rather than a missing project.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.find_in_project_for_test(window, cx);
    });
    let panel = workspace
        .read_with(cx, |workspace, _cx| workspace.search_panel_for_test())
        .expect("the panel still opens; it is the *results* that need a folder");

    panel.update(cx, |panel, cx| panel.type_query_for_test("needle", cx));
    finish_search(cx);

    panel.read_with(cx, |panel, _cx| {
        assert!(matches!(panel.state(), SearchState::Idle), "no root means no search at all");
    });
    draw(cx);
}

// --- the completion popup (#61) ---------------------------------------------------------
//
// What these can and cannot establish is worth stating, because the boundary is the same one
// this module's header describes and it bites hardest here. gpui's headless text system is a
// fake perfect monospace (`600.0 * len_utf16`), so **none of these verify where the popup
// lands on screen**. `completion::place` is unit-tested against numbers instead — that is the
// arithmetic that flips and clamps — and whether the cursor's own measured x is right stays
// on issue #35's human list, exactly as the caret's does.
//
// What they do establish: the popup renders under every theme without panicking, the buffer
// still receives text while the popup holds focus, an accepted item replaces the word being
// typed rather than appending to it, and no source answering stays silent.

/// Opens a PHP file and returns the workspace, ready for ⌃space.
fn open_php(workspace: &gpui::Entity<WorkspaceView>, cx: &mut VisualTestContext) {
    workspace.update_in(cx, |workspace, window, cx| {
        let document =
            Document::new(Some(std::path::PathBuf::from("User.php")), "<?php\n\n$user->\n", true)
                .expect("php grammar loads");
        workspace.open_document_for_test(document, window, cx);
        // The cursor goes where a user completing a member access would have left it: at the
        // end of `$user->`, which is the position both sources are asked about.
        if let Some(editor) = workspace.active_editor_for_test() {
            editor.update(cx, |editor, _cx| {
                let end = editor.document.buffer.len_bytes() - 1;
                editor.document.move_to(end, false);
            });
        }
    });
}

fn lsp_item(label: &str) -> CompletionItem {
    CompletionItem::new(label.to_string(), CompletionSource::Lsp)
}

#[gpui::test]
async fn the_completion_popup_renders_with_items_from_both_sources(cx: &mut TestAppContext) {
    // One list, two provenances, rendered under both themes. The badge is a *sibling* of the
    // label in the row layout, so a long label plus a detail plus a badge is the case that
    // would break a row built as a single string — the layout #61 says must not be bolted on
    // later.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    workspace.update_in(cx, |workspace, window, cx| workspace.complete_for_test(window, cx));

    let popup = workspace
        .read_with(cx, |workspace, _cx| workspace.completion_for_test())
        .expect("⌃space in an open PHP file opens the popup");

    workspace.update(cx, |workspace, cx| {
        workspace.offer_completions_for_test(
            vec![
                lsp_item("getName"),
                CompletionItem::new("users.show".to_string(), CompletionSource::LaravelRoute),
                lsp_item("getNamespace").with_detail(Some("string".into())),
            ],
            cx,
        );
    });

    popup.read_with(cx, |popup, _cx| {
        let items = popup.visible_items();
        assert_eq!(items.len(), 3, "every offered item is shown before anything is typed");
        // The property that matters: each row still knows its own source after passing
        // through the list. Nothing here infers it from the label.
        assert_eq!(items[0].source, CompletionSource::Lsp);
        assert_eq!(items[1].source, CompletionSource::LaravelRoute);
    });

    draw(cx);
}

#[gpui::test]
async fn the_popup_renders_while_its_sources_are_still_answering(cx: &mut TestAppContext) {
    // The state a user sees *first*, and the one an empty-list renderer panics in. It must
    // say "Completing…" rather than "No completions": with a source still to report, saying
    // there are none is a claim nobody has established.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    workspace.update_in(cx, |workspace, window, cx| workspace.complete_for_test(window, cx));
    workspace
        .read_with(cx, |workspace, _cx| workspace.completion_for_test())
        .expect("the popup opens before any source has answered");

    draw(cx);
}

#[gpui::test]
async fn typing_while_the_popup_is_open_still_reaches_the_buffer(cx: &mut TestAppContext) {
    // The failure this popup exists to avoid. The popup holds keyboard focus — that is what
    // makes its arrows work without stealing the editor's — so a character typed while it is
    // open arrives at the *popup*. If it stopped there, ⌃space would become a mode where
    // typing is silently swallowed.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    let editor = workspace.update_in(cx, |workspace, window, cx| {
        workspace.complete_for_test(window, cx);
        workspace.active_editor_for_test().expect("a file is open")
    });
    let before = editor.read_with(cx, |editor, _cx| editor.document.buffer.text());

    workspace.update(cx, |workspace, cx| {
        workspace.offer_completions_for_test(vec![lsp_item("getName"), lsp_item("setName")], cx);
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.completion_typed_for_test("g", window, cx);
    });

    let after = editor.read_with(cx, |editor, _cx| editor.document.buffer.text());
    assert_ne!(before, after, "the character must reach the buffer, not only the filter");
    assert!(after.contains("$user->g"), "and it must land at the cursor: {after:?}");

    // And it narrowed the list, which is the other half of the same keystroke.
    let popup = workspace
        .read_with(cx, |workspace, _cx| workspace.completion_for_test())
        .expect("one match remains, so the popup stays open");
    popup.read_with(cx, |popup, _cx| {
        let items = popup.visible_items();
        assert_eq!(items.len(), 1, "typing `g` narrows to getName");
        assert_eq!(items[0].label, "getName");
    });

    draw(cx);
}

#[gpui::test]
async fn typing_past_every_match_closes_the_popup(cx: &mut TestAppContext) {
    // A popup that follows the user down the line saying "No completions" is a popup in the
    // way. Every editor closes instead — and the character must still reach the buffer on
    // the way out, which is the half that would be easy to lose.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    let editor = workspace.update_in(cx, |workspace, window, cx| {
        workspace.complete_for_test(window, cx);
        workspace.active_editor_for_test().expect("a file is open")
    });

    workspace.update(cx, |workspace, cx| {
        workspace.offer_completions_for_test(vec![lsp_item("getName")], cx);
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.completion_typed_for_test("z", window, cx);
    });

    assert!(
        workspace.read_with(cx, |workspace, _cx| workspace.completion_for_test().is_none()),
        "nothing matches `z`, so the list closes rather than sitting there empty"
    );
    let text = editor.read_with(cx, |editor, _cx| editor.document.buffer.text());
    assert!(text.contains("$user->z"), "the keystroke is not lost on the way out: {text:?}");

    draw(cx);
}

#[gpui::test]
async fn accepting_replaces_the_word_being_typed_rather_than_appending(cx: &mut TestAppContext) {
    // The bug a wrong replace-range gives you is `$user->gegetName`. Typing narrows the list
    // *and* moves the cursor, so the accepted item has to overwrite everything typed since
    // the popup opened — which is why the range is `word_start..cursor` and not an insertion
    // point.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    let editor = workspace.update_in(cx, |workspace, window, cx| {
        workspace.complete_for_test(window, cx);
        workspace.active_editor_for_test().expect("a file is open")
    });

    workspace.update(cx, |workspace, cx| {
        workspace.offer_completions_for_test(vec![lsp_item("getName")], cx);
    });
    // Two characters typed while the list narrows.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.completion_typed_for_test("g", window, cx);
        workspace.completion_typed_for_test("e", window, cx);
    });

    let popup = workspace
        .read_with(cx, |workspace, _cx| workspace.completion_for_test())
        .expect("`ge` still matches getName");
    let item = popup.read_with(cx, |popup, _cx| popup.visible_items()[0].clone());

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.accept_completion_for_test(item, window, cx);
    });

    let text = editor.read_with(cx, |editor, _cx| editor.document.buffer.text());
    assert!(text.contains("$user->getName"), "got {text:?}");
    assert!(!text.contains("gegetName"), "the typed prefix must be overwritten: {text:?}");

    // Accepting closes the popup. Escape and accept both have to leave the user where they
    // were typing rather than in the workspace, which is the difference from the palette.
    assert!(workspace.read_with(cx, |workspace, _cx| workspace.completion_for_test().is_none()));

    draw(cx);
}

#[gpui::test]
async fn dismissing_the_popup_leaves_the_buffer_alone(cx: &mut TestAppContext) {
    // Escape must not write anything. The range the popup was holding is dropped rather than
    // used — #83 documented the ordering where taking it after the dismissal instead made the
    // completion silently never fire, and the mirror image writes into the wrong place.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    let editor = workspace.update_in(cx, |workspace, window, cx| {
        workspace.complete_for_test(window, cx);
        workspace.active_editor_for_test().expect("a file is open")
    });
    let before = editor.read_with(cx, |editor, _cx| editor.document.buffer.text());

    workspace.update(cx, |workspace, cx| {
        workspace.offer_completions_for_test(vec![lsp_item("getName")], cx);
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.dismiss_completion_for_test(window, cx);
    });

    assert!(workspace.read_with(cx, |workspace, _cx| workspace.completion_for_test().is_none()));
    assert_eq!(
        editor.read_with(cx, |editor, _cx| editor.document.buffer.text()),
        before,
        "escape writes nothing"
    );

    draw(cx);
}

#[gpui::test]
async fn completing_with_no_language_server_stays_silent(cx: &mut TestAppContext) {
    // #74's rule, which this must not regress: nobody has Intelephense on a fresh machine.
    // No dialog, no status message, no retry — the popup simply has nothing from that
    // source. Every test here runs with no server, so this is the path they all took; it is
    // asserted explicitly because it is a *requirement* rather than an accident of the
    // environment.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    workspace.update_in(cx, |workspace, window, cx| workspace.complete_for_test(window, cx));

    workspace.read_with(cx, |workspace, _cx| {
        assert!(
            workspace.status_for_test().is_none(),
            "a missing language server is not a problem the user has (§24)"
        );
    });

    draw(cx);
}

#[gpui::test]
async fn closing_the_tab_takes_the_popup_with_it(cx: &mut TestAppContext) {
    // ⌘W is workspace-scoped, so it fires while the popup holds focus. Left standing, the
    // popup is anchored to a cursor in a tab that no longer exists and still holds a byte
    // offset into that buffer — and the offset is the dangerous half, because a later accept
    // would write it into whichever document inherited the active slot.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    workspace.update_in(cx, |workspace, window, cx| workspace.complete_for_test(window, cx));
    assert!(
        workspace.read_with(cx, |workspace, _cx| workspace.completion_for_test().is_some()),
        "the popup must be open for this test to be testing anything"
    );

    workspace.update_in(cx, |workspace, window, cx| workspace.close_tab_for_test(window, cx));

    assert!(
        workspace.read_with(cx, |workspace, _cx| workspace.completion_for_test().is_none()),
        "closing the tab must close the popup anchored into it"
    );
    draw(cx);
}

#[gpui::test]
async fn opening_the_palette_closes_the_popup(cx: &mut TestAppContext) {
    // Same shape, different key: the palette's chords are workspace-scoped too, so ⌘P
    // arrives with the popup focused. Two overlays both believing they own the keyboard is a
    // state with no correct behaviour, and the palette is the one the user just asked for.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    workspace.update_in(cx, |workspace, window, cx| workspace.complete_for_test(window, cx));
    assert!(workspace.read_with(cx, |workspace, _cx| workspace.completion_for_test().is_some()));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_palette_for_test(PaletteMode::Commands, window, cx);
    });

    assert!(
        workspace.read_with(cx, |workspace, _cx| workspace.completion_for_test().is_none()),
        "the popup must not survive underneath a palette that now holds focus"
    );
    draw(cx);
}
