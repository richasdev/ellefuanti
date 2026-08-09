//! Owns the set of open sessions and which one is active.
//!
//! Separate from [`Session`] so the panel has one thing to hold and one place where
//! "close the active tab and pick a sensible next one" is decided — logic that is easy to
//! get subtly wrong (off-by-one on the last tab) and is unit-tested here rather than
//! re-derived in a render function.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::session::{Session, SessionId, SessionStatus};

/// A set of terminal sessions with one active.
pub struct TerminalManager {
    sessions: Vec<Session>,
    active: usize,
    next_id: u64,
    /// Where new sessions start — the workspace root, when a folder is open.
    cwd: Option<PathBuf>,
    /// Size new sessions are spawned at, kept current by the panel's resize.
    rows: u16,
    cols: u16,
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalManager {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            active: 0,
            next_id: 1,
            cwd: None,
            // A conventional 80x24 until the panel measures itself and resizes.
            rows: 24,
            cols: 80,
        }
    }

    /// Sets the directory new sessions start in. Existing sessions keep their own cwd:
    /// a running shell has its own working directory that this cannot retroactively change.
    pub fn set_cwd(&mut self, cwd: Option<PathBuf>) {
        self.cwd = cwd;
    }

    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active(&self) -> Option<&Session> {
        self.sessions.get(self.active)
    }

    pub fn active_mut(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(self.active)
    }

    pub fn get_mut(&mut self, id: SessionId) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|session| session.id() == id)
    }

    /// Spawns a session and makes it active.
    ///
    /// Blocking (it forks a shell); the caller wraps it. On failure nothing is added and
    /// the error is returned for the panel to display — §24: a shell that will not start
    /// is a message, not a crash.
    pub fn open(&mut self) -> Result<SessionId> {
        self.open_with_shell(None)
    }

    /// `open`, with an explicit program. Tests use it to run a known binary instead of
    /// whatever login shell the machine happens to have.
    pub fn open_with_shell(&mut self, shell: Option<&str>) -> Result<SessionId> {
        let id = SessionId(self.next_id);
        let session = Session::spawn(id, self.cwd.as_deref(), shell, self.rows, self.cols)?;

        // Only commit the id once the spawn succeeded, so a failed attempt does not
        // consume one and leave a gap in the numbering.
        self.next_id += 1;
        self.sessions.push(session);
        self.active = self.sessions.len() - 1;
        Ok(id)
    }

    /// Closes a session, returning whether it existed.
    ///
    /// Dropping the `Session` is the teardown: it kills the shell and joins its threads.
    pub fn close(&mut self, id: SessionId) -> bool {
        let Some(index) = self.sessions.iter().position(|session| session.id() == id) else {
            return false;
        };

        self.sessions.remove(index);

        // Keep the selection on the same *visual* position where possible, and clamp when
        // the last tab was the one closed.
        if self.active > index || self.active >= self.sessions.len() {
            self.active = self.active.saturating_sub(1);
        }
        true
    }

    pub fn close_active(&mut self) -> bool {
        match self.active().map(|session| session.id()) {
            Some(id) => self.close(id),
            None => false,
        }
    }

    /// Activates a session by index. Out-of-range is ignored rather than clamped: it
    /// means a stale click, and silently selecting a neighbour would be surprising.
    pub fn activate(&mut self, index: usize) {
        if index < self.sessions.len() {
            self.active = index;
        }
    }

    pub fn activate_id(&mut self, id: SessionId) {
        if let Some(index) = self.sessions.iter().position(|session| session.id() == id) {
            self.active = index;
        }
    }

    /// Cycles forward, wrapping. The panel's ctrl-tab equivalent.
    pub fn activate_next(&mut self) {
        if !self.sessions.is_empty() {
            self.active = (self.active + 1) % self.sessions.len();
        }
    }

    /// Resizes every session, and records the size for ones opened later.
    ///
    /// All of them, not just the active one: a background session whose grid still thinks
    /// it is 80 columns renders wrapped at the wrong width the moment it is selected.
    pub fn resize_all(&mut self, rows: u16, cols: u16) {
        self.rows = rows.max(1);
        self.cols = cols.max(1);

        for session in &mut self.sessions {
            // One session failing to resize must not stop the others.
            if let Err(err) = session.resize(self.rows, self.cols) {
                tracing::debug!("resizing terminal {:?} failed: {err:#}", session.id());
            }
        }
    }

    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    /// Removes sessions whose shell has exited.
    ///
    /// Not automatic on exit: a command that fails should leave its output on screen to
    /// be read. The panel calls this when the user dismisses a dead session.
    pub fn reap_exited(&mut self) -> usize {
        let before = self.sessions.len();
        self.sessions.retain(|session| session.status().is_running());
        if self.active >= self.sessions.len() {
            self.active = self.sessions.len().saturating_sub(1);
        }
        before - self.sessions.len()
    }

    /// Statuses of every session, for the tab strip's error markers.
    pub fn statuses(&self) -> Vec<(SessionId, SessionStatus)> {
        self.sessions.iter().map(|s| (s.id(), s.status())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manager with `count` sessions running a program that stays alive but does
    /// nothing. `cat` blocks on stdin, so it neither exits nor produces output.
    fn manager_with(count: usize) -> TerminalManager {
        let mut manager = TerminalManager::new();
        for _ in 0..count {
            manager.open_with_shell(Some("cat")).expect("cat should spawn");
        }
        manager
    }

    #[test]
    fn a_new_manager_is_empty() {
        let manager = TerminalManager::new();
        assert!(manager.is_empty());
        assert!(manager.active().is_none());
    }

    #[test]
    fn opening_activates_the_new_session_and_ids_are_unique() {
        let manager = manager_with(3);
        assert_eq!(manager.len(), 3);
        assert_eq!(manager.active_index(), 2);

        let ids: Vec<_> = manager.sessions().iter().map(|s| s.id()).collect();
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "session ids must be unique");
    }

    #[test]
    fn closing_the_last_session_clamps_the_selection() {
        let mut manager = manager_with(2);
        let last = manager.sessions()[1].id();
        assert!(manager.close(last));
        assert_eq!(manager.len(), 1);
        assert_eq!(manager.active_index(), 0);
    }

    #[test]
    fn closing_an_earlier_session_keeps_the_same_one_active() {
        let mut manager = manager_with(3);
        let first = manager.sessions()[0].id();
        let active_id = manager.active().unwrap().id();

        assert!(manager.close(first));
        // The same session is still selected, now at a lower index.
        assert_eq!(manager.active().unwrap().id(), active_id);
        assert_eq!(manager.active_index(), 1);
    }

    #[test]
    fn closing_an_unknown_id_is_a_no_op() {
        let mut manager = manager_with(1);
        assert!(!manager.close(SessionId(9999)));
        assert_eq!(manager.len(), 1);
    }

    #[test]
    fn closing_everything_leaves_no_active_session() {
        let mut manager = manager_with(2);
        assert!(manager.close_active());
        assert!(manager.close_active());
        assert!(!manager.close_active());
        assert!(manager.active().is_none());
    }

    #[test]
    fn activate_ignores_out_of_range_and_next_wraps() {
        let mut manager = manager_with(2);
        manager.activate(0);
        assert_eq!(manager.active_index(), 0);

        manager.activate(99);
        assert_eq!(manager.active_index(), 0, "a stale index must not move the selection");

        manager.activate_next();
        assert_eq!(manager.active_index(), 1);
        manager.activate_next();
        assert_eq!(manager.active_index(), 0, "cycling wraps");
    }

    #[test]
    fn resize_all_applies_to_every_session_and_to_later_ones() {
        let mut manager = manager_with(2);
        manager.resize_all(40, 100);

        for session in manager.sessions() {
            assert_eq!(session.size(), (40, 100));
        }

        // A session opened afterwards inherits the recorded size rather than 80x24.
        manager.open_with_shell(Some("cat")).unwrap();
        assert_eq!(manager.active().unwrap().size(), (40, 100));
    }

    #[test]
    fn a_failed_spawn_adds_nothing_and_reports_the_error() {
        let mut manager = TerminalManager::new();
        let result = manager.open_with_shell(Some("/nonexistent/definitely-not-a-shell"));

        assert!(result.is_err(), "spawning a missing program must fail");
        assert!(manager.is_empty(), "a failed spawn must not leave a session behind");
        // And the manager is still usable afterwards.
        assert!(manager.open_with_shell(Some("cat")).is_ok());
    }

    #[test]
    fn set_cwd_directs_new_sessions() {
        let dir = std::env::temp_dir();
        let mut manager = TerminalManager::new();
        manager.set_cwd(Some(dir.clone()));
        manager.open_with_shell(Some("cat")).unwrap();

        // canonicalize: /tmp is a symlink to /private/tmp on macOS, so the shell's cwd
        // and the requested path differ textually while naming the same directory.
        let actual = manager.active().unwrap().cwd().canonicalize().unwrap();
        assert_eq!(actual, dir.canonicalize().unwrap());
    }
}
