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
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        Undo,
        Redo,
        Copy,
        Cut,
        Paste,
        ToggleHiddenFiles,
        NewTerminal,
        ToggleTerminal,
        ToggleTheme,
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
        // ctrl-` is the conventional terminal toggle. It is bound workspace-wide so it
        // also *closes* the panel while the terminal itself has focus.
        KeyBinding::new("ctrl-`", ToggleTerminal, Some(context::WORKSPACE)),
        KeyBinding::new("ctrl-shift-`", NewTerminal, Some(context::WORKSPACE)),
        // `ToggleTheme` is deliberately unbound: it reaches the user through the palette.
        // Every obvious chord (cmd-k, cmd-t) is a prefix or a tab command elsewhere, and
        // picking one now means choosing a keymap before there is a file to override it in.
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
        KeyBinding::new("shift-left", SelectLeft, Some(context::EDITOR)),
        KeyBinding::new("shift-right", SelectRight, Some(context::EDITOR)),
        KeyBinding::new("shift-up", SelectUp, Some(context::EDITOR)),
        KeyBinding::new("shift-down", SelectDown, Some(context::EDITOR)),
        KeyBinding::new("cmd-a", SelectAll, Some(context::EDITOR)),
        KeyBinding::new("cmd-z", Undo, Some(context::EDITOR)),
        KeyBinding::new("cmd-shift-z", Redo, Some(context::EDITOR)),
        KeyBinding::new("cmd-c", Copy, Some(context::EDITOR)),
        KeyBinding::new("cmd-x", Cut, Some(context::EDITOR)),
        KeyBinding::new("cmd-v", Paste, Some(context::EDITOR)),
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
    Quit,
    NewTerminal,
    ToggleTerminal,
    ToggleTheme,
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
        "terminal.new" => Dispatch::NewTerminal,
        "terminal.toggle" => Dispatch::ToggleTerminal,
        "theme.toggle" => Dispatch::ToggleTheme,
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
