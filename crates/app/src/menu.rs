//! The macOS menu bar — a second view over the same commands the palette lists.
//!
//! The rule this module exists to enforce: **a menu item names a command id, and the
//! action it fires is the one `dispatch_for` already maps that id to.** The menu does not
//! get its own list of things the app can do. If it did, the palette and the menu would
//! drift the first time somebody added a command to one and forgot the other, and the
//! menu would quietly become a lie about what the app supports.
//!
//! So every item here is an `Item`, which carries both halves, and
//! `every_menu_item_names_a_real_command` checks the id half against the registry. Items
//! that are *not* commands — Hide, Minimise, Cut/Copy/Paste — are `Item::system`: standard
//! macOS behaviour that was never a `Command` and should not become one to satisfy a test.
//!
//! Items whose command does not exist yet are **absent**, not disabled. gpui does grey out
//! an item whose action no view handles (`on_validate_app_menu_command` -> `is_action_available`),
//! but that is for an item that is live in one context and not another — Save with no file
//! open. A permanently dead entry advertising a feature nobody built is worse than silence,
//! which is the opposite of the activity bar's §6 rule, where disabled panels *are* the roadmap.

use gpui::{App, Menu, MenuItem, OsAction};

use crate::actions::{
    CloseTab, Copy, Cut, NewFile, NewTerminal, OpenFolder, Paste, Quit, Redo, Save, SelectAll,
    ToggleCommandPalette, ToggleHiddenFiles, ToggleQuickOpen, ToggleTerminal, ToggleTheme, Undo,
};

