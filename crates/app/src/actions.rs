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
        AcceptPredictionWord,
        AcceptPredictionLine,
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
        ToggleComment,
        Undo,
        Redo,
        Copy,
        Cut,
        Paste,
        ToggleHiddenFiles,
        NewTerminal,
        ToggleTerminal,
        // Terminal close and split (#97). Both terminal-scoped, so neither has any effect
        // unless the panel itself has focus.
        CloseTerminal,
        SplitTerminal,
        ToggleTheme,
        // Fullscreen and Zen (owner request). Fullscreen defers to the platform;
        // Zen hides the chrome around the editor and is this app's own state.
        ToggleFullscreen,
        ToggleZen,
        // The AI chat panel (#99). Workspace-scoped like the terminal toggle, and for the
        // same reason: the chord must also *close* the panel while the panel has focus.
        ToggleAiChat,
        /// Clears the AI chat transcript (⌃L), the shell's own chord for the same act.
        /// Explicit, because closing the panel no longer discards anything.
        ClearAiChat,
        // The preview pane (#31). Workspace-scoped for the same reason as the two above:
        // the chord closes the pane while the pane has focus.
        TogglePreview,
        // The View menu needs an action for route search; the palette only ever reached
        // route mode through a command id, never a keybinding, so there was none.
        GoToRoute,
        // Completion (#61). `Complete` is ⌥⌘I and opens the popup at the cursor with every
        // source that has something to say. Not ⌃space, which macOS intercepts — the chord
        // is chosen and justified where it is bound, below.
        //
        // `CompleteLaravel` is kept and is no longer bound to a key: #83 registered it as a
        // palette *command* (`laravel.route_name`), so removing it would delete a row from
        // the command palette that people may have learned. It now opens the popup filtered
        // to route names, which is what that command always meant.
        Complete,
        CompleteLaravel,
        // Navigation (#81). `GoToDefinition` and `FindReferences` are also reachable by
        // ⌘click and the Go menu; the palette-backed two are keyboard-only.
        GoToSymbol,
        SetLanguage,
        SelectNextOccurrence,
        GoToDefinition,
        FormatDocument,
        PushToRemote,
        // Git panel toolbar (#64 follow-up). `PushToRemote` already exists (⇧⌥P); these two
        // give the panel's Branch and History buttons an action to fire so they reuse the
        // exact `toggle_palette` paths the palette commands do, rather than reimplementing
        // branch-switch or log. Keyboard-unbound: the palette and its commands are the
        // keyboard route, the buttons are the pointer route to the same handlers.
        SwitchBranch,
        ShowGitLog,
        RenameSymbol,
        QuickFix,
        FoldBlock,
        UnfoldBlock,
        FindReferences,
        NavigateBack,
        NavigateForward,
        OpenSettings,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize,
        // Find and replace (#80). `Find` and `Replace` open the bar; the rest act on it.
        Find,
        Replace,
        FindNext,
        FindPrev,
        ReplaceOne,
        ReplaceAll,
        ToggleCaseSensitive,
        ToggleWholeWord,
        ToggleRegex,
        FocusReplaceField,
        // Test runner (#25). `RunTests` runs the whole suite, `RunTestsInFile` the active
        // tab, and `RerunFailedTests` only what failed last time. All four are no-ops in a
        // project with no test framework, which is the common case (§24).
        ToggleTestPanel,
        RunTests,
        RunTestsInFile,
        RerunFailedTests,
        // Find in project (#80's second half). Opens the Search panel in the sidebar and
        // focuses its query field; pressing it again with the panel already open returns
        // to the file tree, which is what the activity bar's Search icon does too.
        FindInProject,
        // The Xdebug debugger (#30). `ToggleBreakpoint` is the only one that does anything
        // without a session: breakpoints are set before the page is loaded, which is how
        // debugging usually starts. The rest are no-ops unless execution is paused, and the
        // panel shows its controls as unavailable to match (§24).
        ToggleDebugPanel,
        StartDebugging,
        StopDebugging,
        ToggleBreakpoint,
        DebugContinue,
        DebugStepOver,
        DebugStepInto,
        DebugStepOut,
    ]
);

