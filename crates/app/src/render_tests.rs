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
use gpui::{TestAppContext, VisualTestContext, px, size};

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

/// Opens a PHP file and returns the workspace, ready for an explicit completion invoke.
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
        .expect("invoking completion in an open PHP file opens the popup");

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
    // open arrives at the *popup*. If it stopped there, completion would become a mode where
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
async fn a_typed_character_opens_no_popup_when_no_server_declared_any_trigger(
    cx: &mut TestAppContext,
) {
    // The default state of the app and of every non-PHP project: no language server, so no
    // declared triggers, so typing must behave exactly as it did before #61 existed.
    //
    // This is the honest headless test of the trigger path. `is_completion_trigger` consults
    // a live client's capabilities, and there is no server in a `gpui::test` — so what can be
    // established here is the *negative*, and it is worth establishing because the failure it
    // rules out is a popup opening on every `>` typed by a user who has no PHP server at all.
    // The positive case is covered against a real Intelephense in
    // `crates/lsp/tests/real_server.rs::a_real_server_declares_its_own_trigger_characters`.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    workspace.read_with(cx, |workspace, _cx| {
        // Every character Intelephense declares, none of which anything here has been told
        // about. A hardcoded list anywhere in the trigger path makes each of these true.
        for character in ["$", ">", ":", "\\", "/", "'", "\"", "*", ".", "<", "-"] {
            assert!(
                !workspace.is_completion_trigger_for_test(character),
                "{character:?} must not be a trigger with no server: a `true` here means the \
                 list came from somewhere other than the server's own declaration"
            );
        }
    });

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.editor_typed_for_test(">", window, cx);
    });

    assert!(
        workspace.read_with(cx, |workspace, _cx| workspace.completion_for_test().is_none()),
        "with no server there are no declared triggers, so nothing opens"
    );

    draw(cx);
}

#[test]
fn a_trigger_opens_a_popup_only_when_declared_and_none_is_already_open() {
    // Both halves of the rule, which is why this is a unit test on the predicate rather than
    // a render test on the handler.
    //
    // The render-test version of this was **written first and was vacuous**: a headless test
    // has no language server, so `is_completion_trigger` is always false there, and the
    // handler returns at that check before ever reaching the already-open guard. Deleting the
    // guard left the test passing. That is the trap CONTEXT.md describes, caught here by
    // deleting the guard and watching nothing fail — so the rule was extracted into
    // `should_open_on_trigger`, where both inputs can actually be varied.
    assert!(
        WorkspaceView::should_open_on_trigger_for_test(false, true),
        "a declared trigger with no popup open must open one"
    );
    assert!(
        !WorkspaceView::should_open_on_trigger_for_test(true, true),
        "a declared trigger with a popup already open must not open a second: the keystroke \
         belongs to `completion_typed`, and running both would double it in the query"
    );
    assert!(
        !WorkspaceView::should_open_on_trigger_for_test(false, false),
        "an undeclared character must never open a popup"
    );
    assert!(!WorkspaceView::should_open_on_trigger_for_test(true, false));
}

#[gpui::test]
async fn the_editor_path_does_nothing_while_a_popup_is_open(cx: &mut TestAppContext) {
    // The plumbing half of the rule above, through the real handler. It establishes less than
    // its name might suggest — with no server the trigger check also declines — so the
    // decision itself is pinned by the unit test above. What this adds is that the wiring
    // does not insert, narrow, or replace the popup as a side effect.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    workspace.update_in(cx, |workspace, window, cx| workspace.complete_for_test(window, cx));
    workspace.update(cx, |workspace, cx| {
        workspace.offer_completions_for_test(vec![lsp_item("getName"), lsp_item("getAge")], cx);
    });

    let editor = workspace
        .read_with(cx, |workspace, _cx| workspace.active_editor_for_test())
        .expect("a file is open");
    let before = editor.read_with(cx, |editor, _cx| editor.document.buffer.text());

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.editor_typed_for_test("g", window, cx);
    });

    let after = editor.read_with(cx, |editor, _cx| editor.document.buffer.text());
    assert_eq!(before, after, "the editor path must not also insert while a popup is open");

    let popup = workspace
        .read_with(cx, |workspace, _cx| workspace.completion_for_test())
        .expect("the popup is still the same one");
    popup.read_with(cx, |popup, _cx| {
        assert_eq!(popup.visible_items().len(), 2, "and it must not have narrowed");
    });

    draw(cx);
}

#[gpui::test]
async fn an_incomplete_list_stays_open_even_when_nothing_matches(cx: &mut TestAppContext) {
    // The behaviour `is_incomplete` buys, and the reason it is not merely an optimisation.
    //
    // With a *complete* list, no matches means the user has typed past everything the server
    // had and the popup closes. With an incomplete one it means nothing of the kind: the
    // server truncated its answer, so the rows matching the longer prefix may be exactly the
    // ones it cut. Closing there would delete a popup that the re-request is about to refill.
    //
    // Mutation-checked: making `push_query` ignore `incomplete` closes the popup here.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    workspace.update_in(cx, |workspace, window, cx| workspace.complete_for_test(window, cx));
    // A truncated list, exactly as Intelephense sends one: a hundred rows that happen not to
    // include the thing the user is about to type.
    workspace.update(cx, |workspace, cx| {
        workspace.offer_incomplete_completions_for_test(vec![lsp_item("STREAM_BUFFER_FULL")], cx);
    });

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.completion_typed_for_test("z", window, cx);
    });

    assert!(
        workspace.read_with(cx, |workspace, _cx| workspace.completion_for_test().is_some()),
        "an incomplete list must survive a keystroke that matches none of it — the server's \
         answer was never the whole answer"
    );

    draw(cx);
}

#[gpui::test]
async fn a_complete_list_still_closes_when_nothing_matches(cx: &mut TestAppContext) {
    // The other side of the branch above, and the reason it is a branch rather than a rule.
    // Without this, `an_incomplete_list_stays_open_even_when_nothing_matches` would also pass
    // against an implementation that simply never closed the popup.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    workspace.update_in(cx, |workspace, window, cx| workspace.complete_for_test(window, cx));
    workspace.update(cx, |workspace, cx| {
        workspace.offer_completions_for_test(vec![lsp_item("STREAM_BUFFER_FULL")], cx);
    });

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.completion_typed_for_test("z", window, cx);
    });

    assert!(
        workspace.read_with(cx, |workspace, _cx| workspace.completion_for_test().is_none()),
        "a complete list that matches nothing has genuinely run out, so it closes"
    );

    draw(cx);
}

#[gpui::test]
async fn backspacing_an_incomplete_list_keeps_the_popup_open(cx: &mut TestAppContext) {
    // Backspace widens the prefix, which matches *more* — so a truncated list is even less
    // of the answer than it was, and the popup must survive to be refilled. The complete-list
    // rule is the opposite and unchanged: backspacing past where the popup opened closes it,
    // because the user has left the word.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    let editor = workspace.update_in(cx, |workspace, window, cx| {
        workspace.complete_for_test(window, cx);
        workspace.active_editor_for_test().expect("a file is open")
    });
    workspace.update(cx, |workspace, cx| {
        workspace.offer_incomplete_completions_for_test(vec![lsp_item("STREAM_BUFFER_FULL")], cx);
    });

    // Type a character the list does not contain, then take it back. Both keystrokes have to
    // reach the buffer, which is the half a filter-only implementation loses.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.completion_typed_for_test("z", window, cx);
    });
    let typed = editor.read_with(cx, |editor, _cx| editor.document.buffer.text());
    assert!(typed.contains("$user->z"), "the character reaches the buffer: {typed:?}");

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.completion_backspace_for_test(window, cx);
    });

    let after = editor.read_with(cx, |editor, _cx| editor.document.buffer.text());
    assert!(!after.contains("$user->z"), "and the backspace removes it again: {after:?}");
    assert!(
        workspace.read_with(cx, |workspace, _cx| workspace.completion_for_test().is_some()),
        "an incomplete list must survive the backspace that widens its prefix"
    );

    draw(cx);
}

#[gpui::test]
async fn a_fresh_answer_replaces_the_servers_previous_one_rather_than_stacking(
    cx: &mut TestAppContext,
) {
    // What the re-request would corrupt if it appended. The second answer describes the same
    // source at a longer prefix, so the first is stale — and because filtering preserves
    // source order, appending would leave the stale rows sorting *ahead* of the fresh ones.
    //
    // The route item is the control: a re-request to the language server must not delete what
    // Laravel found, because nothing re-asked Laravel.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    workspace.update_in(cx, |workspace, window, cx| workspace.complete_for_test(window, cx));

    workspace.update(cx, |workspace, cx| {
        workspace.offer_completions_for_test(
            vec![
                CompletionItem::new("users.show".to_string(), CompletionSource::LaravelRoute),
                lsp_item("STREAM_BUFFER_FULL"),
            ],
            cx,
        );
    });

    // The server answers again, as it does after a keystroke on an incomplete list.
    workspace.update(cx, |workspace, cx| {
        workspace.offer_incomplete_completions_for_test(vec![lsp_item("strlen")], cx);
    });

    let popup = workspace
        .read_with(cx, |workspace, _cx| workspace.completion_for_test())
        .expect("the popup is still open");
    popup.read_with(cx, |popup, _cx| {
        let labels: Vec<&str> = popup.visible_items().iter().map(|i| i.label.as_ref()).collect();
        assert!(
            !labels.contains(&"STREAM_BUFFER_FULL"),
            "the server's stale answer must be gone, not stacked underneath: {labels:?}"
        );
        assert!(labels.contains(&"strlen"), "the fresh answer must be there: {labels:?}");
        assert!(
            labels.contains(&"users.show"),
            "and the other source must survive a re-request it was not part of: {labels:?}"
        );
    });

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

#[gpui::test]
async fn accepting_writes_into_the_file_the_popup_was_opened_on(cx: &mut TestAppContext) {
    // The worst bug in this feature, found in review. Clicking a tab sets `active_tab`
    // directly and does not touch the popup, so resolving the target through
    // `active_editor()` at accept time wrote the completion into whichever file was
    // frontmost *then* — at a byte offset that meant something in a different file. The
    // bounds check could not catch it: a longer buffer accepts the offset happily.
    //
    // The popup now holds the editor handle it was opened against, which is the same fix
    // `close_tab_at` already uses for the same reason.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    let first = workspace.update_in(cx, |workspace, window, cx| {
        workspace.complete_for_test(window, cx);
        workspace.active_editor_for_test().expect("the file the popup is about")
    });

    workspace.update(cx, |workspace, cx| {
        workspace.offer_completions_for_test(vec![lsp_item("getName")], cx);
    });

    // A second file, longer than the first so a stale offset would land inside it rather
    // than being rejected by the bounds check — the case that made this silent.
    let second = workspace.update_in(cx, |workspace, window, cx| {
        let document = Document::new(
            Some(std::path::PathBuf::from("Other.php")),
            "<?php\n// a much longer second file, with plenty of room for a stale offset\n$x = 1;\n",
            true,
        )
        .expect("php grammar loads");
        workspace.open_document_for_test(document, window, cx);
        workspace.active_editor_for_test().expect("the second file is now active")
    });
    let untouched = second.read_with(cx, |editor, _cx| editor.document.buffer.text());

    // Put the second file's cursor *past* the popup's offset. Without this the accept is
    // rejected by the `start > end` bounds check and the test passes for the wrong reason —
    // which is what it did when first written, and is exactly the "passed against the bug it
    // was named for" trap. The offset has to be genuinely writable in the wrong file for
    // this test to be about anything.
    second.update(cx, |editor, _cx| {
        let end = editor.document.buffer.len_bytes() - 1;
        editor.document.move_to(end, false);
    });

    // Accept from the popup, which is still the one opened on the first file.
    let item = CompletionItem::new("getName".to_string(), CompletionSource::Lsp);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.accept_completion_for_test(item, window, cx);
    });

    assert_eq!(
        second.read_with(cx, |editor, _cx| editor.document.buffer.text()),
        untouched,
        "the completion must never be written into a file the popup was not about"
    );
    // And the first file is either completed or left alone — never corrupted.
    let first_text = first.read_with(cx, |editor, _cx| editor.document.buffer.text());
    assert!(
        first_text.starts_with("<?php"),
        "the origin file must stay well-formed: {first_text:?}"
    );

    draw(cx);
}

