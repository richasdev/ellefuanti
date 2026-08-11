//! Command metadata.
//!
//! Every user-visible action has a stable dotted id (`editor.save`). The id is what
//! the command palette searches, what keymaps will name, and what future macros and
//! plugins will reference — so it is defined once, here, away from the UI.
//!
//! ponytail: this registry stores metadata only; *dispatch* rides gpui's own action
//! system in the `app` crate rather than a second dispatcher of our own. Add an
//! `execute` closure here only if a caller ever needs to run a command without a
//! window (a headless CLI, say) — nothing does yet.

use std::fmt;

/// Stable identifier for an action, e.g. `editor.save`.
///
/// `&'static str` because ids are compile-time constants: no allocation on the
/// palette's hot path, and a typo is a build error rather than a silent no-op.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CommandId(pub &'static str);

impl CommandId {
    /// Text before the first dot: `editor.save` -> `editor`. Used to group the palette.
    pub fn namespace(&self) -> &'static str {
        match self.0.find('.') {
            Some(i) => &self.0[..i],
            None => self.0,
        }
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// A command as the palette sees it.
#[derive(Clone, Copy, Debug)]
pub struct Command {
    pub id: CommandId,
    /// Human-readable label, e.g. "Save File".
    pub title: &'static str,
}

impl Command {
    pub const fn new(id: &'static str, title: &'static str) -> Self {
        Self { id: CommandId(id), title }
    }
}

/// All commands known to the running app.
#[derive(Default, Debug)]
pub struct CommandRegistry {
    commands: Vec<Command>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a command. Later registration of the same id replaces the earlier
    /// one, so a keymap or plugin can override a builtin title without a duplicate
    /// entry appearing in the palette.
    pub fn register(&mut self, command: Command) {
        match self.commands.iter_mut().find(|c| c.id == command.id) {
            Some(existing) => *existing = command,
            None => self.commands.push(command),
        }
    }

    pub fn register_all(&mut self, commands: impl IntoIterator<Item = Command>) {
        for command in commands {
            self.register(command);
        }
    }

    pub fn all(&self) -> &[Command] {
        &self.commands
    }

    pub fn get(&self, id: CommandId) -> Option<Command> {
        self.commands.iter().copied().find(|c| c.id == id)
    }

    /// Fuzzy-ish palette search: every character of `query` must appear in order in
    /// the haystack. Results are sorted by match tightness (span of the match), then
    /// title, so an exact prefix beats a scattered match.
    ///
    /// ponytail: subsequence match over a list this size (dozens, not thousands) is
    /// well under a frame. Swap in a real scorer (nucleo/fzf-style) if the registry
    /// ever grows past a few thousand entries or ranking quality complaints start.
    pub fn search(&self, query: &str) -> Vec<Command> {
        if query.trim().is_empty() {
            let mut all = self.commands.clone();
            all.sort_by_key(|c| c.title);
            return all;
        }

        let mut hits: Vec<(usize, Command)> = self
            .commands
            .iter()
            .filter_map(|c| {
                // Match against "title" and the id, so both "save" and "editor.save" work.
                let by_title = subsequence_span(c.title, query);
                let by_id = subsequence_span(c.id.0, query);
                by_title.into_iter().chain(by_id).min().map(|span| (span, *c))
            })
            .collect();

        hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.title.cmp(b.1.title)));
        hits.into_iter().map(|(_, c)| c).collect()
    }
}

/// If every char of `needle` occurs in order within `haystack` (ASCII-case-insensitive),
/// returns the number of haystack chars spanned by the match. Lower is a tighter match.
fn subsequence_span(haystack: &str, needle: &str) -> Option<usize> {
    let mut chars = haystack.char_indices().map(|(i, c)| (i, c.to_ascii_lowercase()));
    let mut first = None;
    let mut last = 0;

    for want in needle.chars().filter(|c| !c.is_whitespace()) {
        let want = want.to_ascii_lowercase();
        let (idx, _) = chars.find(|(_, c)| *c == want)?;
        first.get_or_insert(idx);
        last = idx;
    }

    Some(last - first.unwrap_or(0) + 1)
}