/// One row of a menu, before gpui sees it.
///
/// The `command` field is the whole point: it is what the drift test reads. `None` means
/// "standard platform behaviour, deliberately not a command" rather than "not wired up yet".
struct Item {
    label: &'static str,
    /// The registry id this row is a view of, if it is a command at all.
    ///
    /// Only the tests read it — the running app needs the action, not the id. That is the
    /// intended shape rather than dead weight: recording the id is what lets the build fail
    /// when a command is renamed out from under a menu item.
    #[cfg_attr(not(test), allow(dead_code))]
    command: Option<&'static str>,
    build: fn(&'static str) -> MenuItem,
}

impl Item {
    /// A row backed by a registry command. The id is checked by the test below.
    const fn command(
        label: &'static str,
        id: &'static str,
        build: fn(&'static str) -> MenuItem,
    ) -> Self {
        Self { label, command: Some(id), build }
    }

    /// A row that is standard macOS behaviour, not an ellefuanti command.
    const fn system(label: &'static str, build: fn(&'static str) -> MenuItem) -> Self {
        Self { label, command: None, build }
    }
}

/// The menu bar, as data. Separated from `set_menus` so the test can read it without
/// starting a platform — building `MenuItem`s needs no window, but installing them does.
fn menu_bar() -> Vec<(&'static str, Vec<Item>)> {
    vec![
        (
            "ellefuanti",
            vec![
                // Unblocked by #76: opens settings.json in a tab, since the file *is* the
                // settings interface for now. macOS puts this under the app menu, not Edit.
                Item::command("Settings…", "workspace.open_settings", |label| {
                    MenuItem::action(label, crate::actions::OpenSettings)
                }),
                // No "About": gpui 0.2.2 has no about-panel call (`open_about_panel`
                // exists on `main`, not on the pinned release — ADR-0002's trap exactly) and
                // no version accessor either. Showing one means an `objc`/`cocoa` dependency
                // and an `unsafe` msg_send for `orderFrontStandardAboutPanel:`, which is a
                // lot of new surface for one dialog. Left out until something else here
                // needs AppKit directly.
                Item::system("Services", |_| {
                    MenuItem::os_submenu("Services", gpui::SystemMenuType::Services)
                }),
                Item::system("Hide ellefuanti", |label| MenuItem::action(label, Hide)),
                Item::system("Hide Others", |label| MenuItem::action(label, HideOthers)),
                Item::command("Quit ellefuanti", "workspace.quit", |label| {
                    MenuItem::action(label, Quit)
                }),
            ],
        ),
        (
            "File",
            vec![
                Item::command("New File", "editor.new_file", |label| {
                    MenuItem::action(label, NewFile)
                }),
                Item::command("Open Folder…", "workspace.open_folder", |label| {
                    MenuItem::action(label, OpenFolder)
                }),
                Item::command("Save", "editor.save", |label| MenuItem::action(label, Save)),
                Item::command("Close Tab", "editor.close", |label| {
                    MenuItem::action(label, CloseTab)
                }),
                // ponytail: no "Recent Folders" — the list has to survive a restart to mean
                // anything, and persistence is #60's store. No item beats an empty submenu.
            ],
        ),
        (
            "Edit",
            vec![
                // `os_action` matters here: it hands macOS the selector it expects
                // (`cut:`, `copy:`, `paste:`, `selectAll:`), so these keep working in native
                // text fields — the open-folder dialog — where our editor is not focused.
                Item::system("Undo", |label| MenuItem::os_action(label, Undo, OsAction::Undo)),
                Item::system("Redo", |label| MenuItem::os_action(label, Redo, OsAction::Redo)),
                Item::system("Cut", |label| MenuItem::os_action(label, Cut, OsAction::Cut)),
                Item::system("Copy", |label| MenuItem::os_action(label, Copy, OsAction::Copy)),
                Item::system("Paste", |label| MenuItem::os_action(label, Paste, OsAction::Paste)),
                Item::system("Select All", |label| {
                    MenuItem::os_action(label, SelectAll, OsAction::SelectAll)
                }),
                // #80. Unlike the motions below, these two belong in a menu: find is what
                // someone reaches for by name in an editor they do not know yet, and Edit
                // is where every other application puts it. Plain `action`, not
                // `os_action` — macOS's `performFindPanelAction:` drives *its* find panel,
                // which is not this one.
                Item::command("Find…", "editor.find", |label| {
                    MenuItem::action(label, crate::actions::Find)
                }),
                Item::command("Replace…", "editor.replace", |label| {
                    MenuItem::action(label, crate::actions::Replace)
                }),
                Item::command("Find in Project…", "editor.find_in_project", |label| {
                    MenuItem::action(label, crate::actions::FindInProject)
                }),
                // ponytail: the #73 motions (Move Line Up, Duplicate Line, Delete Line,
                // Indent/Outdent) are deliberately not here. They are keyboard verbs used
                // mid-flow — reaching for a menu to duplicate a line is not a thing anyone
                // does — and none of them is a registry command, so each would be a new
                // command invented to justify a menu row. That is the drift this menu avoids.
            ],
        ),
        (
            "View",
            vec![
                Item::command("Command Palette", "palette.toggle", |label| {
                    MenuItem::action(label, ToggleCommandPalette)
                }),
                Item::command("Quick Open File", "palette.quick_open", |label| {
                    MenuItem::action(label, ToggleQuickOpen)
                }),
                Item::command("Go to Route…", "laravel.routes", |label| {
                    MenuItem::action(label, crate::actions::GoToRoute)
                }),
                Item::command("Toggle Terminal", "terminal.toggle", |label| {
                    MenuItem::action(label, ToggleTerminal)
                }),
                Item::command("New Terminal", "terminal.new", |label| {
                    MenuItem::action(label, NewTerminal)
                }),
                // Test runner (#25). Under View with the terminal because the panel is the
                // same kind of thing; the three run commands are next to it because that is
                // where someone who just opened the panel will look for them.
                Item::command("Toggle Test Panel", "tests.toggle", |label| {
                    MenuItem::action(label, crate::actions::ToggleTestPanel)
                }),
                Item::command("Run All Tests", "tests.run", |label| {
                    MenuItem::action(label, crate::actions::RunTests)
                }),
                Item::command("Run Tests in Current File", "tests.run_file", |label| {
                    MenuItem::action(label, crate::actions::RunTestsInFile)
                }),
                Item::command("Rerun Failed Tests", "tests.rerun_failed", |label| {
                    MenuItem::action(label, crate::actions::RerunFailedTests)
                }),
                Item::command("Switch Theme", "theme.toggle", |label| {
                    MenuItem::action(label, ToggleTheme)
                }),
                Item::command("Toggle Full Screen", "view.fullscreen", |label| {
                    MenuItem::action(label, crate::actions::ToggleFullscreen)
                }),
                Item::command("Toggle Zen Mode", "view.zen", |label| {
                    MenuItem::action(label, crate::actions::ToggleZen)
                }),
                // The AI chat panel (#99), under View with the other panels.
                Item::command("Toggle AI Chat", "ai.chat", |label| {
                    MenuItem::action(label, crate::actions::ToggleAiChat)
                }),
                Item::command("Toggle Hidden Files", "workspace.toggle_hidden_files", |label| {
                    MenuItem::action(label, ToggleHiddenFiles)
                }),
                // ponytail: no "Toggle Sidebar" — the sidebar has no toggle, in the keymap or
                // anywhere else, so the item would need the feature built first. And no
                // Zoom In/Out: #49, fonts are compile-time constants.
            ],
        ),
        // Its own menu rather than more rows under View: these are all "take me somewhere",
        // which is what every IDE calls Go, and View is already the miscellany drawer.
        (
            "Go",
            vec![
                Item::command("Go to Symbol in File…", "navigate.symbol", |label| {
                    MenuItem::action(label, crate::actions::GoToSymbol)
                }),
                Item::command("Go to Definition", "navigate.definition", |label| {
                    MenuItem::action(label, crate::actions::GoToDefinition)
                }),
                Item::command("Find Usages", "navigate.references", |label| {
                    MenuItem::action(label, crate::actions::FindReferences)
                }),
                Item::command("Back", "navigate.back", |label| {
                    MenuItem::action(label, crate::actions::NavigateBack)
                }),
                Item::command("Forward", "navigate.forward", |label| {
                    MenuItem::action(label, crate::actions::NavigateForward)
                }),
                // ponytail: no "Go to Symbol in Project…" — that needs `workspace/symbol`,
                // which is not among the typed methods `crates/lsp` has. #81 stops here.
            ],
        ),
        (
            "Window",
            vec![
                Item::system("Minimize", |label| MenuItem::action(label, Minimize)),
                Item::system("Zoom", |label| MenuItem::action(label, Zoom)),
            ],
        ),
        (
            "Help",
            vec![Item::system("ellefuanti on GitHub", |label| {
                MenuItem::action(label, OpenRepository)
            })],
        ),
    ]
}

/// Installs the menu bar and the handlers for the items that are not commands.
///
/// Called after `actions::init`, because gpui reads the keymap when it builds the menu:
/// the ⌘S beside "Save" is looked up from the binding, not written here. Bind first or the
/// items render bare, which looks like the shortcuts do not exist.
pub fn init(cx: &mut App) {
    cx.on_action(|_: &Hide, cx: &mut App| cx.hide());
    cx.on_action(|_: &HideOthers, cx: &mut App| cx.hide_other_apps());
    cx.on_action(|_: &Minimize, cx: &mut App| {
        // Whichever window the user is looking at, which on a single-window app is the
        // only one — but asking the platform beats keeping our own handle in a global.
        if let Some(window) = cx.active_window() {
            let _ = window.update(cx, |_, window, _| window.minimize_window());
        }
    });
    cx.on_action(|_: &Zoom, cx: &mut App| {
        if let Some(window) = cx.active_window() {
            let _ = window.update(cx, |_, window, _| window.zoom_window());
        }
    });
    cx.on_action(|_: &OpenRepository, cx: &mut App| cx.open_url(REPOSITORY_URL));

    cx.set_menus(
        menu_bar()
            .into_iter()
            .map(|(name, items)| Menu {
                name: name.into(),
                items: items.into_iter().map(|item| (item.build)(item.label)).collect(),
            })
            .collect(),
    );
}

const REPOSITORY_URL: &str = "https://github.com/richasdev/ellefuanti";

gpui::actions!(menu, [Hide, HideOthers, Minimize, Zoom, OpenRepository]);

#[cfg(test)]
mod tests {
    use super::*;
    use elle_core::{BUILTIN_COMMANDS, CommandId, CommandRegistry};

