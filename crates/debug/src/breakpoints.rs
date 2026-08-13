//! Breakpoints the user set, independent of whether anything is running.
//!
//! # Why this is not just a field on `Session`
//!
//! Breakpoints outlive sessions, and by a lot. The user sets one, loads a page, hits it,
//! finishes; the session ends and the *next* page load is a whole new session that must
//! stop at the same line. A session that owned the list would forget every breakpoint on
//! every request — the single most obviously broken thing a debugger can do.
//!
//! So the store is the source of truth and the session is a projection of it. Setting a
//! breakpoint while nothing is connected is normal, not an error: it is how debugging
//! usually starts, since the user marks a line *before* loading the page.
//!
//! # Xdebug's ids are per session, ours are not
//!
//! `breakpoint_set` returns an id we need to remove it later, but that id belongs to one
//! connection and means nothing to the next. [`Breakpoint::engine_id`] is therefore
//! cleared whenever a session ends, and re-registering the whole store is part of
//! starting a new one. Keeping a stale id would produce a `breakpoint_remove` naming
//! something the current engine has never heard of.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One line breakpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Breakpoint {
    /// 0-based, matching the editor's rows. Converted to the protocol's 1-based line at
    /// the boundary, which is the single place that conversion is allowed to happen.
    pub row: usize,
    /// Whether the engine currently knows about it. `None` when nothing is connected, or
    /// when the engine refused it.
    pub engine_id: Option<String>,
}

/// Every breakpoint in the project, by file.
///
/// A `BTreeMap` keyed by path, with rows in a sorted set, so iteration is deterministic:
/// the gutter and the breakpoint panel render in a stable order rather than reshuffling
/// as a `HashMap` rehashes.
#[derive(Debug, Default)]
pub struct BreakpointStore {
    files: BTreeMap<PathBuf, Vec<Breakpoint>>,
}