#[gpui::test]
async fn the_tab_close_button_takes_the_popup_with_it(cx: &mut TestAppContext) {
    // ⌘W was fixed first; the ✕ reaches `close_tab_at` directly and is the more common
    // gesture, so the dismissal belongs at that shared choke point rather than in the
    // action handler.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    workspace.update_in(cx, |workspace, window, cx| workspace.complete_for_test(window, cx));
    assert!(workspace.read_with(cx, |workspace, _cx| workspace.completion_for_test().is_some()));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.close_tab_at_for_test(0, window, cx);
    });

    assert!(
        workspace.read_with(cx, |workspace, _cx| workspace.completion_for_test().is_none()),
        "the ✕ must close the popup anchored into the tab it removes"
    );
    draw(cx);
}

#[gpui::test]
async fn anything_that_takes_focus_closes_the_popup(cx: &mut TestAppContext) {
    // The invariant an earlier comment *claimed* and nothing enforced. Without it, ⌘F left
    // the popup on screen but unfocused — its key context inactive, so Escape no longer
    // reached it and it could not be dismissed at all, while still holding an offset a
    // later accept would write at.
    //
    // **What this test actually covers is `open_find`'s explicit `dismiss_completion`.** The
    // popup also carries a focus-out subscription that is the general rule, and this test
    // does *not* exercise it: gpui assembles the focus path during paint, so the listener
    // does not fire in a headless harness. I wrote this expecting the subscription alone to
    // carry it, watched it fail with the popup genuinely focused, and added the explicit
    // call — a rule that cannot be tested should not be the only thing holding.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    workspace.update_in(cx, |workspace, window, cx| workspace.complete_for_test(window, cx));
    assert!(workspace.read_with(cx, |workspace, _cx| workspace.completion_for_test().is_some()));

    // Drawn before the focus is taken away, and that is not test scaffolding: gpui activates
    // a focus listener through `cx.defer`, and the focus itself only truly lands once a frame
    // has been laid out. Asserting on a popup that was never painted would be asserting about
    // a state the real app never passes through.
    draw(cx);
    cx.run_until_parked();

    // ⌘F, which never had a `dismiss_completion` call of its own.
    workspace.update_in(cx, |workspace, window, cx| workspace.find_for_test(false, window, cx));
    // A frame, because gpui dispatches focus listeners while painting rather than at the
    // moment `window.focus` is called.
    draw(cx);
    cx.run_until_parked();

    assert!(
        workspace.read_with(cx, |workspace, _cx| workspace.completion_for_test().is_none()),
        "opening the find bar takes focus, and the popup must not survive losing it"
    );
}

#[gpui::test]
async fn typing_over_an_auto_closed_bracket_closes_the_popup(cx: &mut TestAppContext) {
    // `insert_with_pairs` types *over* an existing closer: the caret moves and the buffer
    // does not grow. Mirroring that keystroke into the filter anyway made the query describe
    // one more byte than the replaced range had — the same divergence as the dotted route
    // name, and an accept would then have overwritten the bracket.
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));

    let editor = workspace.update_in(cx, |workspace, window, cx| {
        let document =
            Document::new(Some(std::path::PathBuf::from("Pairs.php")), "<?php\nfoo\n", true)
                .expect("php grammar loads");
        workspace.open_document_for_test(document, window, cx);
        let editor = workspace.active_editor_for_test().expect("a file is open");
        editor.update(cx, |editor, cx| {
            // Put the caret after `foo` and type `(`, which auto-closes to `foo(|)`.
            let end = editor.document.buffer.len_bytes() - 1;
            editor.document.move_to(end, false);
            editor.insert_typed("(", cx);
        });
        editor
    });

    let with_pair = editor.read_with(cx, |editor, _cx| editor.document.buffer.text());
    assert!(with_pair.contains("foo()"), "the pair must have auto-closed: {with_pair:?}");

    workspace.update_in(cx, |workspace, window, cx| workspace.complete_for_test(window, cx));
    // An item that still matches after a `)` is appended to the query, so the popup would
    // survive on the "nothing matched" path. Without it this test closes for the wrong
    // reason and passes even with the desync bug present — which it did when first written.
    workspace.update(cx, |workspace, cx| {
        workspace.offer_completions_for_test(vec![lsp_item(")brace"), lsp_item("getName")], cx);
    });

    // Typing the closer steps over it rather than inserting.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.completion_typed_for_test(")", window, cx);
    });

    assert!(
        workspace.read_with(cx, |workspace, _cx| workspace.completion_for_test().is_none()),
        "a keystroke that did not land as typed must close the list rather than desync it"
    );
    // And it must not have doubled the bracket.
    let after = editor.read_with(cx, |editor, _cx| editor.document.buffer.text());
    assert!(!after.contains("foo())"), "typing over a closer must not insert one: {after:?}");

    draw(cx);
}

// --- the file tree's context menu (#126) -------------------------------------------

/// A Laravel-shaped folder for the tree tests.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("app")).unwrap();
    std::fs::write(dir.path().join("app/User.php"), "<?php\n").unwrap();
    std::fs::write(dir.path().join("artisan"), "#!/usr/bin/env php\n").unwrap();
    dir
}

/// Right-clicking a row opens a menu about *that* row.
///
/// The report #126 came from was "não dá pra apertar click direito" — there was no
/// right-click handler in the crate at all. This is the assertion that the gesture now
/// reaches something, and that what it offers depends on what was clicked: a directory
/// can hold new files, a file cannot.
#[gpui::test]
async fn right_clicking_a_directory_offers_more_than_a_file(cx: &mut TestAppContext) {
    use crate::context_menu::MenuAction;

    install_theme(cx);
    let dir = project();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        // Directories sort first, so row 0 is `app` and row 1 is `artisan`.
        workspace.right_click_tree_row_for_test(0, window, cx);
    });

    let on_dir = workspace.read_with(cx, |workspace, cx| workspace.menu_actions_for_test(cx));
    let on_dir = on_dir.expect("right-clicking a row must open a menu");
    assert!(on_dir.contains(&MenuAction::NewFile), "a directory can hold a new file");
    assert!(on_dir.contains(&MenuAction::Rename));
    assert!(on_dir.contains(&MenuAction::Delete));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.right_click_tree_row_for_test(1, window, cx);
    });
    let on_file = workspace.read_with(cx, |workspace, cx| workspace.menu_actions_for_test(cx));
    let on_file = on_file.expect("a file row must open a menu too");
    assert!(!on_file.contains(&MenuAction::NewFile), "a file is not somewhere to create one");
    assert!(on_file.contains(&MenuAction::Rename), "a file must still be renameable");

    draw(cx);
}

/// Creating a file writes it, shows it, and opens it.
#[gpui::test]
async fn creating_a_file_from_the_menu_writes_it_and_opens_it(cx: &mut TestAppContext) {
    use crate::context_menu::MenuAction;

    install_theme(cx);
    let dir = project();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        workspace.right_click_tree_row_for_test(0, window, cx);
        workspace.pick_menu_action_for_test(MenuAction::NewFile, window, cx);
        workspace.confirm_name_for_test("Post.php", window, cx);
    });
    cx.run_until_parked();

    assert!(dir.path().join("app/Post.php").exists(), "the file must be on disk");
    workspace.read_with(cx, |workspace, _cx| {
        assert!(!workspace.overlay_is_open_for_test(), "the prompt must close");
        assert_eq!(
            workspace.tab_count_for_test(),
            1,
            "a new file opens, so the user is not left hunting for it in the tree"
        );
    });

    draw(cx);
}

/// Renaming moves the file and leaves the old name behind.
#[gpui::test]
async fn renaming_from_the_menu_moves_the_file(cx: &mut TestAppContext) {
    use crate::context_menu::MenuAction;

    install_theme(cx);
    let dir = project();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        // Row 1 is `artisan`, a file at the root.
        workspace.right_click_tree_row_for_test(1, window, cx);
        workspace.pick_menu_action_for_test(MenuAction::Rename, window, cx);
        workspace.confirm_name_for_test("artisan.bak", window, cx);
    });
    cx.run_until_parked();

    assert!(dir.path().join("artisan.bak").exists());
    assert!(!dir.path().join("artisan").exists());

    // And the tree shows the new name without the user doing anything, which is the half
    // that makes the operation feel finished rather than merely performed.
    workspace.read_with(cx, |workspace, _cx| {
        let names = workspace.tree_names_for_test();
        assert!(names.contains(&"artisan.bak".to_string()), "the tree must refresh: {names:?}");
        assert!(!names.contains(&"artisan".to_string()));
    });

    draw(cx);
}

/// Deleting asks first, and only then deletes.
///
/// The confirmation is the point: delete is the one action in this editor that cannot be
/// undone (there is no trash), so picking `Delete` from the menu must *not* delete anything
/// on its own. A version that deleted on the menu click would pass a test that only checked
/// the file was gone at the end — so this asserts the file survives the menu step.
#[gpui::test]
async fn deleting_asks_before_it_deletes_and_closes_the_tab(cx: &mut TestAppContext) {
    use crate::context_menu::MenuAction;

    install_theme(cx);
    let dir = project();
    let victim = dir.path().join("artisan");
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        let document = Document::new(Some(victim.clone()), "#!/usr/bin/env php\n", true).unwrap();
        workspace.open_document_for_test(document, window, cx);
        workspace.right_click_tree_row_for_test(1, window, cx);
        workspace.pick_menu_action_for_test(MenuAction::Delete, window, cx);
    });
    cx.run_until_parked();

    assert!(victim.exists(), "choosing Delete must only ask; nothing is destroyed yet");
    workspace.read_with(cx, |workspace, _cx| {
        assert!(workspace.overlay_is_open_for_test(), "a confirmation must be on screen");
    });

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.confirm_delete_for_test(window, cx);
    });
    cx.run_until_parked();

    assert!(!victim.exists(), "confirming must delete");
    workspace.read_with(cx, |workspace, _cx| {
        // A tab left open on a deleted file writes it back into existence on the next ⌘S,
        // quietly undoing the delete.
        assert_eq!(workspace.tab_count_for_test(), 0, "the tab on the deleted file must close");
    });

    draw(cx);
}

