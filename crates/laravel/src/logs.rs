//! Parsing Laravel's log file into structured entries (#25).
//!
//! The format is Monolog's line formatter — `[timestamp] env.LEVEL: message` followed
//! by an optional JSON context and a `[stacktrace]` block of `#N /path.php(line): …`
//! frames. Scan-family contract: read the text, report what is there, invent nothing.
//! A line that is not an entry header belongs to the entry above it (multi-line
//! exceptions), and a file that matches nothing yields no entries rather than one
//! garbage entry spanning the file.

use std::path::PathBuf;

/// One log entry, ready for a panel row and a click.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    /// `2026-08-12 03:04:05` — verbatim, no timezone reinterpretation.
    pub timestamp: String,
    /// `ERROR`, `INFO`, … — upper-cased by Monolog already, reported as written.
    pub level: String,
    /// The header line's message, without the JSON context tail.
    pub message: String,
    /// The first stack frame's file and 1-based line, when the entry carries a trace —
    /// the frame the click jumps to. The *first* frame is the throw site, which is the
    /// one worth landing on.
    pub target: Option<(PathBuf, u32)>,
}

/// Parses a Laravel log's text into entries, newest last (file order).
pub fn parse_laravel_log(text: &str) -> Vec<LogEntry> {
    let mut entries: Vec<LogEntry> = Vec::new();

    for line in text.lines() {
        if let Some(entry) = header(line) {
            entries.push(entry);
        } else if let Some(current) = entries.last_mut()
            && current.target.is_none()
            && let Some(frame) = frame(line)
        {
            current.target = Some(frame);
        }
    }
    entries
}

/// The last `max` entries of a log, for the panel (#25).
///
/// A real `laravel.log` reaches megabytes; a log *viewer* wants the recent tail, not the
/// whole history rendered into a `uniform_list`. Parsing the whole text and keeping the
/// tail is O(file) still, but the caller reads only the file's final chunk, so together
/// they bound both the read and the row count — see the panel's use. Kept a separate
/// function so `parse_laravel_log`'s full-file contract (and its tests) are untouched.
pub fn parse_laravel_log_tail(text: &str, max: usize) -> Vec<LogEntry> {
    let mut entries = parse_laravel_log(text);
    if entries.len() > max {
        entries.drain(..entries.len() - max);
    }
    entries
}

/// `[2026-08-12 03:04:05] local.ERROR: message …` → an entry, or `None`.
fn header(line: &str) -> Option<LogEntry> {
    let rest = line.strip_prefix('[')?;
    let (timestamp, rest) = rest.split_once(']')?;
    // Timestamps are date-time shaped; a `[stacktrace]` marker or a `#0 [internal]`
    // is not. Cheap shape check: two spaces-separated parts, first starts with a digit.
    if !timestamp.chars().next().is_some_and(|c| c.is_ascii_digit()) || !timestamp.contains(' ') {
        return None;
    }
    let rest = rest.trim_start();
    let (channel_level, message) = rest.split_once(':')?;
    let level = channel_level.rsplit('.').next()?.trim();
    if level.is_empty() || !level.chars().all(|c| c.is_ascii_uppercase()) {
        return None;
    }
    // The JSON context tail (` {"exception":…`) is noise in a one-line row; the panel
    // shows the human half. The brace search is from the right shape: Monolog appends
    // context after the message, space-brace.
    let message = message.trim();
    let message = match message.find(" {\"") {
        Some(at) => &message[..at],
        None => message,
    };
    Some(LogEntry {
        timestamp: timestamp.to_string(),
        level: level.to_string(),
        message: message.to_string(),
        target: None,
    })
}

/// `#0 /var/www/app/File.php(42): App\…` → the frame's path and line.
fn frame(line: &str) -> Option<(PathBuf, u32)> {
    let rest = line.trim_start().strip_prefix('#')?;
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit()).trim_start();
    // The FIRST paren: `/path/File.php(42): App\Class->method()` has a second pair on
    // the call, and rfind would try to parse `method()`'s empty interior as the line.
    let open = rest.find('(')?;
    let close = rest[open..].find(')')? + open;
    let line_number: u32 = rest[open + 1..close].parse().ok()?;
    let path = PathBuf::from(&rest[..open]);
    // `[internal function]` and `{main}` frames have no path; a frame worth jumping to
    // is absolute and names a real-looking file.
    path.is_absolute().then_some((path, line_number))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = r#"[2026-08-12 03:04:05] local.ERROR: Undefined variable $x {"exception":"[object] (ErrorException(code: 0): Undefined variable $x at /var/www/app/Http/Controllers/UserController.php:42)
[stacktrace]
#0 [internal function]: Illuminate\Foundation\Bootstrap\HandleExceptions->handleError()
#1 /var/www/app/Http/Controllers/UserController.php(42): show()
#2 /var/www/vendor/laravel/framework/src/Router.php(700): call()
"}
[2026-08-12 03:05:00] local.INFO: User logged in
[2026-08-12 03:06:00] production.WARNING: Slow query: 2s
"#;

    #[test]
    fn entries_split_on_headers_and_keep_their_shape() {
        let entries = parse_laravel_log(LOG);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].level, "ERROR");
        assert_eq!(entries[0].message, "Undefined variable $x", "the JSON tail is cut");
        assert_eq!(entries[1].level, "INFO");
        assert_eq!(entries[1].message, "User logged in");
        assert_eq!(entries[2].level, "WARNING");
        assert_eq!(entries[2].timestamp, "2026-08-12 03:06:00");
    }

    #[test]
    fn the_click_target_is_the_first_real_frame_not_the_internal_one() {
        let entries = parse_laravel_log(LOG);
        assert_eq!(
            entries[0].target,
            Some((PathBuf::from("/var/www/app/Http/Controllers/UserController.php"), 42)),
            "frame #0 is [internal function] and must be skipped for the throw site"
        );
        assert_eq!(entries[1].target, None, "an INFO line has no trace and claims none");
    }

    #[test]
    fn the_tail_keeps_the_most_recent_entries() {
        let mut log = String::new();
        for i in 0..10 {
            log.push_str(&format!("[2026-08-12 03:0{i}:00] local.INFO: entry {i}\n"));
        }
        let tail = parse_laravel_log_tail(&log, 3);
        assert_eq!(tail.len(), 3, "capped to the last three");
        assert_eq!(tail[0].message, "entry 7", "and they are the LAST three, in order");
        assert_eq!(tail[2].message, "entry 9");
        // A short log is returned whole.
        assert_eq!(parse_laravel_log_tail(&log, 100).len(), 10);
    }

    #[test]
    fn garbage_and_stacktrace_markers_are_not_entries() {
        assert!(parse_laravel_log("not a log\n[stacktrace]\n#0 x\n").is_empty());
    }
}
