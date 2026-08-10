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
    Command::new("editor.new_file", "New File"),
    Command::new("editor.save", "Save File"),
    Command::new("editor.close", "Close Tab"),
    Command::new("palette.toggle", "Command Palette"),
    Command::new("palette.quick_open", "Quick Open File"),
    Command::new("terminal.new", "New Terminal"),
    Command::new("terminal.toggle", "Toggle Terminal"),
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