/// Dismissing the confirmation leaves the file alone.
#[gpui::test]
async fn cancelling_the_confirmation_deletes_nothing(cx: &mut TestAppContext) {
    use crate::context_menu::MenuAction;

    install_theme(cx);
    let dir = project();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        workspace.right_click_tree_row_for_test(0, window, cx);
        workspace.pick_menu_action_for_test(MenuAction::Delete, window, cx);
        workspace.dismiss_overlay_for_test(window, cx);
    });
    cx.run_until_parked();

    assert!(dir.path().join("app/User.php").exists(), "cancelling must destroy nothing");
    workspace.read_with(cx, |workspace, _cx| {
        assert!(!workspace.overlay_is_open_for_test());
    });

    draw(cx);
}

/// A bad name is refused with a message, and nothing is created.
#[gpui::test]
async fn a_name_with_a_slash_is_reported_not_silently_mangled(cx: &mut TestAppContext) {
    use crate::context_menu::MenuAction;

    install_theme(cx);
    let dir = project();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        workspace.right_click_tree_row_for_test(0, window, cx);
        workspace.pick_menu_action_for_test(MenuAction::NewFile, window, cx);
        workspace.confirm_name_for_test("sub/Post.php", window, cx);
    });
    cx.run_until_parked();

    assert!(!dir.path().join("app/sub").exists(), "a slash must not create a directory");
    workspace.read_with(cx, |workspace, _cx| {
        assert!(
            workspace.status_for_test().is_some(),
            "a refused name must say why, not fail silently"
        );
    });

    draw(cx);
}

// --- setting the language for a buffer (#127) ---------------------------------------

/// ⌘N then choosing PHP colours the buffer, without saving it first.
///
/// The whole of #127 as the user meets it: `Document::untitled()` has no path, so nothing
/// detects a language and there is no syntax colour, and before this the only way to get
/// one was to save the file. This goes through the real palette path — the same one the
/// status-bar cell and the command both open — rather than calling `set_language` directly,
/// because what was missing was the *route to it*, not the capability.
#[gpui::test]
async fn an_untitled_buffer_can_be_given_a_language_from_the_palette(cx: &mut TestAppContext) {
    install_theme(cx);
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.new_file(window, cx);
    });

    workspace.read_with(cx, |workspace, cx| {
        assert_eq!(
            workspace.active_language_for_test(cx),
            Some(elle_syntax::Language::PlainText),
            "a new buffer starts as plain text — that is the bug"
        );
    });

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_palette_for_test(PaletteMode::Languages, window, cx);
        // The id is the language's own `name()`, which is what the rows carry.
        workspace.confirm_palette_for_test("PHP", window, cx);
    });
    cx.run_until_parked();

    workspace.read_with(cx, |workspace, cx| {
        assert_eq!(
            workspace.active_language_for_test(cx),
            Some(elle_syntax::Language::Php),
            "choosing PHP must reach the document"
        );
        assert_eq!(
            workspace.tab_count_for_test(),
            1,
            "the buffer must still be the same one, not a saved file"
        );
    });

    draw(cx);
}

/// The language palette offers every language, and marks the one in effect.
#[gpui::test]
async fn the_language_palette_lists_every_language(cx: &mut TestAppContext) {
    install_theme(cx);
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    workspace.update_in(cx, |workspace, window, cx| {
        let document =
            Document::new(Some(std::path::PathBuf::from("User.php")), "<?php\n", true).unwrap();
        workspace.open_document_for_test(document, window, cx);
        workspace.toggle_palette_for_test(PaletteMode::Languages, window, cx);
    });

    workspace.read_with(cx, |workspace, cx| {
        let labels = workspace.palette_labels_for_test(cx);
        assert_eq!(
            labels.len(),
            elle_syntax::ALL_LANGUAGES.len(),
            "every language must be offered: {labels:?}"
        );
        // The current one is marked rather than hidden, so the list says what the buffer is
        // as well as what it could become.
        assert!(
            labels.iter().any(|label| label.starts_with("PHP") && label.contains('✓')),
            "the language in effect must be marked: {labels:?}"
        );
    });

    draw(cx);
}

/// Opening a folder actually starts a language server (#125).
///
/// The test that was missing, and whose absence is why #125 shipped. Everything else here
/// uses `open_folder_for_test`, which stops at the tree on purpose — so nothing in the suite
/// ever reached `start_lsp`, and the fact that ⌘O was its only caller went unseen.
///
/// Asserts on the *state*, not on a running process: whether a server is installed is a
/// property of the machine. What must hold everywhere is that opening a PHP project makes
/// the attempt and records the outcome, rather than leaving `Idle` — which is what "nothing
/// happened, and nothing was logged" looked like.
#[gpui::test]
async fn opening_a_folder_starts_a_language_server(cx: &mut TestAppContext) {
    use crate::lsp_session::LspState;

    install_theme(cx);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("composer.json"), "{}").unwrap();
    std::fs::write(dir.path().join("User.php"), "<?php\n").unwrap();

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    workspace.read_with(cx, |workspace, _cx| {
        assert_eq!(workspace.lsp_state_for_test(), LspState::Idle, "nothing attempted yet");
    });

    workspace.update_in(cx, |workspace, _window, cx| {
        workspace.open_folder_and_start_lsp_for_test(dir.path().to_path_buf(), cx);
    });

    workspace.read_with(cx, |workspace, _cx| {
        let state = workspace.lsp_state_for_test();
        assert_ne!(
            state,
            LspState::Idle,
            "opening a folder must attempt a server. Staying Idle is #125 exactly: no \
             attempt, no log line, and a popup that can never open"
        );
        // Either outcome is legitimate and depends on the machine; what matters is that a
        // decision was made and recorded.
        assert!(
            matches!(state, LspState::Starting | LspState::Unavailable),
            "unexpected state after opening a folder: {state:?}"
        );
    });

    draw(cx);
}

/// The list of completions occupies real pixels — the first geometry assertion in the suite.
///
/// # What this would have caught
///
/// The popup shipped with `flex_1` on its `uniform_list`, inside a wrapper whose height was
/// content-driven (`max_h`, no `h`). Flex-basis 0 contributes no content height, so the two
/// resolved to a popup exactly zero pixels tall — while every *state* test kept passing,
/// because selection, filtering and accept do not live in layout. The owner's report was
/// "funciona, mas não mostra nada": Enter inserted the member, the screen showed nothing.
///
/// `debug_selector` + `VisualTestContext::debug_bounds` is gpui 0.2.2's answer to exactly
/// this (#112): the test build records the element's laid-out bounds under a string key,
/// and a headless test can finally assert that a thing the user must see has a size. It
/// still cannot see colour or position-on-screen — this is a narrow window, not the golden
/// images #112 discusses — but zero-height is the failure mode that actually shipped.
#[gpui::test]
async fn the_completion_list_occupies_real_height(cx: &mut TestAppContext) {
    install_theme(cx);
    let registry = registry();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry, cx));
    open_php(&workspace, cx);

    workspace.update_in(cx, |workspace, window, cx| workspace.complete_for_test(window, cx));
    workspace.update(cx, |workspace, cx| {
        workspace.offer_completions_for_test(
            vec![lsp_item("getName"), lsp_item("getEmail"), lsp_item("greet")],
            cx,
        );
    });

    draw(cx);

    let bounds =
        cx.debug_bounds("completion-list").expect("the list must be in the rendered frame at all");
    assert_eq!(
        bounds.size.height,
        crate::completion::popup_height(3),
        "three rows must lay out at three rows' height — zero is the bug that shipped"
    );
    assert!(bounds.size.width > px(0.), "and it must have width: {bounds:?}");
}

/// Hovering a squiggle produces a card with the server's message; leaving clears it.
///
/// What the status-bar affordance kept failing at in live testing: the reason for an error
/// was reachable only with the *text cursor* inside the squiggle's bytes, which nobody
/// discovers. The card follows the mouse, which is the gesture every comparable editor
/// taught people. This test drives the same handlers the mouse events call.
#[gpui::test]
async fn hovering_a_diagnostic_shows_its_message_and_leaving_hides_it(cx: &mut TestAppContext) {
    use elle_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

    install_theme(cx);
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    // Absolute, because it has to survive the `uri_for` round trip: publishing keys the
    // diagnostics by URI, and a relative path has none.
    let path = std::path::PathBuf::from("/srv/app/User.php");
    workspace.update_in(cx, |workspace, window, cx| {
        let document =
            Document::new(Some(path.clone()), "<?php\n$x = $undefined;\n", true).unwrap();
        workspace.open_document_for_test(document, window, cx);
        // Line 1, chars 5..15: `$undefined`.
        workspace.publish_diagnostics_for_test(
            &path,
            &[Diagnostic {
                range: Range {
                    start: Position { line: 1, character: 5 },
                    end: Position { line: 1, character: 15 },
                },
                severity: Some(DiagnosticSeverity::ERROR),
                message: "Undefined variable '$undefined'.".into(),
                ..Default::default()
            }],
            cx,
        );
    });
    draw(cx);

    let editor = workspace
        .read_with(cx, |workspace, _cx| workspace.active_editor_for_test())
        .expect("a tab is open");

    // Driven at the offset level: the pixel conversion is the click tests' business, and
    // the fake text system's advances are fiction anyway (`fonts.rs`). Byte 14 sits inside
    // `$undefined` (bytes 11..21 of this fixture).
    editor.update(cx, |editor, cx| {
        editor.hover_at_for_test(14, 1, cx);
    });
    editor.read_with(cx, |editor, _cx| {
        let hover = editor.hover_diagnostic.as_ref().expect("hovering the squiggle makes a card");
        assert_eq!(hover.message.as_ref(), "Undefined variable '$undefined'.");
        assert_eq!(hover.row, 1);
    });

    // Same row, byte 7 — off the squiggle. The card must go.
    editor.update(cx, |editor, cx| {
        editor.hover_at_for_test(7, 1, cx);
    });
    editor.read_with(cx, |editor, _cx| {
        assert!(editor.hover_diagnostic.is_none(), "off the squiggle, no card");
    });

    // Back on, then the mouse leaves the row entirely.
    editor.update(cx, |editor, cx| {
        editor.hover_at_for_test(14, 1, cx);
    });
    editor.update(cx, |editor, cx| editor.hover_out_for_test(1, cx));
    editor.read_with(cx, |editor, _cx| {
        assert!(editor.hover_diagnostic.is_none(), "leaving the row clears the card");
    });

    draw(cx);
}

