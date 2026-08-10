//! gpui actions, and the bridge to `elle_core`'s command ids.
//!
//! gpui already owns dispatch (keymap -> action -> `on_action` handler), so there is no
//! second dispatcher here. `elle_core::CommandRegistry` supplies the palette's *list*;
//! this module maps each row of that list back to the action to fire.

use elle_core::{BUILTIN_COMMANDS, CommandId, CommandRegistry};
use gpui::{App, KeyBinding, actions};

actions!(
    ellefuanti,
    [
        Quit,
        OpenFolder,
        NewFile,
        Save,
        CloseTab,
        ToggleCommandPalette,
        ToggleQuickOpen,
        Cancel,
        Confirm,
        SelectNext,
        SelectPrev,
        Backspace,
        Delete,
        Newline,
        Tab,
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        MoveLineStart,
        MoveLineEnd,
        MoveWordLeft,
        MoveWordRight,
        MoveDocumentStart,
        MoveDocumentEnd,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectLineStart,
        SelectLineEnd,
        SelectWordLeft,
        SelectWordRight,
        SelectDocumentStart,
        SelectDocumentEnd,
        SelectAll,
        DeleteWordLeft,
        DeleteWordRight,
        DeleteToLineStart,
        DeleteToLineEnd,
        MoveLineUp,
        MoveLineDown,
        DuplicateLineUp,
        DuplicateLineDown,
        DeleteLine,
        OpenLineBelow,
        OpenLineAbove,
        Outdent,
        Indent,
        Undo,
        Redo,
        Copy,
        Cut,
        Paste,
        ToggleHiddenFiles,
        NewTerminal,
        ToggleTerminal,
        ToggleTheme,
        // The View menu needs an action for route search; the palette only ever reached
        // route mode through a command id, never a keybinding, so there was none.
        GoToRoute,
        OpenSettings,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize,
    ]
);

/// Key context names used in keymaps. Constants so a typo is a compile error rather
/// than a binding that silently never fires.
pub mod context {
    pub const WORKSPACE: &str = "Workspace";
    pub const EDITOR: &str = "Editor";
    pub const PALETTE: &str = "Palette";
    pub const TERMINAL: &str = "Terminal";
}