    fn registry() -> CommandRegistry {
        let mut registry = CommandRegistry::new();
        registry.register_all(BUILTIN_COMMANDS.iter().copied());
        registry
    }

    /// The guard this module exists for.
    ///
    /// Worth more than any test of the menu's *structure*: nobody is going to accidentally
    /// delete the File menu, but somebody will rename `editor.close` to `tab.close` and
    /// leave the menu pointing at an id that no longer resolves. Then the item still renders,
    /// still looks live, and the palette and the menu disagree about what the app can do.
    #[test]
    fn every_menu_item_names_a_real_command() {
        let registry = registry();
        let mut unknown = Vec::new();

        for (menu, items) in menu_bar() {
            for item in items {
                let Some(id) = item.command else { continue };
                if registry.get(CommandId(id)).is_none() {
                    unknown.push(format!("{menu} > {} -> {id}", item.label));
                }
            }
        }

        assert!(
            unknown.is_empty(),
            "these menu items name a command id the registry does not have, so the menu and \
             the palette have drifted — either the id was renamed or the item should be \
             removed: {unknown:#?}"
        );
    }

    /// The other half of the same drift: an item that fires an action the palette maps to
    /// nothing. `Dispatch::Unhandled` is how `actions.rs` marks a command that is registered
    /// but not wired up, and a menu row for one would be a dead entry that still looks live.
    #[test]
    fn every_menu_command_is_actually_dispatchable() {
        use crate::actions::{Dispatch, dispatch_for};

        let mut dead = Vec::new();
        for (menu, items) in menu_bar() {
            for item in items {
                let Some(id) = item.command else { continue };
                // The palette's own entry point; re-running it is a documented no-op.
                if id == "palette.toggle" {
                    continue;
                }
                if dispatch_for(CommandId(id)) == Dispatch::Unhandled {
                    dead.push(format!("{menu} > {} -> {id}", item.label));
                }
            }
        }

        assert!(dead.is_empty(), "menu items whose command has no handler: {dead:#?}");
    }

