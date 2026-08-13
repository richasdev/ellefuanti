//! Where ellefuanti keeps the things it writes.

use std::path::PathBuf;

/// `~/Library/Application Support/ellefuanti`, the macOS convention, and already where
/// #72 put the file index (`.../ellefuanti/index/`). One helper rather than two: the index
/// hardcoded this string because no settings layer existed to own it, and a second copy
/// would be the point at which the two silently disagree.
///
/// `None` when `HOME` is unset, which on macOS means a launch context so unusual that
/// guessing a path is worse than running on defaults with nothing persisted.
///
/// No `XDG_CONFIG_HOME`: ellefuanti is macOS-only today (§1), and honouring a variable on
/// a platform whose users do not set it is a branch that can only ever be wrong.
pub fn support_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/ellefuanti"))
}

/// `~/Library/Application Support/ellefuanti/settings.json`.
///
/// Beside the index directory rather than inside it: the index is derived data that may be
/// deleted at any time, and someone clearing a cache directory must not lose their
/// configuration with it.
///
/// **Global only. There is no per-project settings file** — no `.ellefuanti/settings.json`
/// beside `.vscode/`. Stated as a decision rather than an omission, because the read path
/// is a merge with precedence or it is not, and bolting one on later touches every
/// accessor. What per-project settings buy is a per-repo override of things this file does
/// not yet hold; what they cost is that every read grows a precedence rule, every write
/// grows a "which file did this come from" question, and the answer has to be right for
/// keys that do not exist yet. When a key arrives that is genuinely per-project — a PHP
/// binary path, a test command — that is the issue in which to pay for it.
pub fn settings_path() -> Option<PathBuf> {
    Some(support_dir()?.join("settings.json"))
}

/// `~/Library/Application Support/ellefuanti/crash.log`.
///
/// Beside `settings.json` because it is written for the same reason: a human will be asked
/// to find it and read it. A crash report inside the index directory is one `rm -rf` of a
/// cache away from being gone exactly when it is needed.
///
/// A file rather than the tracing subscriber, because `detach_from_terminal` points stderr
/// at `/dev/null` — every panic the app has ever hit was written somewhere nobody could
/// read. See `install_panic_logger` in the app crate.
pub fn crash_log_path() -> Option<PathBuf> {
    Some(support_dir()?.join("crash.log"))
}