/// Commands shipped with Milestone 1. Later milestones append their own lists
/// (`laravel::COMMANDS`, `git::COMMANDS`) rather than editing this one.
pub const BUILTIN_COMMANDS: &[Command] = &[
    Command::new("workspace.open_folder", "Open Folder…"),
    Command::new("workspace.quit", "Quit"),
    // Had an action and a ⌘⇧. binding since the file tree landed, but no id — so it was
    // reachable by chord and invisible to the palette. #62 needed it in the View menu, and
    // a menu row has to name a command, so the command is what got added.
    Command::new("workspace.toggle_hidden_files", "Toggle Hidden Files"),
    Command::new("workspace.open_settings", "Open Settings"),
    Command::new("editor.new_file", "New File"),
    Command::new("editor.save", "Save File"),
    Command::new("editor.close", "Close Tab"),
    // #80. All three are also chords (⌘F / ⌘⌥F / ⌘⇧F) and all three are here anyway,
    // unlike the #73 motions: find is a thing people look for by name in a new editor, and
    // the Edit menu is where they look.
    //
    // `editor.find_in_project` was deliberately absent while it did not exist, on the
    // grounds that a palette row for it would be a lie. It exists now, so it is here.
    Command::new("editor.find", "Find…"),
    // #19. Formatting is the server's answer; with none running the row does nothing
    // silently, same as every navigation command — see the workspace handler's doc.
    Command::new("editor.format", "Format Document"),
    Command::new("editor.replace", "Replace…"),
    Command::new("editor.find_in_project", "Find in Project…"),
    Command::new("palette.toggle", "Command Palette"),
    Command::new("palette.quick_open", "Quick Open File"),
    Command::new("laravel.routes", "Go to Route…"),
    // #83. Named for what it does rather than "Complete", because it completes exactly one
    // thing and a row promising general completion would be the lie §24 warns about.
    Command::new("laravel.route_name", "Insert Route Name…"),
    // #23. "Artisan Command…", not "Run Artisan…": confirming *types* the command into
    // the terminal for the user to finish and execute — a row promising to run it would
    // be the lie §24 warns about.
    Command::new("laravel.artisan", "Artisan Command…"),
    // Navigation (#81). These have keybindings too — a command id is what makes them
    // findable by someone who does not know the chord, and what lets the Go menu name them.
    Command::new("navigate.symbol", "Go to Symbol in File…"),
    // #19. Palette-only, like ToggleTheme: the obvious chord (⌘T) is one this keymap
    // deliberately declines to claim — see the comment beside the zoom bindings.
    Command::new("navigate.workspace_symbol", "Go to Symbol in Project…"),
    Command::new("editor.rename", "Rename Symbol…"),
    Command::new("editor.quick_fix", "Quick Fix…"),
    // #82. Fold/unfold-at-cursor are chords (⌥⌘[ / ⌥⌘]); the all-variants are the ones
    // worth a palette name.
    // #64 item 5, the safe half: fetch/push touch no working-tree file, and switch
    // refuses a dirty tree outright. Force push and stash stay unbuilt behind the
    // danger note — a force flag that does not exist cannot be run.
    Command::new("git.fetch", "Git: Fetch"),
    Command::new("git.push", "Git: Push"),
    Command::new("git.switch_branch", "Git: Switch Branch…"),
    Command::new("editor.fold_all", "Fold All"),
    Command::new("editor.unfold_all", "Unfold All"),
    // #100: ⌘, is the panel; the file keeps a named door for the people who prefer it.
    Command::new("settings.file", "Open settings.json"),
    // #127. Findable by name for the same reason: the status-bar cell is the affordance,
    // and this is how someone who has not noticed it gets there.
    Command::new("editor.language", "Change Language Mode…"),
    Command::new("navigate.definition", "Go to Definition"),
    Command::new("navigate.references", "Find Usages"),
    Command::new("navigate.back", "Back"),
    Command::new("navigate.forward", "Forward"),
    Command::new("terminal.new", "New Terminal"),
    Command::new("terminal.toggle", "Toggle Terminal"),
    // Test runner (#25). Named for what they run rather than "Test", so a project with no
    // framework shows rows that do nothing rather than rows that promise something absent —
    // the panel says which framework it found, or that it found none.
    Command::new("tests.toggle", "Toggle Test Panel"),
    Command::new("tests.run", "Run All Tests"),
    Command::new("tests.run_file", "Run Tests in Current File"),
    Command::new("tests.rerun_failed", "Rerun Failed Tests"),
    Command::new("theme.toggle", "Switch Theme"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_splits_on_first_dot() {
        assert_eq!(CommandId("editor.save").namespace(), "editor");
        assert_eq!(CommandId("quit").namespace(), "quit");
    }

    #[test]
    fn register_replaces_same_id() {
        let mut r = CommandRegistry::new();
        r.register(Command::new("editor.save", "Save"));
        r.register(Command::new("editor.save", "Save File"));
        assert_eq!(r.all().len(), 1);
        assert_eq!(r.get(CommandId("editor.save")).unwrap().title, "Save File");
    }

    #[test]
    fn empty_query_returns_everything() {
        let mut r = CommandRegistry::new();
        r.register_all(BUILTIN_COMMANDS.iter().copied());
        assert_eq!(r.search("").len(), BUILTIN_COMMANDS.len());
    }

    #[test]
    fn search_matches_title_and_id_and_ranks_tight_matches_first() {
        let mut r = CommandRegistry::new();
        r.register_all(BUILTIN_COMMANDS.iter().copied());

        let by_title = r.search("save");
        assert_eq!(by_title.first().unwrap().id, CommandId("editor.save"));

        let by_id = r.search("editor.cl");
        assert_eq!(by_id.first().unwrap().id, CommandId("editor.close"));

        // Scattered subsequence still matches, but loses to the tighter one.
        let ranked = r.search("qo");
        assert_eq!(ranked.first().unwrap().id, CommandId("palette.quick_open"));

        assert!(r.search("zzzz").is_empty());
    }

    #[test]
    fn search_is_case_insensitive() {
        let mut r = CommandRegistry::new();
        r.register_all(BUILTIN_COMMANDS.iter().copied());
        assert_eq!(r.search("SAVE").first().unwrap().id, CommandId("editor.save"));
    }
}