/// The file tree lays out at a real height.
///
/// Guards the wrapper the root context menu added around the rows: the list sizes itself
/// with `flex_1`, and `flex_1` inside a parent that is not a flex column resolves to zero
/// height — the resolution that shipped the invisible completion popup. Unlike the
/// truncation bug (ink escaping a correctly-sized box, which bounds cannot see), a
/// collapsed container is exactly what `debug_bounds` measures.
#[gpui::test]
async fn the_file_tree_occupies_real_height(cx: &mut TestAppContext) {
    install_theme(cx);
    let dir = project();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    workspace.update_in(cx, |workspace, _window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
    });
    draw(cx);

    let bounds = cx.debug_bounds("file-tree-list").expect("the tree is in the frame");
    assert!(
        bounds.size.height >= Metrics::ROW_HEIGHT,
        "a tree with rows must be at least one row tall, got {:?}",
        bounds.size.height
    );
}

/// Renaming a file drags its open tab along — and the language follows the new name.
///
/// The hole this closes is save-resurrection, the same one `close_tabs_under` closed for
/// delete: a tab keeps the path it was opened with, so renaming `notas.txt` to `User.php`
/// with the tab open left ⌘S pointed at `notas.txt` — the next save would quietly recreate
/// the file the user had just renamed away. The tab now follows the file, and because the
/// retarget goes through `Document::set_path`, the buffer starts highlighting as PHP too,
/// the same re-detection save-as performs.
#[gpui::test]
async fn renaming_an_open_file_retargets_its_tab(cx: &mut TestAppContext) {
    use crate::context_menu::MenuAction;

    install_theme(cx);
    let dir = project();
    let old_path = dir.path().join("notas.txt");
    std::fs::write(&old_path, "<?php\nclass A {}\n").unwrap();

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        let document = Document::new(Some(old_path.clone()), "<?php\nclass A {}\n", true).unwrap();
        workspace.open_document_for_test(document, window, cx);
        // Row order: `app` dir, then files alphabetically — `artisan`, `notas.txt`.
        let row = workspace
            .tree_names_for_test()
            .iter()
            .position(|name| name == "notas.txt")
            .expect("the fixture file is in the tree");
        workspace.right_click_tree_row_for_test(row, window, cx);
        workspace.pick_menu_action_for_test(MenuAction::Rename, window, cx);
        workspace.confirm_name_for_test("Renamed.php", window, cx);
    });
    cx.run_until_parked();

    assert!(dir.path().join("Renamed.php").exists());
    assert!(!old_path.exists());

    let editor = workspace
        .read_with(cx, |workspace, _cx| workspace.active_editor_for_test())
        .expect("the tab survived the rename");
    editor.read_with(cx, |editor, _cx| {
        assert_eq!(
            editor.document.path.as_deref().and_then(|p| p.file_name()),
            Some(std::ffi::OsStr::new("Renamed.php")),
            "the document must follow the file, or the next save resurrects the old name"
        );
        assert_eq!(
            editor.document.language(),
            elle_syntax::Language::Php,
            "a rename that changes the extension re-detects the language, like save-as"
        );
    });

    draw(cx);
}

/// A ⌘-clicked `file.php:42` from the terminal opens the file at the line (#70).
///
/// Drives the workspace half through the same resolver the terminal's event reaches. The
/// text-scanning half (`link_at`) has its own unit tests in `elle-terminal`; what this
/// pins is the part the issue said was blocked — landing on the *line* — and the honesty
/// rule: a shape that resolves to nothing opens nothing, silently.
#[gpui::test]
async fn a_terminal_path_click_opens_the_file_at_its_line(cx: &mut TestAppContext) {
    install_theme(cx);
    let dir = project();
    std::fs::write(dir.path().join("app/User.php"), "<?php\nline2\nline3\nline4\nline5\nline6\n")
        .unwrap();

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    workspace.update_in(cx, |workspace, _window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
    });

    // Relative, exactly as a stack trace prints it: resolved against the project root.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_terminal_link_for_test(
            std::path::PathBuf::from("app/User.php"),
            Some(5),
            window,
            cx,
        );
    });
    cx.run_until_parked();

    workspace.read_with(cx, |workspace, cx| {
        assert_eq!(workspace.tab_count_for_test(), 1, "the file must open");
        assert_eq!(
            workspace.cursor_row_for_test(cx),
            Some(4),
            "line 5 in the trace is row 4 in the editor — one-based to zero-based"
        );
    });

    // A path-shaped token that names nothing opens nothing, and says nothing: the
    // detector matches shapes, and prose with a slash in it qualifies.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_terminal_link_for_test(
            std::path::PathBuf::from("does/not/exist.php"),
            Some(1),
            window,
            cx,
        );
    });
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, _cx| {
        assert_eq!(workspace.tab_count_for_test(), 1, "nothing new opens");
        assert!(workspace.status_for_test().is_none(), "and nobody is blamed for it");
    });

    draw(cx);
}

/// A definition jump lands on the identifier, through both doors, in UTF-16-correct columns.
///
/// The jump used to land at column 0 with a comment explaining that converting the
/// server's UTF-16 character needed a buffer nobody had yet — true where it was written,
/// and exactly why the conversion moved to where the buffer exists (`Target::resolve`).
/// The fixture is accented on purpose: `$ação`'s `ç`/`ã` are one UTF-16 unit but two
/// UTF-8 bytes, so an unconverted column lands mid-identifier on the code this editor
/// is for, and an ASCII fixture would pass with the conversion deleted.
#[gpui::test]
async fn a_definition_jump_lands_on_the_identifier(cx: &mut TestAppContext) {
    install_theme(cx);
    let dir = project();
    // UTF-16 character 11 on line 1 is past `$ação = $b` — byte column 13.
    let accented = "<?php\n$ação = $bem;\nmais\n";
    let on_disk = dir.path().join("app/Acentos.php");
    std::fs::write(&on_disk, accented).unwrap();

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    // Door one: the file is already open — the `$this->name` same-file case.
    let open_path = std::path::PathBuf::from("/srv/app/Aberto.php");
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        let document = Document::new(Some(open_path.clone()), accented, true).unwrap();
        workspace.open_document_for_test(document, window, cx);
        workspace.open_path_at_lsp_for_test(open_path.clone(), 1, 11, window, cx);
    });
    workspace.read_with(cx, |workspace, cx| {
        assert_eq!(
            workspace.cursor_point_for_test(cx),
            Some(elle_text::Point::new(1, 13)),
            "UTF-16 character 11 is byte column 13 on this line — column 0 or 11 is the bug"
        );
    });

    // Door two: the file loads first, and the conversion waits for it.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_path_at_lsp_for_test(on_disk.clone(), 1, 11, window, cx);
    });
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, cx| {
        assert_eq!(workspace.tab_count_for_test(), 2, "the second door opened a new tab");
        assert_eq!(
            workspace.cursor_point_for_test(cx),
            Some(elle_text::Point::new(1, 13)),
            "the loaded door must convert too, after the text exists"
        );
    });

    draw(cx);
}

/// ⌘, opens the settings panel, its steppers apply live, and Escape closes it (#100).
///
/// The live-apply is the issue's constraint that matters: "changes apply live. theme.toggle
/// and ⌘+/⌘− already do; the panel should feel the same." The stepper goes through
/// `update_settings`, which re-resolves `Fonts` — so the assertion is on the *applied*
/// global, not on a value the panel merely displays. With no `LiveSettings` installed (the
/// malformed-file launch, and every headless test) the change still applies to the session
/// and nothing is written, which is #60's rule holding for the panel.
#[gpui::test]
async fn the_settings_panel_applies_changes_live(cx: &mut TestAppContext) {
    install_theme(cx);
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_settings_panel_for_test(window, cx);
    });
    let panel = workspace
        .read_with(cx, |workspace, _cx| workspace.settings_panel_for_test())
        .expect("⌘, opens the panel");

    let before = cx.update(|_window, cx| f32::from(Fonts::get(cx).size));
    panel.update(cx, |panel, cx| panel.step_for_test("editor", 2.0, cx));
    let after = cx.update(|_window, cx| f32::from(Fonts::get(cx).size));
    assert_eq!(after, before + 2.0, "the stepper must reach the applied fonts, not a label");

    // Second ⌘, closes, like the palette.
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_settings_panel_for_test(window, cx);
    });
    workspace.read_with(cx, |workspace, _cx| {
        assert!(workspace.settings_panel_for_test().is_none(), "a second press dismisses");
    });

    draw(cx);
}

/// The theme picker cycles through every selectable theme and applies each.
#[gpui::test]
async fn the_theme_picker_cycles_and_applies(cx: &mut TestAppContext) {
    install_theme(cx);
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_settings_panel_for_test(window, cx);
    });
    let panel = workspace
        .read_with(cx, |workspace, _cx| workspace.settings_panel_for_test())
        .expect("panel open");

    // A full lap must return to where it started — the wrap is what makes ‹ from the
    // first theme reach the last, and a sticky picker reads as broken.
    let start = cx.update(|_window, cx| cx.theme().background);
    let lap = crate::theme::ThemeVariant::ALL.len();
    for _ in 0..lap {
        panel.update(cx, |panel, cx| panel.cycle_theme_for_test(true, cx));
    }
    let back = cx.update(|_window, cx| cx.theme().background);
    assert_eq!(start, back, "a full forward lap lands home");

    draw(cx);
}

/// Editing a model offers its own columns, provenance in the detail (#22).
///
/// The first consumer of #21's index, driven end to end: a real project, a really-built
/// index, the real popup. A non-model file must get nothing — the source's claim is
/// scoped to where it holds, which is the routes source's rule inherited.
#[gpui::test]
async fn a_model_offers_its_columns_with_provenance(cx: &mut TestAppContext) {
    // `index_path` derives from HOME, and this test reads it on a background task while
    // the `file_cache` tests may be *setting* HOME — hold their lock for the duration.
    let _home = crate::file_cache::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    install_theme(cx);
    let dir = project();
    std::fs::create_dir_all(dir.path().join("app/Models")).unwrap();
    std::fs::create_dir_all(dir.path().join("database/migrations")).unwrap();
    let model_path = dir.path().join("app/Models/User.php");
    std::fs::write(
        &model_path,
        "<?php\nclass User extends Model {\n    protected $casts = ['is_admin' => 'boolean'];\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("database/migrations/2026_01_01_create_users.php"),
        "<?php\nreturn new class extends Migration {\n  public function up(): void {\n    Schema::create('users', function (Blueprint $table) {\n      $table->id();\n      $table->string('name');\n    });\n  }\n};\n",
    )
    .unwrap();

    // Build the index the way the folder-open task does, synchronously here — against
    // the CANONICAL root, because that is what the workspace's tree hands the background
    // build. The raw tempdir spelling produced a different index file entirely: the
    // /var-vs-/private/var trap's fifth appearance in this codebase, this time between
    // a test and the code it tests.
    let canonical = dir.path().canonicalize().unwrap();
    let index_path = crate::file_cache::index_path(&canonical).expect("index path");
    {
        let (index, _) = elle_index::Index::open(&index_path).expect("index opens");
        elle_index::laravel::build(
            index.connection(),
            &canonical,
            &elle_workspace::CancelFlag::default(),
        )
        .expect("index builds");
    }

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        let text = std::fs::read_to_string(&model_path).unwrap();
        let document = Document::new(Some(model_path.clone()), &text, true).unwrap();
        workspace.open_document_for_test(document, window, cx);
    });
    // A frame first: the popup anchors at a measured cursor origin, and measurement
    // happens in prepaint — invoking before any frame returns early with no popup.
    draw(cx);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.complete_for_test(window, cx);
    });
    cx.run_until_parked();

    let popup = workspace
        .read_with(cx, |workspace, _cx| workspace.completion_for_test())
        .expect("popup open");
    popup.read_with(cx, |popup, _cx| {
        let items = popup.visible_items();
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_ref()).collect();
        assert!(labels.contains(&"id"), "migration columns arrive: {labels:?}");
        assert!(labels.contains(&"is_admin"), "cast columns arrive: {labels:?}");
        let is_admin = items.iter().find(|item| item.label.as_ref() == "is_admin").unwrap();
        assert_eq!(
            is_admin.detail.as_ref().map(|d| d.as_ref()),
            Some("boolean · cast"),
            "the provenance is the detail — a cast's word is a different promise"
        );
        assert!(
            items.iter().any(|item| matches!(item.source, CompletionSource::LaravelColumn)),
            "the source is modelled, not inferred"
        );
    });

    draw(cx);
}

