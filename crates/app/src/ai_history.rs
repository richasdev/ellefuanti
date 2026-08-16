//! Chat history that survives an app restart.
//!
//! One JSON file next to `settings.json` — `chat_history.json` — holding the last
//! [`HISTORY_CAP`] turns. Written atomically (a temp file renamed into place) so a crash
//! mid-write can never leave half a conversation; read leniently (a missing or corrupt
//! file is an empty history, logged, never a panic) because history is a convenience,
//! not data the app may refuse to start over.
//!
//! What is *not* stored: proposals, approvals, attachments — they reference live state
//! (a CLI child, request ids, files on disk) that does not exist after a restart. For an
//! HTTP provider the restored turns genuinely restore context, because every send
//! re-sends the conversation; for Codex the thread lives inside the CLI process, so the
//! restored text is readable history and the agent starts fresh — the panel says so.

use crate::ai_chat::{ChatTurn, FlowBlock, Role};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Turns kept across restarts. Enough for days of use; bounded so the file and the
/// panel's first paint stay small.
pub const HISTORY_CAP: usize = 200;

#[derive(Serialize, Deserialize)]
enum StoredRole {
    User,
    Assistant,
    Note,
}

#[derive(Serialize, Deserialize)]
enum StoredFlow {
    Text(String),
    Activity { label: String, done: bool },
}

#[derive(Serialize, Deserialize)]
struct StoredTurn {
    role: StoredRole,
    text: String,
    flow: Vec<StoredFlow>,
}

impl From<&ChatTurn> for StoredTurn {
    fn from(turn: &ChatTurn) -> Self {
        StoredTurn {
            role: match turn.role {
                Role::User => StoredRole::User,
                Role::Assistant => StoredRole::Assistant,
                Role::Note => StoredRole::Note,
            },
            text: turn.text.clone(),
            flow: turn
                .flow
                .iter()
                .map(|block| match block {
                    FlowBlock::Text(text) => StoredFlow::Text(text.clone()),
                    FlowBlock::Activity { label, done } => {
                        StoredFlow::Activity { label: label.clone(), done: *done }
                    }
                })
                .collect(),
        }
    }
}

impl From<StoredTurn> for ChatTurn {
    fn from(turn: StoredTurn) -> Self {
        ChatTurn {
            role: match turn.role {
                StoredRole::User => Role::User,
                StoredRole::Assistant => Role::Assistant,
                StoredRole::Note => Role::Note,
            },
            text: turn.text,
            flow: turn
                .flow
                .into_iter()
                .map(|block| match block {
                    StoredFlow::Text(text) => FlowBlock::Text(text),
                    StoredFlow::Activity { label, done } => FlowBlock::Activity { label, done },
                })
                .collect(),
        }
    }
}

/// `~/Library/Application Support/ellefuanti/chat_history.json`.
pub fn history_path() -> Option<PathBuf> {
    Some(elle_settings::support_dir()?.join("chat_history.json"))
}

/// Saves the last [`HISTORY_CAP`] turns, atomically.
pub fn save_to(path: &Path, turns: &[ChatTurn]) -> Result<(), String> {
    let start = turns.len().saturating_sub(HISTORY_CAP);
    let stored: Vec<StoredTurn> = turns[start..].iter().map(StoredTurn::from).collect();
    let json = serde_json::to_string(&stored).map_err(|err| err.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| err.to_string())?;
    }
    // Write-then-rename: the rename is atomic on APFS, so a reader (or a crash) sees
    // either the old file or the new one, never a torn one.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|err| err.to_string())?;
    std::fs::rename(&tmp, path).map_err(|err| err.to_string())
}

/// Loads whatever is there; anything wrong is an empty history and a log line.
pub fn load_from(path: &Path) -> Vec<ChatTurn> {
    let Ok(json) = std::fs::read_to_string(path) else {
        return Vec::new(); // no file yet: the ordinary first run
    };
    match serde_json::from_str::<Vec<StoredTurn>>(&json) {
        Ok(stored) => stored.into_iter().map(ChatTurn::from).collect(),
        Err(err) => {
            tracing::debug!("chat history unreadable, starting empty: {err}");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(role: Role, text: &str) -> ChatTurn {
        ChatTurn { role, text: text.to_string(), flow: Vec::new() }
    }

    #[test]
    fn a_conversation_round_trips_with_its_flow() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("chat_history.json");
        let turns = vec![
            turn(Role::User, "muda a rota"),
            ChatTurn {
                role: Role::Assistant,
                text: "Feito — ação concluída.".to_string(),
                flow: vec![
                    FlowBlock::Text("Feito — ".to_string()),
                    FlowBlock::Activity { label: "Applied web.php (+3 −1)".to_string(), done: true },
                    FlowBlock::Text("ação concluída.".to_string()),
                ],
            },
            turn(Role::Note, "overloaded"),
        ];

        save_to(&path, &turns).expect("save");
        let loaded = load_from(&path);
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].role, Role::User);
        assert_eq!(loaded[1].text, "Feito — ação concluída.");
        assert_eq!(loaded[1].flow.len(), 3);
        assert!(matches!(
            &loaded[1].flow[1],
            FlowBlock::Activity { label, done: true } if label == "Applied web.php (+3 −1)"
        ));
        assert_eq!(loaded[2].role, Role::Note);
    }

    #[test]
    fn a_corrupt_file_is_an_empty_history_not_a_panic() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("chat_history.json");
        std::fs::write(&path, "{not json at all").expect("write");
        assert!(load_from(&path).is_empty());
        // And a missing file is the same.
        assert!(load_from(&dir.path().join("nope.json")).is_empty());
    }

    #[test]
    fn the_cap_keeps_the_newest_turns() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("chat_history.json");
        let turns: Vec<ChatTurn> =
            (0..HISTORY_CAP + 50).map(|i| turn(Role::User, &format!("turn {i}"))).collect();
        save_to(&path, &turns).expect("save");
        let loaded = load_from(&path);
        assert_eq!(loaded.len(), HISTORY_CAP);
        assert_eq!(loaded[0].text, "turn 50", "the oldest turns are the ones dropped");
        assert_eq!(loaded.last().unwrap().text, format!("turn {}", HISTORY_CAP + 49));
    }

    #[test]
    fn saving_replaces_rather_than_appends() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("chat_history.json");
        save_to(&path, &[turn(Role::User, "first")]).expect("save");
        save_to(&path, &[turn(Role::User, "second")]).expect("save");
        let loaded = load_from(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].text, "second");
    }
}