/// Registers the default keymap and the palette's command list.
///
/// Bindings are scoped by key context so `enter` in the palette confirms a selection
/// while `enter` in the editor inserts a newline.
pub fn init(cx: &mut App) -> CommandRegistry {
    cx.bind_keys([
        // Workspace-wide
        KeyBinding::new("cmd-o", OpenFolder, Some(context::WORKSPACE)),
        KeyBinding::new("cmd-n", NewFile, Some(context::WORKSPACE)),
        KeyBinding::new("cmd-s", Save, Some(context::WORKSPACE)),
        KeyBinding::new("cmd-w", CloseTab, Some(context::WORKSPACE)),
        KeyBinding::new("cmd-shift-p", ToggleCommandPalette, Some(context::WORKSPACE)),
        KeyBinding::new("cmd-p", ToggleQuickOpen, Some(context::WORKSPACE)),
        KeyBinding::new("cmd-shift-.", ToggleHiddenFiles, Some(context::WORKSPACE)),
        // ⌘, is the macOS convention for preferences, and the menu item shows it.
        KeyBinding::new("cmd-,", OpenSettings, Some(context::WORKSPACE)),
        // ctrl-` is the conventional terminal toggle. It is bound workspace-wide so it
        // also *closes* the panel while the terminal itself has focus.
        KeyBinding::new("ctrl-`", ToggleTerminal, Some(context::WORKSPACE)),
        KeyBinding::new("ctrl-shift-`", NewTerminal, Some(context::WORKSPACE)),
        // `ToggleTheme` is deliberately unbound: it reaches the user through the palette.
        // Every obvious chord (cmd-k, cmd-t) is a prefix or a tab command elsewhere, and
        // picking one now means choosing a keymap before there is a file to override it in.
        //
        // Zoom (#49). Bound and *not* in the palette, the opposite of `ToggleTheme`: these
        // are held down and repeated, which is a chord's job and not a command list's.
        // Three bindings for two keys because macOS reports the unshifted `=` for ⌘+ on a
        // US layout while a numpad or a shifted press reports `+`; binding only one means
        // the key works on some keyboards and not others.
        KeyBinding::new("cmd-=", IncreaseFontSize, Some(context::WORKSPACE)),
        KeyBinding::new("cmd-+", IncreaseFontSize, Some(context::WORKSPACE)),
        KeyBinding::new("cmd--", DecreaseFontSize, Some(context::WORKSPACE)),
        KeyBinding::new("cmd-0", ResetFontSize, Some(context::WORKSPACE)),
        //
        // Overlay
        KeyBinding::new("escape", Cancel, Some(context::PALETTE)),
        KeyBinding::new("enter", Confirm, Some(context::PALETTE)),
        KeyBinding::new("down", SelectNext, Some(context::PALETTE)),
        KeyBinding::new("up", SelectPrev, Some(context::PALETTE)),
        KeyBinding::new("ctrl-n", SelectNext, Some(context::PALETTE)),
        KeyBinding::new("ctrl-p", SelectPrev, Some(context::PALETTE)),
        KeyBinding::new("backspace", Backspace, Some(context::PALETTE)),
        // Editor
        KeyBinding::new("backspace", Backspace, Some(context::EDITOR)),
        KeyBinding::new("delete", Delete, Some(context::EDITOR)),
        KeyBinding::new("enter", Newline, Some(context::EDITOR)),
        KeyBinding::new("tab", Tab, Some(context::EDITOR)),
        KeyBinding::new("left", MoveLeft, Some(context::EDITOR)),
        KeyBinding::new("right", MoveRight, Some(context::EDITOR)),
        KeyBinding::new("up", MoveUp, Some(context::EDITOR)),
        KeyBinding::new("down", MoveDown, Some(context::EDITOR)),
        KeyBinding::new("cmd-left", MoveLineStart, Some(context::EDITOR)),
        KeyBinding::new("cmd-right", MoveLineEnd, Some(context::EDITOR)),
        KeyBinding::new("home", MoveLineStart, Some(context::EDITOR)),
        KeyBinding::new("end", MoveLineEnd, Some(context::EDITOR)),
        KeyBinding::new("alt-left", MoveWordLeft, Some(context::EDITOR)),
        KeyBinding::new("alt-right", MoveWordRight, Some(context::EDITOR)),
        KeyBinding::new("cmd-up", MoveDocumentStart, Some(context::EDITOR)),
        KeyBinding::new("cmd-down", MoveDocumentEnd, Some(context::EDITOR)),
        KeyBinding::new("shift-left", SelectLeft, Some(context::EDITOR)),
        KeyBinding::new("shift-right", SelectRight, Some(context::EDITOR)),
        KeyBinding::new("shift-up", SelectUp, Some(context::EDITOR)),
        KeyBinding::new("shift-down", SelectDown, Some(context::EDITOR)),
        KeyBinding::new("cmd-shift-left", SelectLineStart, Some(context::EDITOR)),
        KeyBinding::new("cmd-shift-right", SelectLineEnd, Some(context::EDITOR)),
        KeyBinding::new("alt-shift-left", SelectWordLeft, Some(context::EDITOR)),
        KeyBinding::new("alt-shift-right", SelectWordRight, Some(context::EDITOR)),
        KeyBinding::new("cmd-shift-up", SelectDocumentStart, Some(context::EDITOR)),
        KeyBinding::new("cmd-shift-down", SelectDocumentEnd, Some(context::EDITOR)),
        KeyBinding::new("alt-backspace", DeleteWordLeft, Some(context::EDITOR)),
        KeyBinding::new("alt-delete", DeleteWordRight, Some(context::EDITOR)),
        KeyBinding::new("cmd-backspace", DeleteToLineStart, Some(context::EDITOR)),
        KeyBinding::new("cmd-delete", DeleteToLineEnd, Some(context::EDITOR)),
        KeyBinding::new("alt-up", MoveLineUp, Some(context::EDITOR)),
        KeyBinding::new("alt-down", MoveLineDown, Some(context::EDITOR)),
        KeyBinding::new("alt-shift-up", DuplicateLineUp, Some(context::EDITOR)),
        KeyBinding::new("alt-shift-down", DuplicateLineDown, Some(context::EDITOR)),
        KeyBinding::new("cmd-shift-k", DeleteLine, Some(context::EDITOR)),
        KeyBinding::new("cmd-enter", OpenLineBelow, Some(context::EDITOR)),
        KeyBinding::new("cmd-shift-enter", OpenLineAbove, Some(context::EDITOR)),
        // `tab` itself stays bound to `Tab`, which indents a selection and otherwise
        // inserts spaces — the shape every editor has. ⇧⇥ only ever outdents.
        KeyBinding::new("shift-tab", Outdent, Some(context::EDITOR)),
        KeyBinding::new("cmd-]", Indent, Some(context::EDITOR)),
        KeyBinding::new("cmd-[", Outdent, Some(context::EDITOR)),
        KeyBinding::new("cmd-a", SelectAll, Some(context::EDITOR)),
        KeyBinding::new("cmd-z", Undo, Some(context::EDITOR)),
        KeyBinding::new("cmd-shift-z", Redo, Some(context::EDITOR)),
        KeyBinding::new("cmd-c", Copy, Some(context::EDITOR)),
        KeyBinding::new("cmd-x", Cut, Some(context::EDITOR)),
        KeyBinding::new("cmd-v", Paste, Some(context::EDITOR)),
        // Terminal. Copy and paste are on ⌘, never ⌃: ⌃C must reach the shell as SIGINT,
        // which is the whole reason macOS terminals moved copy to the command key. There
        // is no `Cut` — a terminal's scrollback is not editable.
        KeyBinding::new("cmd-c", Copy, Some(context::TERMINAL)),
        KeyBinding::new("cmd-v", Paste, Some(context::TERMINAL)),
        KeyBinding::new("cmd-a", SelectAll, Some(context::TERMINAL)),
    ]);

    let mut registry = CommandRegistry::new();
    registry.register_all(BUILTIN_COMMANDS.iter().copied());
    registry
}