/// Key context names used in keymaps. Constants so a typo is a compile error rather
/// than a binding that silently never fires.
pub mod context {
    pub const WORKSPACE: &str = "Workspace";
    pub const EDITOR: &str = "Editor";
    pub const PALETTE: &str = "Palette";
    pub const TERMINAL: &str = "Terminal";
    /// The find bar (#80). Its own context, not `Palette`: `enter` there means "next
    /// match" rather than "confirm and close", `escape` returns focus to the editor
    /// instead of dismissing an overlay, and the bar has toggles a palette has no
    /// concept of. Sharing the context would have meant a mode check in every handler.
    pub const FIND: &str = "Find";
    /// The preview pane's address bar (#31). Its own context so Enter means "load this
    /// URL" only while the bar has focus, and means whatever it usually means elsewhere.
    pub const PREVIEW: &str = "Preview";
    /// The test results panel (#25). Its own context so a rerun key means "rerun" only
    /// while the panel has focus, and does not shadow anything in the editor.
    pub const TESTS: &str = "Tests";
    /// The debugger panel (#30). Its own context for the test panel's reason, though the
    /// step keys are deliberately bound in `WORKSPACE` rather than here: F5 and F10 must
    /// work while the caret is in the editor, which is where someone stepping through code
    /// actually is. This context exists for keys that only make sense with the panel
    /// focused, and to keep the panel's element consistent with every other one.
    pub const DEBUG: &str = "Debug";
    /// The completion popup (#61). **Its own context is the entire reason arrows are not
    /// stolen from the document**: `up` and `down` are bound here and in `Editor`, and gpui
    /// dispatches to the innermost context that has a binding. With no popup open there is
    /// no element carrying this context, so every arrow reaches the editor exactly as
    /// before — the popup cannot suppress a key it is not on screen for.
    ///
    /// Not `PALETTE`: `escape` there dismisses an overlay and returns focus to the
    /// workspace, where here it must return focus to the *editor* mid-edit, and `tab`
    /// accepts a completion where in the palette it does nothing. Sharing the context would
    /// have meant a mode check at the top of five handlers.
    pub const COMPLETION: &str = "Completion";
    /// The find-in-project panel (#80). Its own context rather than `FIND`: `enter` there
    /// runs the search rather than advancing to a next match, `escape` returns focus to
    /// the editor without closing the panel, and there is no replace field for ⇥ to reach.
    /// Sharing `FIND` would have meant three handlers that begin by asking which one they
    /// are in — the shape a second widget wears when it is pretending to be the first.
    pub const SEARCH_PANEL: &str = "SearchPanel";
    /// The AI chat panel's input (#99). Its own context for the find bar's reasons:
    /// `enter` here sends a message, `escape` cancels a streaming reply, and neither
    /// meaning belongs anywhere else in the window.
    pub const AI_CHAT: &str = "AiChat";
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
        // ⌃⌘F is the macOS-wide fullscreen chord; ⌘K Z is VS Code's zen chord, chosen
        // so the muscle memory transfers.
        KeyBinding::new("ctrl-cmd-f", ToggleFullscreen, Some(context::WORKSPACE)),
        KeyBinding::new("cmd-k z", ToggleZen, Some(context::WORKSPACE)),
        // ⌘⇧A toggles the AI chat panel (#99). Nothing else in this keymap claims the
        // chord, and macOS's symbolic-hotkey table (read for the ⌥⌘I decision above)
        // does not either.
        KeyBinding::new("cmd-shift-a", ToggleAiChat, Some(context::WORKSPACE)),
        // ⌘⇧V, VS Code's markdown-preview chord.
        //
        // This was ⌘⇧P until it was found to be the command palette's chord, fourteen lines
        // above, in the same `WORKSPACE` context. The comment here claimed "⌘⇧P is free
        // because this app puts the command palette on ⌘K" — it does not, and never did.
        // gpui resolves the later binding, so shipping the preview pane silently took the
        // palette away: pressing ⌘⇧P opened a WKWebView. The lesson is not "check twice"
        // but that a keymap this size cannot be checked by reading, which is why
        // `no_duplicate_bindings_within_a_context` now fails the build instead.
        //
        // ⌘⇧V checked the way ⌥⌘I was, against `com.apple.symbolichotkeys` rather than by
        // eye: no system hotkey binds keycode 9 (`v`) under any modifier mask, and nothing
        // in this keymap claims it.
        KeyBinding::new("cmd-shift-v", TogglePreview, Some(context::WORKSPACE)),
        // #82 stage 1: ⌘D grows a cursor per occurrence; Escape (the editor's existing
        // Cancel) collapses back to one.
        KeyBinding::new("cmd-d", SelectNextOccurrence, Some(context::EDITOR)),
        KeyBinding::new("escape", Cancel, Some(context::EDITOR)),
        // Navigation (#81). Workspace-scoped, not editor-scoped: they act on the active
        // tab but the palette they open belongs to the workspace, and ⌘⇧O must still work
        // when focus sits in the tree rather than in the text.
        //
        // F12 and ⇧F12 are the cross-platform IDE convention; ⌘⇧O is VS Code's and
        // PhpStorm's "go to symbol in file". ⌃- / ⌃⇧- are the JetBrains back/forward pair,
        // chosen over ⌘[ / ⌘] because those are already indent and outdent in the editor.
        KeyBinding::new("f12", GoToDefinition, Some(context::WORKSPACE)),
        // ⇧⌥F is VS Code's chord, on the workspace like F12: it acts on the active
        // editor and must not require focus juggling to reach.
        KeyBinding::new("shift-alt-f", FormatDocument, Some(context::WORKSPACE)),
        // ⇧⌥P pushes — the follow-through the commit feedback points at, so the whole
        // commit→push flow is reachable from the keyboard without the palette.
        KeyBinding::new("shift-alt-p", PushToRemote, Some(context::WORKSPACE)),
        // F2 is the rename key everywhere; like F12 it acts on the active editor.
        KeyBinding::new("f2", RenameSymbol, Some(context::WORKSPACE)),
        // ⌘. is VS Code's quick-fix chord and nothing here claims it.
        KeyBinding::new("cmd-.", QuickFix, Some(context::WORKSPACE)),
        // VS Code's fold chords, on the editor: they act on the buffer under the caret.
        KeyBinding::new("alt-cmd-[", FoldBlock, Some(context::EDITOR)),
        KeyBinding::new("alt-cmd-]", UnfoldBlock, Some(context::EDITOR)),
        KeyBinding::new("shift-f12", FindReferences, Some(context::WORKSPACE)),
        KeyBinding::new("cmd-shift-o", GoToSymbol, Some(context::WORKSPACE)),
        // Explicit "complete here". It opened #83's route-name palette until #61; it now
        // opens the popup, which asks the language server *and* Laravel and shows both in
        // one list with their sources marked.
        //
        // # Why this is not ⌃space, which is what every other editor uses
        //
        // **macOS takes ⌃space before the app ever sees it.** It is bound system-wide to
        // "select the previous input source", and on a machine with more than one keyboard
        // layout installed pressing it switches language instead of reaching us. That is not
        // an exotic configuration — it is every user who types in more than one language,
        // which is most users outside the US. The popup shipped in #118 was therefore
        // *unreachable* by keyboard for them.
        //
        // The replacement was checked against the system table rather than guessed, because
        // a chord gpui accepts is not the same as a chord the OS delivers — the same lesson
        // #104 learned checking gpui's own `keymap.rs` for context shadowing before binding
        // ⌘W in the terminal. Reading `com.apple.symbolichotkeys` on the reporting machine,
        // **every** modifier combination on the spacebar is already claimed and enabled:
        //
        // | id  | chord   | macOS uses it for              |
        // | --- | ------- | ------------------------------ |
        // | 60  | ⌃space  | select the previous input source |
        // | 61  | ⌃⌥space | select the next input source   |
        // | 64  | ⌘space  | Spotlight                      |
        // | 65  | ⌥⌘space | Finder search window           |
        // | 156 | ⌃⇧space | the character picker           |
        //
        // ⌥⌘space was the first choice and id 65 rules it out, so the nearest free chord
        // wins instead: ⌥⌘I is absent from that table entirely and unbound anywhere in this
        // keymap. It keeps the ⌥⌘ shape that was asked for and gives up only the mnemonic
        // of the spacebar, which was never available.
        //
        // **Trigger characters are the real answer to this.** Typing `->` opens the popup
        // with no chord at all (see `WorkspaceView::editor_typed`), so completion works for
        // a user whose every spacebar chord is spoken for. This binding is the deliberate
        // invoke on top of that, not the only way in.
        //
        // Still workspace-scoped rather than editor-scoped, matching every other binding
        // that acts on the active tab. It is silent with no tab and no server, which stays
        // the common case (#74).
        KeyBinding::new("cmd-alt-i", Complete, Some(context::WORKSPACE)),
        KeyBinding::new("ctrl--", NavigateBack, Some(context::WORKSPACE)),
        KeyBinding::new("ctrl-shift--", NavigateForward, Some(context::WORKSPACE)),
        // ctrl-` is the conventional terminal toggle. It is bound workspace-wide so it
        // also *closes* the panel while the terminal itself has focus.
        KeyBinding::new("ctrl-`", ToggleTerminal, Some(context::WORKSPACE)),
        KeyBinding::new("ctrl-shift-`", NewTerminal, Some(context::WORKSPACE)),
        //
        // Test runner (#25). Workspace-scoped so they work from the editor and from the
        // panel, and so the toggle also *closes* the panel while it has focus — the same
        // reasoning as the terminal above.
        //
        // ⌃⇧ rather than ⌘: ⌘T and ⌘R are a new tab and a reload in every macOS app the
        // user also has open, and the comment on `ToggleTheme` above declines to claim
        // them for exactly that reason. These four are near the terminal's own ⌃` chord,
        // which is where a runner panel belongs.
        KeyBinding::new("ctrl-shift-t", ToggleTestPanel, Some(context::WORKSPACE)),
        KeyBinding::new("ctrl-shift-r", RunTests, Some(context::WORKSPACE)),
        KeyBinding::new("ctrl-shift-f", RunTestsInFile, Some(context::WORKSPACE)),
        //
        // The debugger (#30). F5/F8/F7/⇧F8 and ⌘F8 are PhpStorm's; F5/F10/F11/⇧F11 and F9
        // are VS Code's. The F-keys chosen here are the ones the two agree on or leave
        // free, so neither audience has to unlearn a reflex: F5 continues in both, and
        // F9 toggles a breakpoint in VS Code while PhpStorm's ⌘F8 is bound alongside it.
        //
        // All of them are `WORKSPACE`-scoped, not `DEBUG`-scoped: someone stepping through
        // code has the caret in the editor, and a step key that only works while the panel
        // has focus is a step key nobody can reach without the mouse.
        KeyBinding::new("f5", DebugContinue, Some(context::WORKSPACE)),
        KeyBinding::new("f10", DebugStepOver, Some(context::WORKSPACE)),
        KeyBinding::new("f11", DebugStepInto, Some(context::WORKSPACE)),
        KeyBinding::new("shift-f11", DebugStepOut, Some(context::WORKSPACE)),
        KeyBinding::new("f9", ToggleBreakpoint, Some(context::WORKSPACE)),
        KeyBinding::new("cmd-f8", ToggleBreakpoint, Some(context::WORKSPACE)),
        KeyBinding::new("ctrl-shift-e", RerunFailedTests, Some(context::WORKSPACE)),
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
        // Find and replace (#80). Workspace-scoped so ⌘F works with the editor, the tree
        // or nothing focused — the bar belongs to the active tab, not to whatever has
        // keyboard focus at the moment. ⌘G is macOS's find-again since long before it was
        // an IDE convention, and it must work with focus back in the editor, which is the
        // whole reason it is not bound in the FIND context.
        KeyBinding::new("cmd-f", Find, Some(context::WORKSPACE)),
        KeyBinding::new("cmd-alt-f", Replace, Some(context::WORKSPACE)),
        KeyBinding::new("cmd-g", FindNext, Some(context::WORKSPACE)),
        KeyBinding::new("cmd-shift-g", FindPrev, Some(context::WORKSPACE)),
        // Inside the bar. `enter` advances rather than confirming-and-closing: the find
        // bar is not a modal, and every editor keeps it open so the next press advances
        // again.
        // The preview address bar (#31): the same three keys a one-field text input needs,
        // scoped to the pane so they are inert anywhere else.
        KeyBinding::new("enter", Confirm, Some(context::PREVIEW)),
        KeyBinding::new("backspace", Backspace, Some(context::PREVIEW)),
        KeyBinding::new("escape", Cancel, Some(context::PREVIEW)),
        KeyBinding::new("escape", Cancel, Some(context::FIND)),
        KeyBinding::new("enter", FindNext, Some(context::FIND)),
        KeyBinding::new("shift-enter", FindPrev, Some(context::FIND)),
        KeyBinding::new("backspace", Backspace, Some(context::FIND)),
        // ⇥ moves between the find and replace fields, which is the only reason the bar
        // needs two focus targets rather than two entities.
        KeyBinding::new("tab", FocusReplaceField, Some(context::FIND)),
        KeyBinding::new("cmd-enter", ReplaceAll, Some(context::FIND)),
        KeyBinding::new("cmd-alt-c", ToggleCaseSensitive, Some(context::FIND)),
        KeyBinding::new("cmd-alt-w", ToggleWholeWord, Some(context::FIND)),
        KeyBinding::new("cmd-alt-r", ToggleRegex, Some(context::FIND)),
        // Find in project. ⌘⇧F is the binding in VS Code, PhpStorm and Sublime alike, and
        // it is workspace-scoped for the same reason ⌘F is: it opens a panel that belongs
        // to the project, not to whatever currently has keyboard focus.
        KeyBinding::new("cmd-shift-f", FindInProject, Some(context::WORKSPACE)),
        // Inside the panel. `enter` runs the search *now* rather than waiting out the
        // debounce — a deliberate "I have finished typing" must not be ignored for another
        // quarter second. `escape` gives focus back to the editor and leaves the results
        // standing, unlike the find bar's escape, which clears: a list that took a second
        // to populate must not vanish on a stray key.
        KeyBinding::new("escape", Cancel, Some(context::SEARCH_PANEL)),
        KeyBinding::new("enter", Confirm, Some(context::SEARCH_PANEL)),
        KeyBinding::new("backspace", Backspace, Some(context::SEARCH_PANEL)),
        KeyBinding::new("cmd-alt-c", ToggleCaseSensitive, Some(context::SEARCH_PANEL)),
        KeyBinding::new("cmd-alt-w", ToggleWholeWord, Some(context::SEARCH_PANEL)),
        KeyBinding::new("cmd-alt-r", ToggleRegex, Some(context::SEARCH_PANEL)),
        //
        // The AI chat input (#99). `enter` sends; `escape` cancels a reply in flight and
        // is otherwise inert — it does not close the panel, because a stray Esc throwing
        // away a conversation is the find-in-project "results must survive" lesson.
        KeyBinding::new("enter", Confirm, Some(context::AI_CHAT)),
        KeyBinding::new("escape", Cancel, Some(context::AI_CHAT)),
        KeyBinding::new("backspace", Backspace, Some(context::AI_CHAT)),
        // ⌃L is what a terminal uses to clear, and this panel is read as one. Scoped to the
        // chat so it cannot shadow anything elsewhere; nothing else in this keymap claims
        // it, and macOS's symbolic-hotkey table does not either (checked as ⌥⌘I was).
        KeyBinding::new("ctrl-l", ClearAiChat, Some(context::AI_CHAT)),
        //
        // The completion popup (#61). These exist *only* while the popup is on screen,
        // because the context comes from the popup's own element — which is what makes
        // "arrows must still move the cursor when nothing is open" true by construction
        // rather than by a guard inside a handler.
        //
        // `tab` as well as `enter` accepts, which is the convention in every IDE and is the
        // key most people actually press. It shadows the editor's `Tab` (indent) only while
        // the popup holds focus.
        KeyBinding::new("escape", Cancel, Some(context::COMPLETION)),
        KeyBinding::new("enter", Confirm, Some(context::COMPLETION)),
        KeyBinding::new("tab", Confirm, Some(context::COMPLETION)),
        KeyBinding::new("down", SelectNext, Some(context::COMPLETION)),
        KeyBinding::new("up", SelectPrev, Some(context::COMPLETION)),
        KeyBinding::new("ctrl-n", SelectNext, Some(context::COMPLETION)),
        KeyBinding::new("ctrl-p", SelectPrev, Some(context::COMPLETION)),
        KeyBinding::new("backspace", Backspace, Some(context::COMPLETION)),
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
        // Partial accepts for the AI ghost (#29, Zed's granularity): word, then line.
        // ⌃Tab is free here (the OS keeps ⌘Tab; in-app tab cycling is ⌘⇧[ ]), and both
        // are no-ops without a visible ghost, so the keys cost nothing when idle.
        KeyBinding::new("ctrl-tab", AcceptPredictionWord, Some(context::EDITOR)),
        KeyBinding::new("ctrl-shift-tab", AcceptPredictionLine, Some(context::EDITOR)),
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
        // ⌘/ is the toggle in both PhpStorm and VS Code. On a US layout `/` is unshifted,
        // so one binding covers it; other layouts reach it through the same physical key
        // because gpui binds the layout-independent label.
        KeyBinding::new("cmd-/", ToggleComment, Some(context::EDITOR)),
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
        // ⌘W closes the *terminal* while the terminal has focus (#97). The panel's div sits
        // inside the workspace's, so this shadows the `CloseTab` above rather than replacing
        // it — with focus anywhere else ⌘W still closes the editor tab.
        //
        // Bound deliberately, and the "it will surprise someone mid-edit" objection is the
        // reason it is safe rather than a reason against: ⌘W only arrives here when the
        // terminal already holds keyboard focus, which takes a click in the panel or ⌃`.
        // Someone editing text has editor focus and is unaffected. The hazard actually worth
        // avoiding is the mirror image — ⌘W silently closing a *file* while the user is
        // looking at a terminal — which is what leaving this unbound would have kept.
        // Destructiveness is handled by the confirm prompt, not by withholding the key.
        KeyBinding::new("cmd-w", CloseTerminal, Some(context::TERMINAL)),
        // ⌘D is the split in iTerm and VS Code's terminal alike.
        KeyBinding::new("cmd-d", SplitTerminal, Some(context::TERMINAL)),
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
    SetLanguage,
    OpenSettingsFile,
    NewFile,
    Save,
    CloseTab,
    QuickOpen,
    Routes,
    CompleteRouteName,
    GoToSymbol,
    GoToDefinition,
    FindReferences,
    NavigateBack,
    NavigateForward,
    Quit,
    NewTerminal,
    ToggleTerminal,
    ToggleTheme,
    ToggleFullscreen,
    ToggleZen,
    ToggleAiChat,
    TogglePreview,
    ToggleHiddenFiles,
    OpenSettings,
    Find,
    Replace,
    ToggleTestPanel,
    RunTests,
    RunTestsInFile,
    RerunFailedTests,
    FindInProject,
    ToggleDebugPanel,
    StartDebugging,
    StopDebugging,
    ToggleBreakpoint,
    Artisan,
    FormatDocument,
    GoToWorkspaceSymbol,
    RenameSymbol,
    QuickFix,
    FoldAll,
    UnfoldAll,
    GitFetch,
    GitPush,
    GitSwitchBranch,
    GitLog,
    ToggleLogPanel,
    ComposerInstall,
    ComposerUpdate,
    ComposerRequire,
    ComposerScript,
    DockerUp,
    DockerStop,
    DockerLogs,
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
        "laravel.route_name" => Dispatch::CompleteRouteName,
        "navigate.symbol" => Dispatch::GoToSymbol,
        "editor.language" => Dispatch::SetLanguage,
        "settings.file" => Dispatch::OpenSettingsFile,
        "navigate.definition" => Dispatch::GoToDefinition,
        "navigate.references" => Dispatch::FindReferences,
        "navigate.back" => Dispatch::NavigateBack,
        "navigate.forward" => Dispatch::NavigateForward,
        "terminal.new" => Dispatch::NewTerminal,
        "terminal.toggle" => Dispatch::ToggleTerminal,
        "theme.toggle" => Dispatch::ToggleTheme,
        "view.fullscreen" => Dispatch::ToggleFullscreen,
        "view.zen" => Dispatch::ToggleZen,
        "ai.chat" => Dispatch::ToggleAiChat,
        "view.preview" => Dispatch::TogglePreview,
        "workspace.toggle_hidden_files" => Dispatch::ToggleHiddenFiles,
        "workspace.open_settings" => Dispatch::OpenSettings,
        "editor.find" => Dispatch::Find,
        "editor.replace" => Dispatch::Replace,
        "tests.toggle" => Dispatch::ToggleTestPanel,
        "tests.run" => Dispatch::RunTests,
        "tests.run_file" => Dispatch::RunTestsInFile,
        "tests.rerun_failed" => Dispatch::RerunFailedTests,
        "editor.find_in_project" => Dispatch::FindInProject,
        "debug.toggle_panel" => Dispatch::ToggleDebugPanel,
        "debug.start" => Dispatch::StartDebugging,
        "debug.stop" => Dispatch::StopDebugging,
        "debug.toggle_breakpoint" => Dispatch::ToggleBreakpoint,
        "laravel.artisan" => Dispatch::Artisan,
        "editor.format" => Dispatch::FormatDocument,
        "navigate.workspace_symbol" => Dispatch::GoToWorkspaceSymbol,
        "editor.rename" => Dispatch::RenameSymbol,
        "editor.quick_fix" => Dispatch::QuickFix,
        "laravel.logs" => Dispatch::ToggleLogPanel,
        "composer.install" => Dispatch::ComposerInstall,
        "composer.update" => Dispatch::ComposerUpdate,
        "composer.require" => Dispatch::ComposerRequire,
        "composer.script" => Dispatch::ComposerScript,
        "docker.up" => Dispatch::DockerUp,
        "docker.stop" => Dispatch::DockerStop,
        "docker.logs" => Dispatch::DockerLogs,
        "git.fetch" => Dispatch::GitFetch,
        "git.push" => Dispatch::GitPush,
        "git.switch_branch" => Dispatch::GitSwitchBranch,
        "git.log" => Dispatch::GitLog,
        "editor.fold_all" => Dispatch::FoldAll,
        "editor.unfold_all" => Dispatch::UnfoldAll,
        // `palette.toggle` is how you got here; re-running it is a no-op by design.
        _ => Dispatch::Unhandled,
    }
}