/// Editing a model also offers its relationships, kind and target in the detail (#22).
///
/// Same door as columns — the index already stores `(method, kind, target)`; this test
/// pins that they reach the popup as items in their own right, with their own source, so
/// a `posts` from a `hasMany` cannot masquerade as a column named `posts`.
#[gpui::test]
async fn a_model_offers_its_relationships_with_kind_and_target(cx: &mut TestAppContext) {
    // Same HOME race as the columns test above; same lock.
    let _home = crate::file_cache::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    install_theme(cx);
    let dir = project();
    std::fs::create_dir_all(dir.path().join("app/Models")).unwrap();
    let model_path = dir.path().join("app/Models/User.php");
    std::fs::write(
        &model_path,
        "<?php\nclass User extends Model {\n    public function posts() { return $this->hasMany(Post::class); }\n}\n",
    )
    .unwrap();

    // Canonical root, as ever — the /var-vs-/private/var trap (fifth appearance is in
    // the sibling test's comment; this test inherits the lesson, not the price).
    let canonical = dir.path().canonicalize().unwrap();
    let index_path = crate::file_cache::index_path(&canonical).expect("index path");
    {
        let (index, _) = elle_index::Index::open(&index_path).expect("index opens");
        elle_index::laravel::build(
            index.connection(),
            &canonical,
            &elle_workspace::CancelFlag::default(),
        )
        .expect("index builds");
    }

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        let text = std::fs::read_to_string(&model_path).unwrap();
        let document = Document::new(Some(model_path.clone()), &text, true).unwrap();
        workspace.open_document_for_test(document, window, cx);
    });
    draw(cx);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.complete_for_test(window, cx);
    });
    cx.run_until_parked();

    let popup = workspace
        .read_with(cx, |workspace, _cx| workspace.completion_for_test())
        .expect("popup open");
    popup.read_with(cx, |popup, _cx| {
        let items = popup.visible_items();
        let posts = items
            .iter()
            .find(|item| item.label.as_ref() == "posts")
            .expect("the relationship arrives as an item");
        assert_eq!(
            posts.detail.as_ref().map(|d| d.as_ref()),
            Some("hasMany · Post"),
            "kind and target are the detail — what the method body actually says"
        );
        assert!(
            matches!(posts.source, CompletionSource::LaravelRelation),
            "a relationship is its own kind of claim, not a column"
        );
    });

    draw(cx);
}

/// `User::where('…')` in any PHP file offers `users`' columns inside the literal (#22).
///
/// The context is the tree's word (`column_context_at`), not a path heuristic: the file
/// here is a controller, not a model, and the class is read off the chain root. Outside
/// a recognised context the same file offers no columns — that is the sibling claim the
/// columns-in-a-model test already scopes, inherited here by construction.
#[gpui::test]
async fn a_where_call_offers_the_models_columns_inside_the_literal(cx: &mut TestAppContext) {
    // Same HOME race as the other index-backed popup tests; same lock.
    let _home = crate::file_cache::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    install_theme(cx);
    let dir = project();
    std::fs::create_dir_all(dir.path().join("app/Models")).unwrap();
    std::fs::create_dir_all(dir.path().join("app/Http/Controllers")).unwrap();
    std::fs::create_dir_all(dir.path().join("database/migrations")).unwrap();
    std::fs::write(
        dir.path().join("app/Models/User.php"),
        "<?php\nclass User extends Model {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("database/migrations/2026_01_01_create_users.php"),
        "<?php\nreturn new class extends Migration {\n  public function up(): void {\n    Schema::create('users', function (Blueprint $table) {\n      $table->id();\n      $table->string('email');\n    });\n  }\n};\n",
    )
    .unwrap();
    let controller_path = dir.path().join("app/Http/Controllers/UserController.php");
    let controller = "<?php\nclass UserController extends Controller {\n    public function index() { return User::where(''); }\n}\n";
    std::fs::write(&controller_path, controller).unwrap();

    let canonical = dir.path().canonicalize().unwrap();
    let index_path = crate::file_cache::index_path(&canonical).expect("index path");
    {
        let (index, _) = elle_index::Index::open(&index_path).expect("index opens");
        elle_index::laravel::build(
            index.connection(),
            &canonical,
            &elle_workspace::CancelFlag::default(),
        )
        .expect("index builds");
    }

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        let document = Document::new(Some(controller_path.clone()), controller, true).unwrap();
        workspace.open_document_for_test(document, window, cx);
    });
    draw(cx);

    // The caret between the quotes of `where('')` — the moment the completion is wanted.
    let inside = controller.find("('')").unwrap() + 2;
    let editor = workspace
        .read_with(cx, |workspace, _cx| workspace.active_editor_for_test())
        .expect("a file is open");
    editor.update(cx, |editor, _cx| editor.document.select_range_for_test(inside..inside));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.complete_for_test(window, cx);
    });
    cx.run_until_parked();

    let popup = workspace
        .read_with(cx, |workspace, _cx| workspace.completion_for_test())
        .expect("popup open");
    popup.read_with(cx, |popup, _cx| {
        let items = popup.visible_items();
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_ref()).collect();
        assert!(labels.contains(&"email"), "the class's columns arrive: {labels:?}");
        let email = items.iter().find(|item| item.label.as_ref() == "email").unwrap();
        assert!(
            matches!(email.source, CompletionSource::LaravelColumn),
            "a column in a where() is the same kind of claim as a column in the model"
        );
    });

    draw(cx);
}

/// `User::with('…')` offers the model's relationships, not its columns (#22).
///
/// The scanner says which list the literal wants (`Argument::Relation`); offering
/// columns there would be a wrong answer wearing a confident badge, and offering both
/// would bury the two real relations under thirty columns.
#[gpui::test]
async fn a_with_call_offers_relationships_not_columns(cx: &mut TestAppContext) {
    // Same HOME race as the other index-backed popup tests; same lock.
    let _home = crate::file_cache::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    install_theme(cx);
    let dir = project();
    std::fs::create_dir_all(dir.path().join("app/Models")).unwrap();
    std::fs::create_dir_all(dir.path().join("app/Http/Controllers")).unwrap();
    std::fs::create_dir_all(dir.path().join("database/migrations")).unwrap();
    std::fs::write(
        dir.path().join("app/Models/User.php"),
        "<?php\nclass User extends Model {\n    public function posts() { return $this->hasMany(Post::class); }\n}\n",
    )
    .unwrap();
    // A migration too, so "columns exist and are NOT offered" is a real assertion
    // rather than an empty set passing by accident.
    std::fs::write(
        dir.path().join("database/migrations/2026_01_01_create_users.php"),
        "<?php\nreturn new class extends Migration {\n  public function up(): void {\n    Schema::create('users', function (Blueprint $table) {\n      $table->string('email');\n    });\n  }\n};\n",
    )
    .unwrap();
    let controller_path = dir.path().join("app/Http/Controllers/UserController.php");
    let controller = "<?php\nclass UserController extends Controller {\n    public function index() { return User::with(''); }\n}\n";
    std::fs::write(&controller_path, controller).unwrap();

    let canonical = dir.path().canonicalize().unwrap();
    let index_path = crate::file_cache::index_path(&canonical).expect("index path");
    {
        let (index, _) = elle_index::Index::open(&index_path).expect("index opens");
        elle_index::laravel::build(
            index.connection(),
            &canonical,
            &elle_workspace::CancelFlag::default(),
        )
        .expect("index builds");
    }

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        let document = Document::new(Some(controller_path.clone()), controller, true).unwrap();
        workspace.open_document_for_test(document, window, cx);
    });
    draw(cx);

    let inside = controller.find("('')").unwrap() + 2;
    let editor = workspace
        .read_with(cx, |workspace, _cx| workspace.active_editor_for_test())
        .expect("a file is open");
    editor.update(cx, |editor, _cx| editor.document.select_range_for_test(inside..inside));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.complete_for_test(window, cx);
    });
    cx.run_until_parked();

    let popup = workspace
        .read_with(cx, |workspace, _cx| workspace.completion_for_test())
        .expect("popup open");
    popup.read_with(cx, |popup, _cx| {
        let items = popup.visible_items();
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_ref()).collect();
        assert!(labels.contains(&"posts"), "the relationship arrives: {labels:?}");
        assert!(
            !labels.contains(&"email"),
            "columns are not an answer to with(): {labels:?}"
        );
        let posts = items.iter().find(|item| item.label.as_ref() == "posts").unwrap();
        assert!(matches!(posts.source, CompletionSource::LaravelRelation));
    });

    draw(cx);
}