    /// Runs the real `init` — building every `MenuItem` and registering every handler.
    ///
    /// What this proves: the menu is constructible and installing it does not panic, which
    /// is worth having because `menu_bar()` is only ever fully evaluated here and in `main`.
    ///
    /// What it does **not** prove: that a menu bar appears. gpui's test platform implements
    /// `set_menus` as an empty stub, so nothing is handed to AppKit and `get_menus` stays
    /// empty. Only launching the bundle shows the real thing — no test in this file can.
    #[gpui::test]
    async fn installing_the_menu_bar_builds_every_item(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::actions::init(cx);
            init(cx);
        });
    }

    /// Blocked work stays out of the menu bar rather than sitting there greyed out. If
    /// somebody adds one of these before its issue lands, this fails and says why.
    #[test]
    fn items_blocked_on_other_issues_are_absent() {
        let labels: Vec<&str> =
            menu_bar().into_iter().flat_map(|(_, items)| items).map(|item| item.label).collect();

        // "Settings…" was on this list until #76 landed the file it opens — which is the
        // intended way off it: build the thing, then add the item.
        for blocked in ["About ellefuanti", "Zoom In", "Zoom Out", "Toggle Sidebar"] {
            assert!(
                !labels.contains(&blocked),
                "{blocked:?} has nothing to open yet (About: no gpui 0.2.2 API, Zoom In/Out: \
                 #49, Toggle Sidebar: no such feature). An item that never becomes enabled is \
                 worse than no item."
            );
        }
    }
}