/// What clipboard text becomes when it lands in a one-line field.
///
/// Every small text field in this app — the palette, the find and replace boxes, the chat
/// box, the commit message, the project-search query, the file-name prompt — is a `String`
/// rendered as a single `div` child. A `\n` in one of those does not wrap: it renders as an
/// invisible break, so the field silently shows less than it holds and the user cannot see
/// why. Anything pasted from a terminal or an editor carries one, usually trailing.
///
/// So newlines and tabs collapse to spaces and the result is trimmed. A free function
/// because this is the whole decision worth testing — the rest of a paste is one
/// `push_str` inside a `Context` that needs a window to build.
pub fn pasted_into_single_line(clipboard: &str) -> String {
    // Every run of whitespace — including the CRLF pair and a tab-indented line — becomes
    // one space, so a pasted block reads as one line of words rather than losing its
    // breaks silently. `split_whitespace` handles the trimming on both ends for free.
    clipboard.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_newline_never_reaches_a_one_line_field() {
        // The reported shape: a key or path copied from a terminal carries a trailing
        // newline, which renders as an invisible break rather than as nothing.
        assert_eq!(pasted_into_single_line("sk-ant-api03-abc\n"), "sk-ant-api03-abc");
        assert_eq!(pasted_into_single_line("first\nsecond"), "first second");
        assert_eq!(pasted_into_single_line("crlf\r\nline"), "crlf line");
    }

    #[test]
    fn tabs_and_runs_of_space_collapse() {
        // Pasting an indented line out of a file must not push the text off the right of a
        // 120px box with whitespace nobody can see.
        assert_eq!(pasted_into_single_line("\tindented"), "indented");
        assert_eq!(pasted_into_single_line("a\t\tb"), "a b");
        assert_eq!(pasted_into_single_line("wide    gap"), "wide gap");
    }

    #[test]
    fn surrounding_space_is_trimmed() {
        assert_eq!(pasted_into_single_line("  padded  "), "padded");
    }

    #[test]
    fn whitespace_only_and_empty_paste_to_nothing() {
        // Guards the callers: an empty result must stay empty rather than become a lone
        // space that makes a placeholder disappear for no visible reason.
        assert_eq!(pasted_into_single_line(""), "");
        assert_eq!(pasted_into_single_line("   \n\t "), "");
    }

    #[test]
    fn ordinary_text_survives_untouched() {
        assert_eq!(pasted_into_single_line("UserController.php"), "UserController.php");
        assert_eq!(pasted_into_single_line("ação — não"), "ação — não");
    }

    /// The shipped body of `init`, which is where every binding is declared.
    ///
    /// Read from source rather than from gpui, because gpui exposes no way to enumerate the
    /// bindings it has been given — `bind_keys` consumes them. The check is textual and that
    /// crudeness is the point, the same argument `tests/theming.rs` makes: the failure mode
    /// is a binding *written* without a context, and that is exactly what this reads.
    fn keymap_source() -> String {
        let source = include_str!("actions.rs");
        let start = source.find("pub fn init(").expect("init must exist");
        let end = source[start..].find("\n    let mut registry").expect("init must end") + start;
        source[start..end].to_string()
    }

    #[test]
    fn no_binding_is_global() {
        // Every `KeyBinding::new` must name a context. A binding with `None` fires no matter
        // what has focus, which for an arrow key means the popup's navigation would move the
        // list while the user is moving the cursor in a document — #61's "must not steal the
        // keymap", stated as something a machine checks rather than something a reviewer
        // remembers.
        let source = keymap_source();
        for line in source.lines() {
            if line.trim_start().starts_with("//") || !line.contains("KeyBinding::new") {
                continue;
            }
            assert!(
                line.contains("Some(context::"),
                "every binding must be scoped to a context, got: {}",
                line.trim()
            );
        }
    }

    #[test]
    fn the_completion_popups_navigation_keys_are_scoped_to_its_own_context() {
        // The specific keys #61 is about. `up`, `down`, `enter`, `tab` and `escape` all mean
        // something else in the editor, and the popup may only claim them inside
        // `context::COMPLETION` — which no element carries unless a popup is on screen.
        //
        // Mutation-checked: binding `down` to `SelectNext` in `context::EDITOR` instead makes
        // this fail, which is the arrangement that would break arrow-key movement.
        let source = keymap_source();
        let completion_bindings: Vec<&str> = source
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//"))
            .filter(|line| line.contains("context::COMPLETION"))
            .collect();

        for key in ["\"up\"", "\"down\"", "\"enter\"", "\"tab\"", "\"escape\""] {
            assert!(
                completion_bindings.iter().any(|line| line.contains(key)),
                "{key} must be bound in the completion context"
            );
        }

        // And the mirror image: none of the popup's actions may be bound anywhere a document
        // has focus. `SelectNext`/`SelectPrev` in the editor context would move a list that
        // is not on screen instead of the cursor.
        for line in source.lines().map(str::trim).filter(|line| !line.starts_with("//")) {
            // The exact tokens, with the delimiter: a substring match flagged
            // `SelectNextOccurrence` (#82's ⌘D) the day it was added, which is a
            // different action that genuinely belongs in the editor context.
            if line.contains("SelectNext,") || line.contains("SelectPrev,") {
                assert!(
                    line.contains("context::COMPLETION") || line.contains("context::PALETTE"),
                    "list navigation must never be bound where the document has focus: {line}"
                );
            }
        }
    }

    #[test]
    fn the_explicit_completion_chord_opens_the_general_popup_not_only_laravel_routes() {
        // #83 bound the explicit chord to `CompleteLaravel` because no popup existed. #61 is
        // the popup, and the key has to reach it — otherwise the feature ships unreachable,
        // which is the kind of thing that passes every other test in this file.
        //
        // Found by action rather than by chord, so retargeting the binding does not silently
        // stop testing anything: the property is "whatever key invokes completion reaches
        // `Complete`", and it holds whichever chord that turns out to be.
        let source = keymap_source();
        let binding = source
            .lines()
            .map(str::trim)
            .find(|line| line.contains("Complete,") && line.contains("context::WORKSPACE"))
            .expect("some chord must invoke completion");

        assert!(
            !binding.contains("CompleteLaravel"),
            "the completion chord must no longer be the route-name-only path: {binding}"
        );
    }

    #[test]
    fn the_completion_chord_is_one_macos_actually_delivers() {
        // The bug this is here to keep fixed: #118 bound ⌃space, which macOS intercepts
        // system-wide as "select the previous input source". On any machine with a second
        // keyboard layout installed — most machines outside the US — the keystroke never
        // reached the app and the popup was unreachable by keyboard.
        //
        // Read from `com.apple.symbolichotkeys` on the reporting machine, *every* modifier
        // combination on the spacebar is claimed and enabled: ⌃space (60), ⌃⌥space (61),
        // ⌘space (64, Spotlight), ⌥⌘space (65, Finder search) and ⌃⇧space (156). So the rule
        // is not "avoid ⌃space", it is **avoid the spacebar entirely** for this action.
        //
        // A list of forbidden chords rather than an assertion about the one we chose: the
        // next person to retarget this needs to be stopped from reaching for the spacebar
        // again, and naming only the current binding would not do that.
        const CLAIMED_BY_MACOS: [&str; 5] =
            ["ctrl-space", "ctrl-alt-space", "cmd-space", "cmd-alt-space", "ctrl-shift-space"];

        let source = keymap_source();
        let binding = source
            .lines()
            .map(str::trim)
            .find(|line| line.contains("Complete,") && line.contains("context::WORKSPACE"))
            .expect("some chord must invoke completion");

        for chord in CLAIMED_BY_MACOS {
            assert!(
                !binding.contains(&format!("\"{chord}\"")),
                "macOS claims {chord} before the app sees it, so binding completion to it \
                 ships the popup unreachable by keyboard: {binding}"
            );
        }
    }

    #[test]
    fn no_duplicate_bindings_within_a_context() {
        // The bug this exists to keep fixed: the preview pane (#31) bound ⌘⇧P in
        // `WORKSPACE`, which the command palette had already claimed fourteen lines above.
        // gpui resolves the later binding, so the palette became unreachable and ⌘⇧P opened
        // a WKWebView instead. It shipped, and the owner reported it as "⌘⇧P agora abre o
        // navegador".
        //
        // Nothing caught it: 1661 tests passed, the build was clean, and the binding's own
        // comment asserted the chord was free. That is the real finding — a keymap with 135
        // bindings is past the size where reading it proves anything, so the check has to be
        // mechanical.
        //
        // Same chord in *different* contexts is correct and common: `enter` confirms in the
        // palette and inserts a newline in the editor. Only a collision within one context
        // is a bug, because only then does one binding silently shadow the other.
        use std::collections::HashMap;

        let source = keymap_source();
        let mut seen: HashMap<(&str, &str), Vec<&str>> = HashMap::new();

        for line in source.lines().map(str::trim) {
            let Some(rest) = line.strip_prefix("KeyBinding::new(\"") else { continue };
            let Some((chord, rest)) = rest.split_once("\", ") else { continue };
            let Some((action, rest)) = rest.split_once(',') else { continue };
            // `Some(context::WORKSPACE)),` -> `WORKSPACE`. Trimming to the identifier rather
            // than stripping known suffixes: the trailing punctuation varies with
            // formatting, and a context key that silently carries `)),` would compare
            // unequal to the same context written differently — a collision this test
            // exists to catch would slip through as two distinct keys.
            let Some(context) = rest
                .split("context::")
                .nth(1)
                .map(|c| c.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_'))
            else {
                continue;
            };
            seen.entry((chord, context)).or_default().push(action.trim());
        }

        assert!(
            seen.len() > 100,
            "the keymap parser matched only {} bindings — it has drifted from the source it reads",
            seen.len()
        );

        let collisions: Vec<_> = seen
            .iter()
            .filter(|(_, actions)| actions.len() > 1)
            .map(|((chord, context), actions)| format!("  {chord} in {context} -> {actions:?}"))
            .collect();

        assert!(
            collisions.is_empty(),
            "one chord cannot mean two things in the same context — the later binding wins \
             and the earlier one silently stops working:\n{}",
            collisions.join("\n")
        );
    }

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