/// What a palette row does when confirmed.
///
/// An enum rather than a boxed closure per command: the set is small, exhaustive
/// matching catches a command added to the registry without a handler, and no
/// allocation happens per palette row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dispatch {
    OpenFolder,
    NewFile,
    Save,
    CloseTab,
    QuickOpen,
    Routes,
    Quit,
    NewTerminal,
    ToggleTerminal,
    ToggleTheme,
    ToggleHiddenFiles,
    OpenSettings,
    /// Registered but not wired up yet (a later milestone's command).
    Unhandled,
}

/// Maps a command id from the registry to the action the palette should run.
pub fn dispatch_for(id: CommandId) -> Dispatch {
    match id.0 {
        "workspace.open_folder" => Dispatch::OpenFolder,
        "editor.new_file" => Dispatch::NewFile,
        "workspace.quit" => Dispatch::Quit,
        "editor.save" => Dispatch::Save,
        "editor.close" => Dispatch::CloseTab,
        "palette.quick_open" => Dispatch::QuickOpen,
        "laravel.routes" => Dispatch::Routes,
        "terminal.new" => Dispatch::NewTerminal,
        "terminal.toggle" => Dispatch::ToggleTerminal,
        "theme.toggle" => Dispatch::ToggleTheme,
        "workspace.toggle_hidden_files" => Dispatch::ToggleHiddenFiles,
        "workspace.open_settings" => Dispatch::OpenSettings,
        // `palette.toggle` is how you got here; re-running it is a no-op by design.
        _ => Dispatch::Unhandled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_command_except_the_palette_itself_is_wired() {
        for command in BUILTIN_COMMANDS {
            if command.id == CommandId("palette.toggle") {
                continue;
            }
            assert_ne!(
                dispatch_for(command.id),
                Dispatch::Unhandled,
                "{} appears in the palette with no handler",
                command.id
            );
        }
    }
}