/// Typing `User::ac` offers the model's scopes by call name (#22).
///
/// `scopeActive` is *called* as `active()` — Intelephense offers the declared method
/// name, which is exactly the one the user must not type there. The index stores the
/// call name and the scanner finds the `Class::partial` shape mid-typing, ERROR node
/// and all.
#[gpui::test]
async fn a_static_prefix_offers_the_models_scopes(cx: &mut TestAppContext) {
    // Same HOME race as the other index-backed popup tests; same lock.
    let _home = crate::file_cache::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    install_theme(cx);
    let dir = project();
    std::fs::create_dir_all(dir.path().join("app/Models")).unwrap();
    std::fs::create_dir_all(dir.path().join("app/Http/Controllers")).unwrap();
    std::fs::write(
        dir.path().join("app/Models/User.php"),
        "<?php\nclass User extends Model {\n    public function scopeActive($query) { return $query; }\n}\n",
    )
    .unwrap();
    let controller_path = dir.path().join("app/Http/Controllers/UserController.php");
    let controller = "<?php\nclass UserController extends Controller {\n    public function index() { return User::ac; }\n}\n";
    std::fs::write(&controller_path, controller).unwrap();

    let canonical = dir.path().canonicalize().unwrap();
    let index_path = crate::file_cache::index_path(&canonical).expect("index path");
    {
        let (index, _) = elle_index::Index::open(&index_path).expect("index opens");
        elle_index::laravel::build(
            index.connection(),
            &canonical,
            &elle_workspace::CancelFlag::default(),
        )
        .expect("index builds");
    }

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        let document = Document::new(Some(controller_path.clone()), controller, true).unwrap();
        workspace.open_document_for_test(document, window, cx);
    });
    draw(cx);

    // The caret at the end of `User::ac` — mid-typing, the only moment that matters.
    let after = controller.find("User::ac").unwrap() + "User::ac".len();
    let editor = workspace
        .read_with(cx, |workspace, _cx| workspace.active_editor_for_test())
        .expect("a file is open");
    editor.update(cx, |editor, _cx| editor.document.select_range_for_test(after..after));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.complete_for_test(window, cx);
    });
    cx.run_until_parked();

    let popup = workspace
        .read_with(cx, |workspace, _cx| workspace.completion_for_test())
        .expect("popup open");
    popup.read_with(cx, |popup, _cx| {
        let items = popup.visible_items();
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_ref()).collect();
        assert!(labels.contains(&"active"), "the call name arrives: {labels:?}");
        let active = items.iter().find(|item| item.label.as_ref() == "active").unwrap();
        assert!(
            matches!(active.source, CompletionSource::LaravelScope),
            "a scope is its own kind of claim"
        );
    });

    draw(cx);
}

/// Confirming an artisan palette row opens the terminal and types — never runs (#23).
///
/// What is machine-checkable is that the panel opens with a session for the command to
/// land in. That the typed line reaches the PTY without a newline is carried by
/// `artisan::command_line`'s unit test plus the send path the terminal already pins;
/// that the *shell* shows it is a rendering claim this suite cannot make (#112).
#[gpui::test]
async fn confirming_an_artisan_row_opens_the_terminal_to_type_into(cx: &mut TestAppContext) {
    install_theme(cx);
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_palette_for_test(PaletteMode::Artisan, window, cx);
        workspace.confirm_palette_for_test("migrate", window, cx);
    });

    workspace.read_with(cx, |workspace, cx| {
        let terminal = workspace.terminal_for_test().expect("the terminal panel opened");
        assert!(
            terminal.read(cx).session_count() > 0,
            "a session exists for the command to land in"
        );
    });

    draw(cx);
}

/// The workspace-symbol palette shows what the server sent, not a local re-filter (#19).
///
/// The server is the matcher — Intelephense matches camel humps and mid-word fragments a
/// subsequence scan would reject, so filtering its answer again would make real hits
/// vanish as the user types. The fixture makes that falsifiable: the typed query shares
/// no subsequence with the row, and the row must survive.
#[gpui::test]
async fn the_workspace_symbol_palette_trusts_the_server_not_a_local_filter(
    cx: &mut TestAppContext,
) {
    install_theme(cx);
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_palette_for_test(crate::palette::PaletteMode::WorkspaceSymbols, window, cx);
    });
    let palette = workspace
        .read_with(cx, |workspace, _cx| workspace.palette_for_test())
        .expect("palette open");

    // The user types something the server would fuzzy-match but a subsequence scan
    // rejects outright…
    palette.update(cx, |palette, cx| palette.type_for_test("zzz", cx));
    // …and the server answers with a hit anyway (fed directly — no server in tests).
    palette.update(cx, |palette, cx| {
        palette.set_items(vec![("UserController::handle".into(), "x".into())], cx);
    });

    let labels =
        workspace.read_with(cx, |workspace, cx| workspace.palette_labels_for_test(cx));
    assert_eq!(
        labels,
        ["UserController::handle"],
        "the server's hit must survive the local query"
    );

    draw(cx);
}

/// The branch palette lists the repo's branches and confirming switches (#64).
///
/// Through a real repository — the dirty-tree refusal is the crate's and is proven
/// there; what this pins is the wiring: branches arrive labelled, the current one
/// marked, and confirm really moves HEAD.
#[gpui::test]
async fn the_branch_palette_lists_and_switches(cx: &mut TestAppContext) {
    install_theme(cx);
    let dir = project();
    let run = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .unwrap()
                .status
                .success(),
            "git {args:?}"
        );
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "t@t"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(dir.path().join("a.php"), "<?php\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "init"]);
    run(&["branch", "-M", "main"]);
    run(&["branch", "feature"]);

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        workspace.toggle_palette_for_test(crate::palette::PaletteMode::Branches, window, cx);
    });
    cx.run_until_parked();

    let labels = workspace.read_with(cx, |workspace, cx| workspace.palette_labels_for_test(cx));
    assert!(labels.contains(&"main  ✓".to_string()), "current marked: {labels:?}");
    assert!(labels.contains(&"feature".to_string()));

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.confirm_palette_for_test("feature", window, cx);
    });
    cx.run_until_parked();

    let head = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["branch", "--show-current"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "feature", "HEAD really moved");

    draw(cx);
}

/// The Database panel reads the project's sqlite schema when shown (#65).
///
/// What this pins: showing the panel is what loads (the baseline assert says the state
/// starts empty), the read resolves through `.env`, and the tables come back in stable
/// order. The stronger claim — that the REAL folder-open path never touches the
/// database — is not provable from here: `open_folder_for_test` deliberately stops at
/// the tree (the #125 seam), so "never at startup" rests on `adopt_tree` having exactly
/// two `load_db_schema` call sites (activity click, focus-with-panel-up), which is a
/// review property, stated here so nobody mistakes this test for proving it.
#[gpui::test]
async fn the_database_panel_reads_the_schema_on_entry(cx: &mut TestAppContext) {
    install_theme(cx);
    let dir = project();
    std::fs::write(dir.path().join(".env"), "DB_CONNECTION=sqlite\n").unwrap();
    std::fs::create_dir_all(dir.path().join("database")).unwrap();
    let db = dir.path().join("database/database.sqlite");
    rusqlite_fixture(&db);

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    workspace.update_in(cx, |workspace, _window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
    });
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, _cx| {
        assert!(
            workspace.db_schema_for_test().is_none(),
            "the state starts empty — showing the panel is what loads"
        );
    });

    workspace.update(cx, |workspace, cx| workspace.show_database_panel_for_test(cx));
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, _cx| {
        let tables = workspace.db_schema_for_test().expect("loaded").expect("read ok");
        assert_eq!(tables, ["posts", "users"], "alphabetical, from the real file");
    });

    draw(cx);
}

/// Writes a two-table sqlite fixture — a user database, not an index.
fn rusqlite_fixture(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL);
         CREATE TABLE posts (id INTEGER PRIMARY KEY);",
    )
    .unwrap();
}

/// Inside `wire:click="…"` the component's actions arrive; `wire:model` its properties (#24).
///
/// The class resolves by convention from the view path; the scanner decides which list
/// the attribute wants. The negative is real: `sortBy` (an action) must not appear in
/// the `wire:model` list, and `search` (a property) not in `wire:click`'s.
#[gpui::test]
async fn a_wire_attribute_offers_the_components_surface(cx: &mut TestAppContext) {
    install_theme(cx);
    let dir = project();
    std::fs::create_dir_all(dir.path().join("app/Livewire")).unwrap();
    std::fs::create_dir_all(dir.path().join("resources/views/livewire")).unwrap();
    std::fs::write(
        dir.path().join("app/Livewire/UserTable.php"),
        "<?php\nnamespace App\\Livewire;\nuse Livewire\\Component;\nclass UserTable extends Component {\n    public string $search = '';\n    public function render() {}\n    public function sortBy($c) {}\n}\n",
    )
    .unwrap();
    let view_path = dir.path().join("resources/views/livewire/user-table.blade.php");
    let view = "<div>\n    <button wire:click=\"\">Sort</button>\n</div>\n";
    std::fs::write(&view_path, view).unwrap();

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        let document = Document::new(Some(view_path.clone()), view, true).unwrap();
        workspace.open_document_for_test(document, window, cx);
    });
    draw(cx);

    let inside = view.find("wire:click=\"").unwrap() + "wire:click=\"".len();
    let editor = workspace
        .read_with(cx, |workspace, _cx| workspace.active_editor_for_test())
        .expect("a file is open");
    editor.update(cx, |editor, _cx| editor.document.select_range_for_test(inside..inside));

    workspace.update_in(cx, |workspace, window, cx| workspace.complete_for_test(window, cx));
    cx.run_until_parked();

    let popup = workspace
        .read_with(cx, |workspace, _cx| workspace.completion_for_test())
        .expect("popup open");
    popup.read_with(cx, |popup, _cx| {
        let items = popup.visible_items();
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_ref()).collect();
        assert!(labels.contains(&"sortBy"), "the action arrives: {labels:?}");
        assert!(!labels.contains(&"search"), "a property is not a click target: {labels:?}");
        assert!(!labels.contains(&"render"), "lifecycle stays the framework's: {labels:?}");
        let sort = items.iter().find(|item| item.label.as_ref() == "sortBy").unwrap();
        assert!(matches!(sort.source, CompletionSource::Livewire));
        assert_eq!(sort.detail.as_ref().map(|d| d.as_ref()), Some("action · UserTable"));
    });

    draw(cx);
}

/// Folding hides a block's body from the row map and the safety rules hold (#82).
///
/// The row↔line map is the piece the issue warned corrupts edits if wrong; rows here
/// are read through the same map the render callback uses, so what this asserts is what
/// the gutter numbers, the mouse handlers and the painted content are all built from.
#[gpui::test]
async fn folding_maps_rows_past_the_hidden_body_and_reveals_on_entry(cx: &mut TestAppContext) {
    install_theme(cx);
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    let text = "class A {\n    fn b() {\n        body;\n        more;\n    }\n    fn c() {}\n}\n";
    workspace.update_in(cx, |workspace, window, cx| {
        let document = Document::new(Some(std::path::PathBuf::from("A.php")), text, true).unwrap();
        workspace.open_document_for_test(document, window, cx);
    });
    let editor = workspace
        .read_with(cx, |workspace, _cx| workspace.active_editor_for_test())
        .expect("a file is open");

    // Cursor on `fn b`, fold its block.
    editor.update(cx, |editor, cx| {
        let offset = text.find("fn b").unwrap();
        editor.document.move_to(offset, false);
        editor.fold_block_at_cursor(cx);
        assert_eq!(
            editor.visible_lines_for_test(),
            [0, 1, 4, 5, 6, 7],
            "rows skip the hidden body — the closing brace shares the header's indent \
             and stays visible, which is how indent folding reads everywhere"
        );
    });
    draw(cx);

    // Entering the fold reveals it — an invisible caret is an edit about to land
    // somewhere invisible.
    editor.update(cx, |editor, cx| {
        let offset = text.find("body").unwrap();
        editor.document.move_to(offset, false);
        let _ = cx;
    });
    draw(cx);
    editor.update(cx, |editor, _cx| {
        assert_eq!(
            editor.visible_lines_for_test().len(),
            8,
            "the render pass revealed the fold the cursor entered"
        );
    });
}