impl BreakpointStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Toggles the breakpoint on a row, returning whether one is now set.
    ///
    /// Toggle rather than separate add/remove because that is what clicking a gutter is.
    pub fn toggle(&mut self, file: &Path, row: usize) -> bool {
        let rows = self.files.entry(file.to_path_buf()).or_default();
        if let Some(index) = rows.iter().position(|breakpoint| breakpoint.row == row) {
            rows.remove(index);
            if rows.is_empty() {
                // Otherwise a file the user cleared keeps an empty entry forever and
                // `files()` reports it as debugged.
                self.files.remove(file);
            }
            false
        } else {
            rows.push(Breakpoint { row, engine_id: None });
            rows.sort_by_key(|breakpoint| breakpoint.row);
            true
        }
    }

    /// Whether a row carries a breakpoint. The gutter asks this per visible row, so it is
    /// a lookup over one file's short list rather than a scan of the project.
    pub fn is_set(&self, file: &Path, row: usize) -> bool {
        self.rows(file).any(|breakpoint| breakpoint.row == row)
    }

    /// This file's breakpoints, in row order.
    pub fn rows(&self, file: &Path) -> impl Iterator<Item = &Breakpoint> {
        self.files.get(file).into_iter().flatten()
    }

    /// Every file that has at least one breakpoint.
    pub fn files(&self) -> impl Iterator<Item = (&PathBuf, &Vec<Breakpoint>)> {
        self.files.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn len(&self) -> usize {
        self.files.values().map(Vec::len).sum()
    }

    /// Records the id the engine assigned, so the breakpoint can be removed later.
    pub fn bind(&mut self, file: &Path, row: usize, engine_id: String) {
        if let Some(breakpoint) = self
            .files
            .get_mut(file)
            .and_then(|rows| rows.iter_mut().find(|breakpoint| breakpoint.row == row))
        {
            breakpoint.engine_id = Some(engine_id);
        }
    }

    /// The engine id for a row, if it is currently registered.
    pub fn engine_id(&self, file: &Path, row: usize) -> Option<&str> {
        self.rows(file)
            .find(|breakpoint| breakpoint.row == row)
            .and_then(|breakpoint| breakpoint.engine_id.as_deref())
    }

    /// Forgets every engine id, keeping the breakpoints themselves.
    ///
    /// Called when a session ends. The ids belonged to that connection; the user's
    /// breakpoints did not.
    pub fn unbind_all(&mut self) {
        for rows in self.files.values_mut() {
            for breakpoint in rows.iter_mut() {
                breakpoint.engine_id = None;
            }
        }
    }

    /// Shifts breakpoints to follow an edit that inserted or removed whole lines.
    ///
    /// Without this a breakpoint marks a line number rather than a line: add an import at
    /// the top of a file and every breakpoint below it now points one line short of where
    /// the user put it. `delta` is the change in line count at `from_row`.
    ///
    /// Breakpoints *inside* a deleted range are dropped rather than collapsed onto the
    /// edit's first row, which would silently pile several onto one line.
    pub fn shift(&mut self, file: &Path, from_row: usize, delta: isize) {
        let Some(rows) = self.files.get_mut(file) else {
            return;
        };

        if delta < 0 {
            let removed = delta.unsigned_abs();
            rows.retain(|breakpoint| {
                breakpoint.row < from_row || breakpoint.row >= from_row + removed
            });
        }

        for breakpoint in rows.iter_mut() {
            if breakpoint.row >= from_row {
                breakpoint.row = breakpoint.row.saturating_add_signed(delta);
            }
        }

        rows.sort_by_key(|breakpoint| breakpoint.row);
        if rows.is_empty() {
            self.files.remove(file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> PathBuf {
        PathBuf::from("/srv/app/index.php")
    }

    #[test]
    fn toggling_sets_then_clears() {
        let mut store = BreakpointStore::new();
        assert!(store.toggle(&file(), 23));
        assert!(store.is_set(&file(), 23));
        assert_eq!(store.len(), 1);

        assert!(!store.toggle(&file(), 23));
        assert!(!store.is_set(&file(), 23));
        assert!(store.is_empty(), "a cleared file must not linger as an empty entry");
    }

    #[test]
    fn breakpoints_are_kept_in_row_order() {
        // The gutter and the panel both iterate this; an unsorted list reorders itself
        // visibly as breakpoints are added.
        let mut store = BreakpointStore::new();
        for row in [40, 12, 7, 25] {
            store.toggle(&file(), row);
        }
        let rows: Vec<usize> = store.rows(&file()).map(|breakpoint| breakpoint.row).collect();
        assert_eq!(rows, vec![7, 12, 25, 40]);
    }

    #[test]
    fn engine_ids_are_forgotten_when_a_session_ends_but_breakpoints_are_not() {
        // The rule this crate exists to get right: the user's breakpoints outlive the
        // request, the engine's ids do not.
        let mut store = BreakpointStore::new();
        store.toggle(&file(), 10);
        store.bind(&file(), 10, "990001".to_string());
        assert_eq!(store.engine_id(&file(), 10), Some("990001"));

        store.unbind_all();
        assert!(store.is_set(&file(), 10), "the breakpoint survives the session");
        assert_eq!(store.engine_id(&file(), 10), None, "its id does not");
    }

    #[test]
    fn inserting_lines_above_a_breakpoint_moves_it_down() {
        // Add a `use` statement at the top and the breakpoint must still mark the same
        // line of code, not the same line number.
        let mut store = BreakpointStore::new();
        store.toggle(&file(), 20);
        store.toggle(&file(), 5);

        store.shift(&file(), 3, 2);

        let rows: Vec<usize> = store.rows(&file()).map(|breakpoint| breakpoint.row).collect();
        assert_eq!(rows, vec![7, 22]);
    }

    #[test]
    fn editing_below_a_breakpoint_leaves_it_alone() {
        let mut store = BreakpointStore::new();
        store.toggle(&file(), 5);
        store.shift(&file(), 40, 10);
        assert!(store.is_set(&file(), 5));
    }

    #[test]
    fn deleting_the_lines_a_breakpoint_sits_on_removes_it() {
        // Collapsing them onto the edit row instead would stack three breakpoints on one
        // line and stop the script three times where the user asked once.
        let mut store = BreakpointStore::new();
        for row in [10, 11, 12, 20] {
            store.toggle(&file(), row);
        }

        // Rows 10..13 deleted.
        store.shift(&file(), 10, -3);

        let rows: Vec<usize> = store.rows(&file()).map(|breakpoint| breakpoint.row).collect();
        assert_eq!(rows, vec![17], "only the one below the deletion survives, moved up");
    }

    #[test]
    fn a_file_whose_breakpoints_are_all_deleted_stops_being_listed() {
        let mut store = BreakpointStore::new();
        store.toggle(&file(), 4);
        store.shift(&file(), 0, -10);
        assert!(store.is_empty());
        assert_eq!(store.files().count(), 0);
    }

    #[test]
    fn breakpoints_in_different_files_do_not_interfere() {
        let mut store = BreakpointStore::new();
        let other = PathBuf::from("/srv/app/User.php");
        store.toggle(&file(), 10);
        store.toggle(&other, 10);

        store.shift(&file(), 0, 5);

        assert!(store.is_set(&file(), 15));
        assert!(store.is_set(&other, 10), "an edit in one file must not move another's");
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn files_are_listed_in_a_stable_order() {
        // A HashMap here would reshuffle the breakpoint panel between frames.
        let mut store = BreakpointStore::new();
        for name in ["/srv/z.php", "/srv/a.php", "/srv/m.php"] {
            store.toggle(Path::new(name), 1);
        }
        let names: Vec<String> =
            store.files().map(|(path, _)| path.display().to_string()).collect();
        assert_eq!(names, vec!["/srv/a.php", "/srv/m.php", "/srv/z.php"]);
    }
}