/// An edit that changes the line count clears every fold — the survival rule (#82).
#[gpui::test]
async fn a_line_count_change_invalidates_folds_at_the_render_funnel(cx: &mut TestAppContext) {
    install_theme(cx);
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    let text = "fn a() {\n    one;\n    two;\n}\nfn z() {}\n";
    workspace.update_in(cx, |workspace, window, cx| {
        let document = Document::new(Some(std::path::PathBuf::from("A.php")), text, true).unwrap();
        workspace.open_document_for_test(document, window, cx);
    });
    let editor = workspace
        .read_with(cx, |workspace, _cx| workspace.active_editor_for_test())
        .expect("a file is open");

    editor.update(cx, |editor, cx| {
        editor.document.move_to(text.find("fn z").unwrap(), false);
        editor.fold_all(cx);
        assert_eq!(editor.visible_lines_for_test(), [0, 3, 4, 5], "the body folded");
    });
    draw(cx);

    // A newline at the cursor changes the line count; the ranges name lines that moved.
    editor.update(cx, |editor, _cx| {
        let at = editor.document.selection.head;
        editor.document.buffer.insert(at, "\n");
    });
    draw(cx);
    editor.update(cx, |editor, _cx| {
        assert_eq!(
            editor.visible_lines_for_test().len(),
            7,
            "all folds cleared rather than pointing at moved lines"
        );
    });
}

/// The rename prompt confirms the typed name, and an empty name is not a rename (#19).
#[gpui::test]
async fn the_rename_prompt_confirms_its_query_not_a_row(cx: &mut TestAppContext) {
    install_theme(cx);
    let confirmed = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));

    let (palette, subscription) = cx.update(|cx| {
        use gpui::AppContext as _;
        let palette =
            cx.new(|cx| crate::palette::Palette::new(crate::palette::PaletteMode::Rename, Vec::new(), cx));
        let sink = confirmed.clone();
        let subscription = cx.subscribe(&palette, move |_palette, event, _cx| {
            if let crate::palette::PaletteEvent::Confirmed(name) = event {
                sink.borrow_mut().push(name.clone());
            }
        });
        (palette, subscription)
    });

    palette.update(cx, |palette, cx| {
        // Empty first: confirming nothing must emit nothing — Escape is how you decline.
        palette.confirm_for_test(cx);
        palette.preset_query("old_name", cx);
        palette.type_for_test("_2", cx);
        palette.confirm_for_test(cx);
    });

    assert_eq!(*confirmed.borrow(), ["old_name_2"], "the typed name is the answer");
    drop(subscription);
}

/// A rename's WorkspaceEdit touches open buffers and closed files — all or none (#19).
///
/// The open buffer takes its edits as one undo step and goes dirty (the user saves);
/// the closed file is rewritten on disk. And the all-or-nothing rule is falsifiable:
/// an edit containing a file operation is refused whole, with both targets untouched.
#[gpui::test]
async fn a_workspace_edit_spans_open_buffers_and_closed_files_or_touches_nothing(
    cx: &mut TestAppContext,
) {
    use elle_lsp::lsp_types as lt;
    install_theme(cx);
    let dir = project();
    let open_path = dir.path().join("a.php");
    let closed_path = dir.path().join("b.php");
    std::fs::write(&open_path, "<?php\n$old = 1;\n").unwrap();
    std::fs::write(&closed_path, "<?php\n$old = 2;\n").unwrap();

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    workspace.update_in(cx, |workspace, window, cx| {
        let text = std::fs::read_to_string(&open_path).unwrap();
        let document = Document::new(Some(open_path.clone()), &text, true).unwrap();
        workspace.open_document_for_test(document, window, cx);
    });

    let edit_for = |path: &std::path::Path| {
        (
            elle_lsp::path_to_uri(path).unwrap(),
            vec![lt::TextEdit {
                range: lt::Range {
                    start: lt::Position { line: 1, character: 1 },
                    end: lt::Position { line: 1, character: 4 },
                },
                new_text: "renamed".into(),
            }],
        )
    };
    let edit = lt::WorkspaceEdit {
        changes: Some([edit_for(&open_path), edit_for(&closed_path)].into_iter().collect()),
        document_changes: None,
        change_annotations: None,
    };

    let files = workspace
        .update(cx, |workspace, cx| workspace.apply_workspace_edit_for_test(edit, cx))
        .expect("the edit applies");
    assert_eq!(files, 2);

    let editor = workspace
        .read_with(cx, |workspace, _cx| workspace.active_editor_for_test())
        .expect("a file is open");
    editor.update(cx, |editor, _cx| {
        assert_eq!(editor.document.buffer.text(), "<?php\n$renamed = 1;\n");
        editor.document.undo();
        assert_eq!(
            editor.document.buffer.text(),
            "<?php\n$old = 1;\n",
            "one undo restores the buffer"
        );
    });
    assert_eq!(
        std::fs::read_to_string(&closed_path).unwrap(),
        "<?php\n$renamed = 2;\n",
        "the closed file is rewritten on disk"
    );

    // The refusal half: a file operation poisons the whole edit.
    std::fs::write(&closed_path, "<?php\n$old = 2;\n").unwrap();
    let poisoned = lt::WorkspaceEdit {
        changes: None,
        document_changes: Some(lt::DocumentChanges::Operations(vec![
            lt::DocumentChangeOperation::Op(lt::ResourceOp::Delete(lt::DeleteFile {
                uri: elle_lsp::path_to_uri(&closed_path).unwrap(),
                options: None,
            })),
        ])),
        change_annotations: None,
    };
    let refused =
        workspace.update(cx, |workspace, cx| workspace.apply_workspace_edit_for_test(poisoned, cx));
    assert!(refused.is_err(), "file operations are refused whole");
    assert_eq!(
        std::fs::read_to_string(&closed_path).unwrap(),
        "<?php\n$old = 2;\n",
        "and nothing was touched"
    );

    draw(cx);
}

/// Confirming a quick-fix row applies that action's edit — by index, all files or none (#19).
///
/// The request half needs a live server; what this pins is the half that can corrupt a
/// file: the chosen row maps to the right pending edit, and the application goes through
/// the same all-or-nothing applier the rename test proves.
#[gpui::test]
async fn confirming_a_quick_fix_applies_the_chosen_edit(cx: &mut TestAppContext) {
    use elle_lsp::lsp_types as lt;
    install_theme(cx);
    let dir = project();
    let target = dir.path().join("fixable.php");
    std::fs::write(&target, "<?php\nnew User();\n").unwrap();

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));

    let import_edit = lt::WorkspaceEdit {
        changes: Some(
            [(
                elle_lsp::path_to_uri(&target).unwrap(),
                vec![lt::TextEdit {
                    range: lt::Range {
                        start: lt::Position { line: 1, character: 0 },
                        end: lt::Position { line: 1, character: 0 },
                    },
                    new_text: "use App\\Models\\User;\n".into(),
                }],
            )]
            .into_iter()
            .collect(),
        ),
        document_changes: None,
        change_annotations: None,
    };
    // A decoy at index 0, so "the right edit" is falsifiable rather than "the only edit".
    let decoy = lt::WorkspaceEdit::default();

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.set_pending_code_actions_for_test(vec![decoy, import_edit], cx);
        workspace.toggle_palette_for_test(crate::palette::PaletteMode::CodeActions, window, cx);
        workspace.confirm_palette_for_test("1", window, cx);
    });

    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "<?php\nuse App\\Models\\User;\nnew User();\n",
        "the chosen fix landed, not the decoy"
    );

    draw(cx);
}

/// With no server, mid-word invoke offers the buffer's own words (#20).
///
/// The degradation path: Intelephense not installed, the user types `$user` and invokes
/// — the file's identifiers are the honest answer. The trigger-char path stays empty on
/// purpose (`$user->` has no typed word, and with no signal every word is noise) — the
/// `typed.is_empty()` guard in `offer_buffer_words`, which the popup layout tests pin by
/// still expecting exactly their offered rows.
#[gpui::test]
async fn a_mid_word_invoke_with_no_server_offers_buffer_words(cx: &mut TestAppContext) {
    install_theme(cx);
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    workspace.update_in(cx, |workspace, window, cx| {
        let text = "<?php\n$username = 1;\n$user\n";
        let document =
            Document::new(Some(std::path::PathBuf::from("User.php")), text, true).unwrap();
        workspace.open_document_for_test(document, window, cx);
        if let Some(editor) = workspace.active_editor_for_test() {
            editor.update(cx, |editor, _cx| {
                let end = text.rfind("$user").unwrap() + "$user".len();
                editor.document.move_to(end, false);
            });
        }
    });
    draw(cx);
    workspace.update_in(cx, |workspace, window, cx| workspace.complete_for_test(window, cx));
    cx.run_until_parked();

    let popup = workspace
        .read_with(cx, |workspace, _cx| workspace.completion_for_test())
        .expect("popup open");
    popup.read_with(cx, |popup, _cx| {
        let items = popup.visible_items();
        let username = items
            .iter()
            .find(|item| item.label.as_ref() == "username")
            .expect("the file's identifier is offered");
        assert!(matches!(username.source, CompletionSource::Buffer));
        assert!(
            !items.iter().any(|item| item.label.as_ref() == "user"),
            "the word being typed is not offered back"
        );
    });

    draw(cx);
}

/// Saving a model rebuilds the Laravel index, so completions track the buffer (#21).
///
/// The staleness that matters in practice: the index was built at folder open, the user
/// adds a cast and saves, and the popup must know the new column without a reopen. The
/// rebuild is wholesale — two directories, milliseconds — which is #21's documented
/// starting point; the dependency-graph incremental pass replaces the *trigger's cost*,
/// not this behaviour.
#[gpui::test]
async fn saving_a_model_rebuilds_the_laravel_index(cx: &mut TestAppContext) {
    // Same HOME race as the other index-backed tests; same lock.
    let _home = crate::file_cache::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    install_theme(cx);
    let dir = project();
    std::fs::create_dir_all(dir.path().join("app/Models")).unwrap();
    let model_path = dir.path().join("app/Models/User.php");
    let before = "<?php\nclass User extends Model {\n    protected $casts = ['is_admin' => 'boolean'];\n}\n";
    std::fs::write(&model_path, before).unwrap();

    let canonical = dir.path().canonicalize().unwrap();
    let index_path = crate::file_cache::index_path(&canonical).expect("index path");
    {
        let (index, _) = elle_index::Index::open(&index_path).expect("index opens");
        elle_index::laravel::build(
            index.connection(),
            &canonical,
            &elle_workspace::CancelFlag::default(),
        )
        .expect("index builds");
    }

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        let document = Document::new(Some(model_path.clone()), before, true).unwrap();
        workspace.open_document_for_test(document, window, cx);
    });
    draw(cx);

    // The user adds a column-bearing cast and saves.
    let after = "<?php\nclass User extends Model {\n    protected $casts = ['is_admin' => 'boolean', 'settings' => 'array'];\n}\n";
    let editor = workspace
        .read_with(cx, |workspace, _cx| workspace.active_editor_for_test())
        .expect("a file is open");
    editor.update(cx, |editor, _cx| {
        let len = editor.document.buffer.text().len();
        editor.document.buffer.replace(0..len, after);
    });
    workspace.update_in(cx, |workspace, window, cx| workspace.save_for_test(window, cx));
    cx.run_until_parked();

    let (index, _) = elle_index::Index::open(&index_path).expect("index reopens");
    let columns =
        elle_index::laravel::columns_for_model(index.connection(), "User").expect("query");
    assert!(
        columns.iter().any(|column| column.name == "settings"),
        "the saved cast is in the index without a folder reopen: {:?}",
        columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
    );

    draw(cx);
}

/// Returning to the window rebuilds the Laravel index — the external-change trigger (#21).
///
/// `artisan make:model` in a terminal, a `git checkout` — none of them pass through this
/// editor's save path. The same reasoning as the git panel's third trigger (#64): to
/// notice stale completions you must first look at the window, and looking is the event.
/// No watcher, no timer — the perf gate measures idle CPU and would call a poll a
/// regression.
#[gpui::test]
async fn returning_to_the_window_rebuilds_the_laravel_index(cx: &mut TestAppContext) {
    // Same HOME race as the other index-backed tests; same lock.
    let _home = crate::file_cache::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    install_theme(cx);
    let dir = project();
    std::fs::create_dir_all(dir.path().join("app/Models")).unwrap();
    std::fs::write(
        dir.path().join("app/Models/User.php"),
        "<?php\nclass User extends Model {}\n",
    )
    .unwrap();

    let canonical = dir.path().canonicalize().unwrap();
    let index_path = crate::file_cache::index_path(&canonical).expect("index path");
    {
        let (index, _) = elle_index::Index::open(&index_path).expect("index opens");
        elle_index::laravel::build(
            index.connection(),
            &canonical,
            &elle_workspace::CancelFlag::default(),
        )
        .expect("index builds");
    }

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    workspace.update_in(cx, |workspace, _window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
    });
    cx.run_until_parked();

    // `artisan make:model Post` outside the editor — a file this session never touched.
    std::fs::write(
        dir.path().join("app/Models/Post.php"),
        "<?php\nclass Post extends Model {\n    protected $casts = ['published' => 'boolean'];\n}\n",
    )
    .unwrap();

    workspace.update_in(cx, |workspace, _window, cx| {
        workspace.window_became_active_for_test(cx);
    });
    cx.run_until_parked();

    let (index, _) = elle_index::Index::open(&index_path).expect("index reopens");
    let columns =
        elle_index::laravel::columns_for_model(index.connection(), "Post").expect("query");
    assert!(
        columns.iter().any(|column| column.name == "published"),
        "the externally created model is indexed after refocus"
    );

    draw(cx);
}

/// The log panel parses the newest log and a row's click lands on the throw site (#25).
#[gpui::test]
async fn the_log_panel_lists_entries_and_jumps_to_the_frame(cx: &mut TestAppContext) {
    install_theme(cx);
    let dir = project();
    std::fs::create_dir_all(dir.path().join("storage/logs")).unwrap();
    std::fs::create_dir_all(dir.path().join("app")).unwrap();
    let throw_site = dir.path().join("app/Broken.php");
    std::fs::write(&throw_site, "<?php\n\n\nthrow new \\Exception('x');\n").unwrap();

    // Two files; the newest by name must win. The trace points at a real file so the
    // jump has somewhere honest to land.
    std::fs::write(dir.path().join("storage/logs/laravel-2026-08-11.log"), "[2026-08-11 09:00:00] local.INFO: old day\n").unwrap();
    std::fs::write(
        dir.path().join("storage/logs/laravel-2026-08-12.log"),
        format!(
            "[2026-08-12 03:00:00] local.INFO: booted\n[2026-08-12 03:04:05] local.ERROR: boom {{\"exception\":\"x\"\n[stacktrace]\n#0 {}(4): throw()\n\"}}\n",
            throw_site.display()
        ),
    )
    .unwrap();

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    workspace.update_in(cx, |workspace, _window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        workspace.toggle_log_panel_for_test(cx);
    });
    cx.run_until_parked();

    let panel = workspace
        .read_with(cx, |workspace, _cx| workspace.log_panel_for_test())
        .expect("panel open");
    panel.read_with(cx, |panel, _cx| {
        let entries = panel.entries_for_test();
        assert_eq!(entries.len(), 2, "the newest FILE only, not the old day's");
        assert_eq!(entries[0].message, "boom", "newest entry first");
        assert_eq!(entries[1].message, "booted");
    });

    panel.update_in(cx, |panel, window, cx| panel.jump_for_test(0, window, cx));
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, cx| {
        let editor = workspace.active_editor_for_test().expect("the throw site opened");
        assert_eq!(editor.read(cx).document.cursor_point().row, 3, "line 4, 0-based row 3");
    });

    draw(cx);
}

/// A project with no Docker files gets the honest line, and nothing ran (#25).
///
/// The deterministic half: the with-daemon path depends on a docker install the test
/// machine is not guaranteed, so the service merge is tested pure in `elle-docker` and
/// this pins the detection refusal plus never-at-entry-without-files.
#[gpui::test]
async fn a_dockerless_project_is_told_so_not_probed(cx: &mut TestAppContext) {
    install_theme(cx);
    let dir = project();
    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    workspace.update_in(cx, |workspace, _window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        workspace.show_docker_panel_for_test(cx);
    });
    cx.run_until_parked();

    workspace.read_with(cx, |workspace, _cx| {
        let state = workspace.docker_services_for_test().expect("loaded");
        assert_eq!(
            state.unwrap_err(),
            "Not a Docker project (no Dockerfile or compose file)",
            "detection refuses before any docker CLI call"
        );
    });

    draw(cx);
}

/// The composer-script palette lists composer.json's own scripts, and confirming
/// opens the terminal to type into — never runs (#26).
#[gpui::test]
async fn the_composer_script_palette_lists_and_types(cx: &mut TestAppContext) {
    install_theme(cx);
    let dir = project();
    std::fs::write(
        dir.path().join("composer.json"),
        r#"{"scripts": {"test": "pest", "lint": "pint"}}"#,
    )
    .unwrap();

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_folder_for_test(dir.path().to_path_buf(), cx);
        workspace.toggle_palette_for_test(crate::palette::PaletteMode::ComposerScripts, window, cx);
    });
    cx.run_until_parked();

    let labels = workspace.read_with(cx, |workspace, cx| workspace.palette_labels_for_test(cx));
    assert_eq!(labels, ["lint", "test"], "the file's own scripts, alphabetical");

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.confirm_palette_for_test("test", window, cx);
    });
    workspace.read_with(cx, |workspace, cx| {
        let terminal = workspace.terminal_for_test().expect("the terminal opened to type into");
        assert!(terminal.read(cx).session_count() > 0);
    });

    draw(cx);
}

/// Losing window focus saves every dirty tab that has a path — autosave (#25 follow-up).
///
/// The default is ON (the settings crate's doc records why); a pathless scratch buffer
/// is skipped because autosave must never open a dialog.
#[gpui::test]
async fn losing_focus_autosaves_dirty_tabs(cx: &mut TestAppContext) {
    install_theme(cx);
    let dir = project();
    let path = dir.path().join("a.php");
    std::fs::write(&path, "<?php\n$old = 1;\n").unwrap();

    let (workspace, cx) = cx.add_window_view(|_window, cx| WorkspaceView::new(registry(), cx));
    workspace.update_in(cx, |workspace, window, cx| {
        let text = std::fs::read_to_string(&path).unwrap();
        let document = Document::new(Some(path.clone()), &text, true).unwrap();
        workspace.open_document_for_test(document, window, cx);
    });

    let editor = workspace
        .read_with(cx, |workspace, _cx| workspace.active_editor_for_test())
        .expect("a file is open");
    editor.update(cx, |editor, _cx| {
        let len = editor.document.buffer.len_bytes();
        editor.document.buffer.replace(0..len, "<?php\n$new = 2;\n");
        assert!(editor.document.buffer.is_dirty());
    });

    workspace.update(cx, |workspace, cx| workspace.window_lost_focus_for_test(cx));
    cx.run_until_parked();

    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "<?php\n$new = 2;\n",
        "the blur wrote the buffer to disk"
    );
    editor.read_with(cx, |editor, _cx| {
        assert!(!editor.document.buffer.is_dirty(), "and the dot cleared");
    });

    draw(cx);
}

/// Typing into the git panel's commit box builds the message, and Enter commits only
/// when something is staged (#64 item 4).
///
/// This pins the *state* path — keystroke to `commit_message` to `CommitRequested` —
/// through the same shared-tail door the palette uses, because a `KeyDownEvent` cannot
/// be conjured headlessly. What it cannot pin is the focus wiring that was the actual
/// bug: whether a click on the box puts the panel in the window's focus path so real
/// keystrokes arrive at all. That stays on issue #35's human list.
#[gpui::test]
async fn typing_in_the_commit_box_builds_the_message_and_enter_guards_on_staged(
    cx: &mut TestAppContext,
) {
    use elle_git::{FileStatus, RepoStatus, Status};

    use crate::git_panel::{GitEvent, GitPanel, PanelState};

    install_theme(cx);
    let committed = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));

    let (panel, subscription) = cx.update(|cx| {
        use gpui::AppContext as _;
        let panel = cx.new(GitPanel::new);
        let sink = committed.clone();
        let subscription = cx.subscribe(&panel, move |_panel, event, _cx| {
            if let GitEvent::CommitRequested { message } = event {
                sink.borrow_mut().push(message.clone());
            }
        });
        (panel, subscription)
    });

    panel.update(cx, |panel, cx| {
        panel.type_for_test("fix: caret!", cx);
        assert_eq!(panel.commit_message_for_test(), "fix: caret!");
        panel.backspace_for_test(cx);
        assert_eq!(panel.commit_message_for_test(), "fix: caret", "backspace pops one char");

        // Nothing staged yet: Enter must be a no-op, not a commit of nothing.
        panel.commit_for_test(cx);
    });
    assert!(committed.borrow().is_empty(), "no staged file, no commit");

    panel.update(cx, |panel, cx| {
        panel.set_state(
            PanelState::Repo(RepoStatus {
                branch: Some("main".to_string()),
                files: vec![FileStatus {
                    path: std::path::PathBuf::from("/r/a.php"),
                    relative: "a.php".to_string(),
                    status: Status::Modified,
                    staged: true,
                }],
                cancelled: false,
            }),
            cx,
        );
        panel.commit_for_test(cx);
    });
    assert_eq!(*committed.borrow(), ["fix: caret"], "staged + message = commit");
    drop(subscription);
}
