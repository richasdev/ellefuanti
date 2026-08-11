//! Running a language server for the open project, and collecting its diagnostics.
//!
//! # What this is for
//!
//! §24, stated as a requirement rather than a hope: **a broken LSP cannot stop you
//! typing.** Everything in this file is arranged around the four ways a language server
//! disappoints you, and none of them may reach the editor:
//!
//! - **Never installed.** The overwhelmingly common case — nobody has Intelephense on a
//!   fresh machine, and most folders anyone opens are not PHP projects at all. This must
//!   be *silent*: no dialog, no status-bar error, no retry. See [`Lsp::start`].
//! - **Slow to start.** Indexing a large `vendor/` tree takes tens of seconds. The
//!   handshake runs on the background executor and nothing waits on it, so the editor is
//!   usable from the first frame (ADR-0007).
//! - **Crashes mid-session.** The editor keeps working, and the server is restarted a
//!   bounded number of times. See [`MAX_RESTARTS`] for why bounded and not "always".
//! - **Outlives the app.** Dropping [`Lsp`] drops the `Client`, whose `Drop` sends
//!   shutdown/exit and then kills the child. A leaked Intelephense is a gigabyte of RAM
//!   and a busy core on someone's laptop after they quit.
//!
//! # Where the server command comes from
//!
//! There is no settings layer yet (#60), so the command is a compiled-in default plus an
//! environment override, and nothing else:
//!
//! ```text
//! ELLE_LSP_COMMAND="phpactor language-server"   # whitespace-split; first word is the binary
//! ELLE_LSP_COMMAND=""                           # disables the LSP entirely
//! ```
//!
//! [`DEFAULT_SERVER`] is what runs when the variable is unset. That default names a
//! specific backend, which is allowed *here* and forbidden one layer down: RISKS.md #2
//! says no backend-specific behaviour may leak past the client boundary, and
//! `crates/lsp/tests/substitutability.rs` enforces that against `crates/lsp` only. A
//! default is data this crate hands to a `ServerConfig`, not a branch — nothing below
//! knows which server it is talking to, and `substitutability.rs`'s companion test in
//! this crate pins that this file never branches on the name.
//!
//! ponytail: this is deliberately not a settings system. When #60 lands, the env lookup
//! becomes a settings read and everything else here is unchanged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use elle_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, DocumentSymbol, DocumentSymbolResponse, Uri,
};
use elle_lsp::{Client, ServerConfig, path_to_uri};

/// The server started when `ELLE_LSP_COMMAND` is unset.
///
/// Intelephense is the default because it is what a Laravel developer already has, and
/// `--stdio` is how it speaks LSP. If it is not on `PATH` the spawn fails and the editor
/// carries on with syntax highlighting only, which is the case this file exists to make
/// uneventful.
pub const DEFAULT_SERVER: &str = "intelephense --stdio";

/// Environment override for the server command.
pub const COMMAND_VAR: &str = "ELLE_LSP_COMMAND";

/// The language id this session sends, and the extension it routes.
///
/// PHP is named here rather than in `crates/lsp` on purpose: the client is generic and
/// takes the id as data (RISKS.md #2), and *something* has to decide that a `.php` file
/// is PHP. This crate is the right place — it is the one that knows what the product is.
const LANGUAGE_ID: &str = "php";

/// How many times a crashed server is restarted before giving up for the session.
///
/// Bounded because a server that dies on startup dies *fast*, and an unbounded restart is
/// then a fork bomb that makes the editor unusable — the exact opposite of §24. Three is
/// enough to ride out a one-off crash and few enough that a genuinely broken install
/// settles into "no LSP" within a second or two instead of respawning forever.
const MAX_RESTARTS: usize = 3;

/// How often the workspace checks for notifications the server has pushed.
///
/// Diagnostics are not a typing-latency feature — they arrive when the server has finished
/// thinking, which is tens or hundreds of milliseconds after a change at best. 250ms of
/// additional delay is imperceptible against that, and it keeps an idle project to four
/// wakeups a second rather than sixty.
///
/// See `WorkspaceView::poll_lsp` for why this is a timer at all rather than a blocking
/// wait on the reader thread — the short version is that the client lives in the view, and
/// reaching into the view happens on the main thread.
pub const LSP_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// What the app knows about one file's diagnostics.
///
/// A `Vec<Diagnostic>` straight from the server, plus the byte ranges resolved against
/// *our* copy of the text. The resolution happens once, when the notification arrives,
/// rather than per frame: the editor renders these on every repaint, and converting
/// UTF-16 positions to byte offsets in a render pass would put a line-index build on the
/// frame budget for no reason.
#[derive(Clone, Debug, Default)]
pub struct FileDiagnostics {
    pub items: Vec<ResolvedDiagnostic>,
}

impl FileDiagnostics {
    /// The diagnostic under a byte offset, if any.
    ///
    /// What makes a squiggle readable without a hover card or a problems panel: put the
    /// cursor on it and the status bar says what it is. An underline the user cannot read
    /// tells them only that something is wrong somewhere, which is close to useless.
    ///
    /// Innermost wins — the shortest range containing the offset. Servers routinely report
    /// a broad "this expression is invalid" over a precise "undefined variable $x", and the
    /// precise one is the one worth reading.
    pub fn at(&self, offset: usize) -> Option<&ResolvedDiagnostic> {
        self.items
            .iter()
            // Inclusive of the end so a cursor just past the last character of a
            // one-character problem still reads it; that is where the cursor lands after
            // typing the thing the server is complaining about.
            .filter(|d| d.range.start <= offset && offset <= d.range.end)
            .min_by_key(|d| d.range.end - d.range.start)
    }

    /// The first diagnostic overlapping a byte range — the cursor's line, in practice.
    ///
    /// The line-level fallback behind the status bar's message. `at` answers "what is under
    /// the cursor"; this answers "what is wrong on this line", which is the question a
    /// click on a marked line is actually asking. First rather than innermost: a line with
    /// two problems shows one of them, and showing *a* reason beats a rule for choosing
    /// between reasons nobody has needed yet.
    pub fn on_line(&self, line: std::ops::Range<usize>) -> Option<&ResolvedDiagnostic> {
        self.items.iter().find(|d| d.range.start < line.end && line.start <= d.range.end)
    }

    /// Counts by severity, for the status bar: `(errors, warnings)`.
    ///
    /// Hints and information are counted as neither. They are advisory, and a status bar
    /// that says "3 problems" for three unused-import hints trains people to ignore it.
    pub fn counts(&self) -> (usize, usize) {
        let errors = self.items.iter().filter(|d| d.severity == Severity::Error).count();
        let warnings = self.items.iter().filter(|d| d.severity == Severity::Warning).count();
        (errors, warnings)
    }
}

/// A diagnostic with its range already in byte offsets.
#[derive(Clone, Debug)]
pub struct ResolvedDiagnostic {
    pub range: std::ops::Range<usize>,
    pub severity: Severity,
    pub message: String,
}

/// Severity, reduced to what the UI distinguishes.
///
/// Not `lsp_types::DiagnosticSeverity` because that is an open integer newtype with no
/// exhaustive match, and the renderer needs one colour per variant with no fallthrough
/// that silently paints an unknown severity as an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

impl Severity {
    fn from_lsp(severity: Option<DiagnosticSeverity>) -> Self {
        match severity {
            Some(DiagnosticSeverity::ERROR) => Self::Error,
            Some(DiagnosticSeverity::WARNING) => Self::Warning,
            Some(DiagnosticSeverity::INFORMATION) => Self::Information,
            Some(DiagnosticSeverity::HINT) => Self::Hint,
            // The specification says an omitted severity is the client's choice. Error is
            // the safe reading: under-reporting a real problem is worse than over-colouring
            // an advisory one.
            _ => Self::Error,
        }
    }
}

/// Why there is no language server running.
///
/// Kept apart from "running" so the status bar can stay *silent* for the ordinary case and
/// speak only when something actually broke. A missing binary is not an error the user
/// needs told about; a server that crashed three times is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LspState {
    /// No attempt yet, or no folder open.
    Idle,
    /// Starting. Nothing waits on this.
    Starting,
    Running,
    /// The binary is not installed, or `ELLE_LSP_COMMAND` is empty. **Silent.**
    Unavailable,
    /// It ran and then died more times than [`MAX_RESTARTS`] allows.
    Failed(String),
}

/// The language server for the open project, if there is one.
pub struct Lsp {
    client: Option<Client>,
    state: LspState,
    root: Option<PathBuf>,
    /// Restarts used so far, reset when a new folder is opened.
    restarts: usize,
    /// Diagnostics by document URI. Cleared when the server goes away, because stale
    /// squiggles from a dead server are worse than none: they look current.
    diagnostics: HashMap<Uri, FileDiagnostics>,
}

impl Default for Lsp {
    fn default() -> Self {
        Self::new()
    }
}

impl Lsp {
    pub fn new() -> Self {
        Self {
            client: None,
            state: LspState::Idle,
            root: None,
            restarts: 0,
            diagnostics: HashMap::new(),
        }
    }

    pub fn state(&self) -> &LspState {
        &self.state
    }

    pub fn set_state(&mut self, state: LspState) {
        self.state = state;
    }

    pub fn is_running(&self) -> bool {
        self.client.as_ref().is_some_and(|c| c.is_alive())
    }

    pub fn restarts(&self) -> usize {
        self.restarts
    }

    /// Whether another restart is allowed. See [`MAX_RESTARTS`].
    pub fn may_restart(&self) -> bool {
        self.restarts < MAX_RESTARTS
    }

    pub fn record_restart(&mut self) {
        self.restarts += 1;
    }

    /// Adopts a freshly started client.
    pub fn adopt(&mut self, client: Client) {
        self.state = LspState::Running;
        self.client = Some(client);
    }

    /// Drops the client and forgets its diagnostics.
    ///
    /// Dropping is what stops the process: `Client::drop` runs shutdown/exit and then
    /// kills the child. Clearing the diagnostics matters just as much — squiggles that
    /// outlive the server that produced them are indistinguishable from current ones.
    pub fn shut_down(&mut self) {
        self.client = None;
        self.diagnostics.clear();
    }

    /// Points at a new project, discarding whatever was running for the old one.
    pub fn set_root(&mut self, root: Option<PathBuf>) {
        self.shut_down();
        self.root = root;
        self.restarts = 0;
        self.state = LspState::Idle;
    }

    pub fn client_mut(&mut self) -> Option<&mut Client> {
        self.client.as_mut()
    }

    /// The client for a caller that only reads — capabilities, liveness.
    ///
    /// Separate from [`Self::client_mut`] so asking "what did the server declare?" does not
    /// need a mutable borrow of the whole session. The trigger-character check (#61) runs on
    /// every keystroke from inside a `&self` context, and taking `&mut` there would conflict
    /// with the editor borrow the same handler holds.
    pub fn client(&self) -> Option<&Client> {
        self.client.as_ref()
    }

    pub fn diagnostics_for(&self, uri: &Uri) -> Option<&FileDiagnostics> {
        self.diagnostics.get(uri)
    }

    /// Total problem counts across every file the server has reported on.
    pub fn totals(&self) -> (usize, usize) {
        self.diagnostics.values().fold((0, 0), |(errors, warnings), file| {
            let (e, w) = file.counts();
            (errors + e, warnings + w)
        })
    }

    /// Stores diagnostics for one file, resolving their ranges against `text`.
    ///
    /// `text` is the app's copy of the document. Resolving against the server's copy
    /// instead would be more correct in principle and useless in practice: the ranges are
    /// painted over *our* buffer, so a position that does not exist in our text has to be
    /// clamped to something that does, or the squiggle lands on the wrong characters.
    pub fn set_diagnostics(&mut self, uri: Uri, diagnostics: &[Diagnostic], text: &str) {
        if diagnostics.is_empty() {
            // An empty publish is how a server says "this file is clean now". Keeping an
            // empty entry would be harmless but pointless; removing it keeps `totals`
            // cheap and the map the size of the problem.
            self.diagnostics.remove(&uri);
            return;
        }

        let index = elle_lsp::LineIndex::new(text);
        let items = diagnostics
            .iter()
            .map(|d| ResolvedDiagnostic {
                // UTF-16 unconditionally: this is the encoding every server must support
                // and the one negotiated for the diagnostics path. A server that
                // negotiated UTF-8 sends positions that happen to agree for ASCII and
                // disagree exactly where it matters — see the offset module's tests.
                range: index.byte_range(text, d.range, encoding_for(text)),
                severity: Severity::from_lsp(d.severity),
                message: d.message.clone(),
            })
            .collect();

        self.diagnostics.insert(uri, FileDiagnostics { items });
    }
}

/// One row of the symbol palette: what to show, and where it lives.
///
/// Flattened out of whichever shape the server answered in, so nothing downstream has to
/// know that `textDocument/documentSymbol` has two incompatible return types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    /// `Class::method` where the server told us the container, `method` where it did not.
    pub label: String,
    /// 0-based, ready for a `Point` row. The *selection* range where there is one — that is
    /// the identifier itself, which is where a reader wants the cursor, rather than the
    /// first line of a doc comment that the enclosing range starts at.
    pub line: u32,
    /// Nesting depth, for the indent that makes a class's methods readable as its methods.
    pub depth: usize,
}

/// Flattens a document-symbol response into palette rows, in document order.
///
/// # Why both shapes are handled
///
/// The protocol has two return types for this request and lets the server pick. `Nested`
/// (`DocumentSymbol`) is the modern one and carries a tree; `Flat` (`SymbolInformation`) is
/// the legacy one and carries a `container_name` string instead. Handling only the shape
/// the server of the day happens to send is the kind of thing that works until someone
/// swaps their server and the palette silently goes empty — RISKS.md #2 in miniature, so
/// both are flattened here into the one type the UI knows.
///
/// Depth-first, because a symbol list that does not follow the file reads as a jumble: a
/// class's methods belong under the class, in the order they are written.
pub fn flatten_symbols(response: &DocumentSymbolResponse) -> Vec<Symbol> {
    let mut out = Vec::new();
    match response {
        DocumentSymbolResponse::Nested(symbols) => push_nested(symbols, 0, &mut out),
        DocumentSymbolResponse::Flat(symbols) => {
            for symbol in symbols {
                // The legacy shape has no tree, only a container name. Qualifying the label
                // with it is what keeps two `handle` methods in one file distinguishable.
                let label = match &symbol.container_name {
                    Some(container) if !container.is_empty() => {
                        format!("{container}::{}", symbol.name)
                    }
                    _ => symbol.name.clone(),
                };
                out.push(Symbol { label, line: symbol.location.range.start.line, depth: 0 });
            }
        }
    }
    out
}

/// Applies LSP `TextEdit`s to a string, or `None` when the batch is malformed (#19).
///
/// The closed-file half of a rename: a buffer the editor holds goes through
/// `Document::apply_edits` (undo, cursor); a file it has never opened goes through this
/// and back to disk. The positions are UTF-16 (`ação` is four units but six bytes — a
/// byte-naive application corrupts the line), and an overlapping batch is a protocol
/// violation refused whole, the same rule as `Document::apply_edits`.
pub fn apply_lsp_edits_to_text(
    text: &str,
    edits: Vec<elle_lsp::lsp_types::TextEdit>,
) -> Option<String> {
    let index = elle_lsp::LineIndex::new(text);
    let mut byte_edits: Vec<(std::ops::Range<usize>, String)> = edits
        .into_iter()
        .map(|edit| {
            (index.byte_range(text, edit.range, elle_lsp::OffsetEncoding::Utf16), edit.new_text)
        })
        .collect();
    byte_edits.sort_by_key(|(range, _)| (range.start, range.end));
    if byte_edits.windows(2).any(|pair| pair[0].0.end > pair[1].0.start) {
        return None;
    }

    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (range, new_text) in byte_edits {
        out.push_str(&text[cursor..range.start]);
        out.push_str(&new_text);
        cursor = range.end;
    }
    out.push_str(&text[cursor..]);
    Some(out)
}

/// Flattens a `WorkspaceEdit` into per-file text edits, or `None` when it contains
/// anything beyond text (#19).
///
/// `None` for file create/rename/delete operations, deliberately whole: a rename that
/// needed a file operation and got only the text half would leave the project broken in
/// the way that is worst to debug — everything compiles except the one file whose name
/// no longer matches its class. Refusing is honest; the status line says why.
pub fn workspace_edit_changes(
    edit: elle_lsp::lsp_types::WorkspaceEdit,
) -> Option<Vec<(PathBuf, Vec<elle_lsp::lsp_types::TextEdit>)>> {
    use elle_lsp::lsp_types::{DocumentChanges, OneOf};
    let mut out: Vec<(PathBuf, Vec<elle_lsp::lsp_types::TextEdit>)> = Vec::new();

    if let Some(changes) = edit.changes {
        for (uri, edits) in changes {
            out.push((elle_lsp::uri_to_path(&uri).ok()?, edits));
        }
    }
    match edit.document_changes {
        None => {}
        Some(DocumentChanges::Edits(document_edits)) => {
            for document_edit in document_edits {
                let path = elle_lsp::uri_to_path(&document_edit.text_document.uri).ok()?;
                let edits = document_edit
                    .edits
                    .into_iter()
                    .map(|edit| match edit {
                        OneOf::Left(edit) => edit,
                        OneOf::Right(annotated) => annotated.text_edit,
                    })
                    .collect();
                out.push((path, edits));
            }
        }
        // File operations: refuse the whole edit rather than apply half a rename.
        Some(DocumentChanges::Operations(_)) => return None,
    }

    // Deterministic order so failures and status messages are stable.
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Some(out)
}

/// Palette rows for a workspace-wide symbol search (#19): `(label, target id)`.
///
/// The label qualifies with the container when there is one — two `handle` methods in
/// two controllers must be distinguishable in a project-wide list. A nested-shape reply
/// whose location carries only a URI (the spec allows it) lands at the top of its file:
/// an honest degradation, not a guess at a line.
pub fn workspace_symbol_items(
    response: &elle_lsp::lsp_types::WorkspaceSymbolResponse,
) -> Vec<(String, std::path::PathBuf, u32)> {
    use elle_lsp::lsp_types::{OneOf, WorkspaceSymbolResponse};
    let label = |name: &str, container: &Option<String>| match container {
        Some(container) if !container.is_empty() => format!("{container}::{name}"),
        _ => name.to_string(),
    };
    match response {
        WorkspaceSymbolResponse::Flat(symbols) => symbols
            .iter()
            .filter_map(|symbol| {
                let path = elle_lsp::uri_to_path(&symbol.location.uri).ok()?;
                Some((
                    label(&symbol.name, &symbol.container_name),
                    path,
                    symbol.location.range.start.line,
                ))
            })
            .collect(),
        WorkspaceSymbolResponse::Nested(symbols) => symbols
            .iter()
            .filter_map(|symbol| {
                let (uri, line) = match &symbol.location {
                    OneOf::Left(location) => (&location.uri, location.range.start.line),
                    OneOf::Right(workspace_location) => (&workspace_location.uri, 0),
                };
                Some((label(&symbol.name, &symbol.container_name), elle_lsp::uri_to_path(uri).ok()?, line))
            })
            .collect(),
    }
}

fn push_nested(symbols: &[DocumentSymbol], depth: usize, out: &mut Vec<Symbol>) {
    for symbol in symbols {
        out.push(Symbol {
            label: symbol.name.clone(),
            line: symbol.selection_range.start.line,
            depth,
        });
        if let Some(children) = &symbol.children {
            push_nested(children, depth + 1, out);
        }
    }
}

/// The encoding diagnostics positions are read in.
///
/// Split out as a function so the choice is stated once and can be traced. It is a
/// constant today: the client advertises UTF-8 first but every server in practice
/// negotiates UTF-16, and reading the *negotiated* value here would mean threading the
/// live `Capabilities` through the notification path for a value that has never differed.
///
/// ponytail: read `Client::encoding()` here once a server is found that negotiates
/// anything else. The conversion is already encoding-generic; only this call site assumes.
fn encoding_for(_text: &str) -> elle_lsp::OffsetEncoding {
    elle_lsp::OffsetEncoding::Utf16
}

/// The configured server command, or `None` if the LSP is switched off.
///
/// Whitespace-split rather than shell-parsed: a real command line needs quoting rules,
/// and the one thing anyone needs here is `binary --flag`. A path with a space in it is
/// the case this cannot express, and it is worth naming rather than half-supporting —
/// #60's settings layer takes a proper argv array.
pub fn configured_command() -> Option<(String, Vec<String>)> {
    let raw = std::env::var(COMMAND_VAR).unwrap_or_else(|_| DEFAULT_SERVER.to_string());
    split_command(&raw)
}

/// Splits a command string into a binary and its arguments.
fn split_command(raw: &str) -> Option<(String, Vec<String>)> {
    let mut parts = raw.split_whitespace().map(str::to_string);
    let command = parts.next()?;
    Some((command, parts.collect()))
}

/// Directories searched for the server binary when `PATH` does not have it.
///
/// # Why this list exists at all
///
/// `open target/ellefuanti.app` hands the app **launchd's** environment, and on a normal
/// macOS install `launchctl getenv PATH` is empty — so a `.app` double-clicked from Finder
/// sees none of what the user's shell sees. Every node-based language server installs under
/// a version manager whose `bin` directory is put on `PATH` by a shell rc file that a
/// Finder launch never runs. The result is #123: the editor works from a terminal and
/// appears to have no LSP at all from the Dock, which reads as the feature being broken.
///
/// These are the prefixes those installers use, relative to `$HOME`. They name *installers*
/// — nvm, Herd, Homebrew — never a language server, which is the RISKS.md #2 line the
/// architecture test enforces: this file may know where binaries live and must not know
/// which one it is looking for.
///
/// ponytail: a fixed list, not a `PATH` reconstruction. Reading the user's login shell and
/// asking it for its `PATH` (`zsh -lic 'echo $PATH'`) is what a full fix does, and it costs
/// a subprocess on every folder open plus a hang when someone's rc file blocks on input.
/// Upgrade to that if a real install turns up outside these three.
const BINARY_SEARCH_PREFIXES: [&str; 5] = [
    // nvm, and Herd's bundled copy of it. The version component is a glob: `versions/node`
    // holds one directory per installed node, and any of them may own the binary.
    ".nvm/versions/node/*/bin",
    "Library/Application Support/Herd/config/nvm/versions/node/*/bin",
    // Homebrew on Apple silicon lives outside `$HOME`; `resolve_binary` also checks the
    // two absolute prefixes below. These two are the per-user npm targets.
    // Herd's own bin directory is where its php lives — artisan (#23) resolves php
    // through this same list.
    "Library/Application Support/Herd/bin",
    ".local/bin",
    ".npm-global/bin",
];

/// Absolute directories searched after the `$HOME`-relative ones.
const ABSOLUTE_SEARCH_PREFIXES: [&str; 3] =
    ["/opt/homebrew/bin", "/usr/local/bin", "/usr/local/opt/node/bin"];

/// Finds `command` as an executable file, or `None` if it is not installed.
///
/// An absolute or relative path is taken at its word — someone who configured
/// `ELLE_LSP_COMMAND=/opt/my-server` means that file, and searching for its basename
/// somewhere else would run a different program than the one they named.
///
/// A bare name is looked up in `dirs`, in order, first hit wins. The caller supplies the
/// list so this is testable against a `tempfile` directory rather than the real machine —
/// a test that asserted against the developer's actual `PATH` would pass or fail depending
/// on who ran it.
pub(crate) fn resolve_binary(command: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    if command.contains('/') {
        let path = PathBuf::from(command);
        return is_executable(&path).then_some(path);
    }

    dirs.iter().find_map(|dir| {
        let candidate = dir.join(command);
        is_executable(&candidate).then_some(candidate)
    })
}

/// Whether `path` is a file this process could execute.
///
/// The mode check matters: `~/.local/bin` routinely holds a directory or a stray README,
/// and treating either as the server turns "not installed" into a spawn failure with a
/// confusing message.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Every directory a bare command name is looked for in, `PATH` first.
///
/// `PATH` wins because a user who put a server on it chose that one. The fallbacks are for
/// the case where there is no `PATH` to consult at all (#123), and appending rather than
/// prepending them means a shell launch behaves exactly as it did before this existed.
pub(crate) fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();

    let home = std::env::var_os("HOME").map(PathBuf::from);
    for prefix in BINARY_SEARCH_PREFIXES {
        let Some(home) = home.as_ref() else { break };
        match prefix.split_once('*') {
            // A glob prefix expands to one candidate per installed version.
            Some((before, after)) => {
                let after = after.trim_start_matches('/');
                let Ok(entries) = std::fs::read_dir(home.join(before.trim_end_matches('/'))) else {
                    continue;
                };
                dirs.extend(entries.flatten().map(|entry| entry.path().join(after)));
            }
            None => dirs.push(home.join(prefix)),
        }
    }
    dirs.extend(ABSOLUTE_SEARCH_PREFIXES.iter().map(PathBuf::from));
    dirs
}

/// Builds the config for a project root, or `None` if no server is installed.
///
/// # Why the binary is resolved here rather than left to the spawn
///
/// This used to return `Some` unconditionally and let `Command::spawn` fail. Two things
/// came of that, and both were reported as "the completion popup never opens" (#125):
///
/// - the failure arrived as an `io::Error` on the background executor, logged at `debug`,
///   so at the default level a machine with no server produced **no output at all** — the
///   evidence that opened #125 was `grep -c` over the log returning zero;
/// - the state went to `Unavailable` only after a spawn attempt, so the difference between
///   "not installed" and "installed somewhere this process cannot see" (#123) was not
///   recorded anywhere, and the two need different answers from the user.
///
/// Resolving first makes "not installed" a fact known before anything is spawned, which is
/// what lets the status bar say so (#125) and what makes the Finder-launch case findable.
///
/// The path found is passed on as the command, so the spawn does not repeat the lookup
/// against the empty `PATH` that caused #123 in the first place.
///
/// # Why the child is also given a `PATH`, which is not the same fix
///
/// Finding the binary is **not sufficient**, and assuming it was is what made the first
/// attempt at #123 incomplete. Every node-based language server installs as a script whose
/// first line is `#!/usr/bin/env node`, so executing it makes the kernel run `env`, and
/// `env` looks up `node` in the child's `PATH`. With launchd's empty environment that fails
/// with `env: node: No such file or directory` and exit status 127 — the binary was found,
/// spawned, and died before writing a byte of LSP.
///
/// Measured on this machine, running the resolved Herd/nvm intelephense directly:
///
/// ```text
/// with PATH:    exit status 1     (the script ran; the flag was wrong)
/// without PATH: exit status 127   env: node: No such file or directory
/// ```
///
/// So the child gets `PATH` set to the same directories the binary was searched in. The
/// interpreter a server needs lives next to the server itself — `node` is in the very
/// `bin/` directory nvm put `intelephense` in — so the list that found one finds the other.
pub fn config_for(root: &Path) -> Option<ServerConfig> {
    let (command, args) = configured_command()?;

    let dirs = search_dirs();
    let Some(binary) = resolve_binary(&command, &dirs) else {
        tracing::debug!("no language server: `{command}` is not installed on PATH");
        return None;
    };

    Some(
        // The label stays the *configured* name, not the resolved path: it is what the user
        // wrote and what they would search for. The path is only what gets executed.
        ServerConfig::new(command, binary.to_string_lossy().into_owned(), root)
            .with_args(args)
            // The interpreter the shebang needs. See this function's docs: without it a
            // Finder launch spawns the server successfully and it dies at `env: node: No
            // such file or directory` before writing a byte.
            .with_env("PATH", join_paths(&dirs))
            .with_language_ids([LANGUAGE_ID]),
    )
}

/// Joins search directories into a `PATH` value for the child process.
///
/// `std::env::join_paths` rather than `join(":")`: the separator is a platform detail, and
/// a directory containing one would silently split into two unusable entries. A path that
/// cannot be expressed is dropped rather than corrupting the whole variable — losing one
/// candidate directory is recoverable, a malformed `PATH` is not.
fn join_paths(dirs: &[PathBuf]) -> String {
    std::env::join_paths(dirs)
        .or_else(|_| {
            let usable: Vec<_> = dirs
                .iter()
                .filter(|dir| !dir.as_os_str().to_string_lossy().contains(':'))
                .collect();
            std::env::join_paths(usable)
        })
        .map(|joined| joined.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Whether this path is one the configured server should be told about.
///
/// Plain PHP only, and **Blade is deliberately excluded** even though `.blade.php` ends in
/// `.php`. A PHP server parsing a Blade template sees `@if`, `@foreach` and `{{ $x }}` as
/// syntax errors and reports one diagnostic per directive — a template would light up red
/// end to end, and the user would read that as the editor being broken rather than as the
/// server being asked the wrong question. ADR-0006 says Blade is handled by the scanner,
/// not by a PHP parser, and that applies to diagnostics too.
///
/// Reuses `elle_syntax`'s detection rather than matching extensions here, so the two
/// cannot disagree about what a `.blade.php` file is.
pub fn handles(path: &Path) -> bool {
    elle_syntax::language_for_path(path) == elle_syntax::Language::Php
}

/// The URI for a path, or `None` if it cannot be expressed as one.
pub fn uri_for(path: &Path) -> Option<Uri> {
    path_to_uri(path).ok()
}

/// Starts a server, blocking. Runs on the background executor.
///
/// Returns `Err` for every failure including "not installed", and the caller decides
/// which of those the user hears about — that split is the whole §24 story, and it is
/// deliberately *not* made here: this function does not know whether it is a first
/// attempt or a third restart.
pub fn start(config: &ServerConfig) -> anyhow::Result<Client> {
    Client::start(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use elle_lsp::lsp_types::{Position, Range, TextEdit};

    fn uri() -> Uri {
        "file:///srv/app/User.php".parse().unwrap()
    }

    fn diagnostic(line: u32, start: u32, end: u32, severity: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position { line, character: start },
                end: Position { line, character: end },
            },
            severity: Some(severity),
            message: "Undefined variable".into(),
            ..Default::default()
        }
    }

    // --- the command, and the case nobody has a server ---------------------------

    #[test]
    fn the_default_command_is_used_when_the_variable_is_unset() {
        // Not `std::env::set_var` — tests share a process, and mutating the environment
        // makes this suite order-dependent. `split_command` is the whole logic; the
        // lookup around it is one `var()` call.
        let (command, args) = split_command(DEFAULT_SERVER).expect("a default must parse");
        assert_eq!(command, "intelephense");
        assert_eq!(args, ["--stdio"]);
    }

    #[test]
    fn an_empty_command_switches_the_lsp_off() {
        // The escape hatch for someone who has Intelephense installed and does not want it
        // running. It must yield None rather than trying to spawn "".
        assert!(split_command("").is_none());
        assert!(split_command("   ").is_none());
    }

    #[test]
    fn a_command_with_arguments_splits_into_binary_and_argv() {
        let (command, args) = split_command("phpactor language-server -vvv").unwrap();
        assert_eq!(command, "phpactor");
        assert_eq!(args, ["language-server", "-vvv"]);
    }

    // --- finding the binary (#123, and half of #125) -----------------------------

    /// Creates `name` under `dir` with the mode `resolve_binary` requires.
    fn install(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn a_server_that_is_not_installed_is_found_before_it_is_spawned() {
        // The #125 case. Before this, `config_for` returned `Some` for a binary that does
        // not exist, the spawn failed on the background executor, and the only trace was a
        // `debug` line — so the log of a real session contained *nothing*, which is what
        // made the bug look like the popup rather than the server.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve_binary("nothing-is-installed", &[dir.path().to_path_buf()]), None);
    }

    #[test]
    fn a_bare_name_is_found_in_the_search_directories() {
        let dir = tempfile::tempdir().unwrap();
        let installed = install(dir.path(), "some-language-server");

        assert_eq!(
            resolve_binary("some-language-server", &[dir.path().to_path_buf()]),
            Some(installed)
        );
    }

    #[test]
    fn the_first_directory_holding_the_binary_wins() {
        // `search_dirs` puts `PATH` in front of the fallbacks, so a server the user chose
        // must beat one that merely happens to sit under a version manager. Asserting the
        // order here is what pins that: a `find_map` over a set would pass a test that only
        // checked "it was found somewhere".
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let preferred = install(first.path(), "some-language-server");
        install(second.path(), "some-language-server");

        let found = resolve_binary(
            "some-language-server",
            &[first.path().to_path_buf(), second.path().to_path_buf()],
        );
        assert_eq!(found, Some(preferred), "PATH must win over the fallbacks");
    }

    #[test]
    fn a_directory_with_the_right_name_is_not_a_server() {
        // `~/.local/bin` holds whatever anyone put there. Accepting a directory would turn
        // "not installed" into a spawn failure with a worse message than the true one.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("some-language-server")).unwrap();

        assert_eq!(resolve_binary("some-language-server", &[dir.path().to_path_buf()]), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_without_the_executable_bit_is_not_a_server() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("some-language-server"), "not a program").unwrap();

        assert_eq!(resolve_binary("some-language-server", &[dir.path().to_path_buf()]), None);
    }

    #[test]
    fn a_configured_path_is_taken_at_its_word() {
        // Someone who writes `ELLE_LSP_COMMAND=/opt/theirs/server` means that file. Looking
        // its basename up in the search directories could run a *different* program with
        // the same name, which is worse than failing.
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let installed = install(dir.path(), "some-language-server");
        install(elsewhere.path(), "some-language-server");

        let configured = installed.to_string_lossy().into_owned();
        assert_eq!(
            resolve_binary(&configured, &[elsewhere.path().to_path_buf()]),
            Some(installed),
            "an absolute path must not be re-resolved against the search list"
        );
    }

    #[test]
    fn a_configured_path_that_does_not_exist_is_not_invented() {
        let elsewhere = tempfile::tempdir().unwrap();
        install(elsewhere.path(), "some-language-server");

        assert_eq!(
            resolve_binary("/nowhere/some-language-server", &[elsewhere.path().to_path_buf()]),
            None,
            "a named path that is absent must fail, not fall back to a namesake"
        );
    }

    #[test]
    fn the_child_is_given_a_path_because_finding_the_binary_is_not_enough() {
        // The half of #123 the first fix missed, and the reason this test exists at all.
        //
        // Every node-based server installs as a script starting `#!/usr/bin/env node`, so
        // running it makes the kernel run `env`, which looks `node` up in the **child's**
        // PATH. Resolving the server's own path does nothing for that. Measured against the
        // real Herd/nvm intelephense on this machine:
        //
        //     with PATH:    exit status 1     (ran; wrong flag)
        //     without PATH: exit status 127   env: node: No such file or directory
        //
        // So the config must carry a PATH, or a Finder launch spawns a server that dies
        // before writing a byte of LSP — which looks exactly like the server not existing.
        let root = tempfile::tempdir().unwrap();
        let Some(config) = config_for(root.path()) else {
            // No server installed on this machine; nothing to assert about its environment.
            return;
        };

        let path = config.env.get("PATH").expect("the child must be given a PATH (#123)");
        assert!(!path.is_empty(), "an empty PATH is the bug, not the fix");
        assert!(
            std::env::split_paths(path).count() > 1,
            "the child's PATH must carry the search directories, not a single entry"
        );
    }

    #[test]
    fn a_directory_that_cannot_be_expressed_does_not_destroy_the_whole_path() {
        // `join_paths` refuses a directory containing the separator. Dropping that one
        // candidate is recoverable; returning an empty string would take every other
        // directory with it and reintroduce #123 for everyone.
        let dirs = vec![PathBuf::from("/usr/local/bin"), PathBuf::from("/opt/we:rd")];

        let joined = join_paths(&dirs);
        assert!(joined.contains("/usr/local/bin"), "the usable directories must survive");
    }

    #[test]
    fn the_search_list_extends_past_path() {
        // #123: a Finder-launched `.app` gets launchd's environment, where `PATH` is empty.
        // The fallbacks are the whole fix, so "the list is longer than `PATH`" is the thing
        // worth pinning — the specific directories are a property of the machine and would
        // make this a test about the developer's laptop.
        let dirs = search_dirs();
        let on_path =
            std::env::var_os("PATH").map(|path| std::env::split_paths(&path).count()).unwrap_or(0);

        assert!(
            dirs.len() > on_path,
            "a launch with no PATH must still have somewhere to look (#123)"
        );
    }

    #[test]
    fn only_php_files_are_sent_to_the_server() {
        assert!(handles(Path::new("/srv/app/User.php")));
        assert!(!handles(Path::new("/srv/README.md")));
        assert!(!handles(Path::new("/srv/app/Makefile")));

        // The one that matters. `.blade.php` ends in `.php`, so an extension check would
        // send it — and a PHP server reports every `@if` and `{{ $x }}` as a syntax error,
        // lighting a template up red end to end. ADR-0006 keeps Blade away from the PHP
        // parser; this keeps it away from the PHP server for the same reason.
        assert!(!handles(Path::new("/srv/resources/views/home.blade.php")));
    }

    #[test]
    fn a_diagnostic_is_found_from_anywhere_on_its_line() {
        // The discoverability fix: the exact-span lookup made the reason invisible unless
        // the cursor landed inside the squiggle's own bytes. Clicking anywhere on the
        // marked line is what people do, so that is what must work.
        let mut lsp = Lsp::new();
        let text = "<?php\n$x = $undefined;\n";
        lsp.set_diagnostics(uri(), &[diagnostic(1, 5, 15, DiagnosticSeverity::ERROR)], text);
        let file = lsp.diagnostics_for(&uri()).unwrap();

        // Line 1 spans bytes 6..22. Column 0 is nowhere near the squiggle at 11..21.
        let line = 6..22;
        assert!(file.on_line(line.clone()).is_some(), "anywhere on the line finds it");
        // And the neighbouring line must not.
        assert!(file.on_line(0..5).is_none(), "a clean line stays clean");
    }

    // --- restart bounding --------------------------------------------------------

    #[test]
    fn restarts_are_bounded() {
        // A crash loop must settle into "no LSP", not respawn forever. Without the bound
        // a server that dies on startup is a fork bomb that makes the editor unusable —
        // the exact opposite of what §24 asks for.
        let mut lsp = Lsp::new();
        for _ in 0..MAX_RESTARTS {
            assert!(lsp.may_restart());
            lsp.record_restart();
        }
        assert!(!lsp.may_restart(), "the {MAX_RESTARTS}th restart must be the last");
    }

    #[test]
    fn opening_a_different_folder_forgives_the_old_ones_crashes() {
        // The budget is per project. A broken install in one folder must not leave the
        // next project with no LSP for reasons the user cannot see.
        let mut lsp = Lsp::new();
        for _ in 0..MAX_RESTARTS {
            lsp.record_restart();
        }
        assert!(!lsp.may_restart());

        lsp.set_root(Some(PathBuf::from("/srv/other")));
        assert!(lsp.may_restart(), "a new project starts with a full budget");
        assert_eq!(lsp.state(), &LspState::Idle);
    }

    // --- diagnostics -------------------------------------------------------------

    #[test]
    fn diagnostic_positions_become_byte_ranges() {
        let mut lsp = Lsp::new();
        let text = "<?php\n$x = $undefined;\n";
        // Line 1, characters 5..15 — `$undefined`.
        lsp.set_diagnostics(uri(), &[diagnostic(1, 5, 15, DiagnosticSeverity::ERROR)], text);

        let file = lsp.diagnostics_for(&uri()).expect("stored");
        assert_eq!(&text[file.items[0].range.clone()], "$undefined");
    }

    #[test]
    fn positions_are_read_as_utf16_not_bytes() {
        // The bug this pins is the one an ASCII fixture cannot see. `ação` is 4 characters
        // and 6 bytes; a diagnostic starting after it arrives as character 10 and must
        // resolve to byte 12. Reading the position as a byte offset would underline four
        // characters to the left of the real problem.
        let mut lsp = Lsp::new();
        let text = "<?php\n// ação $x\n";
        let line = "// ação $x";
        let start_char = line.chars().take_while(|c| *c != '$').count() as u32;

        lsp.set_diagnostics(
            uri(),
            &[diagnostic(1, start_char, start_char + 2, DiagnosticSeverity::WARNING)],
            text,
        );

        let file = lsp.diagnostics_for(&uri()).unwrap();
        assert_eq!(&text[file.items[0].range.clone()], "$x");
    }

    #[test]
    fn an_empty_publish_clears_the_file() {
        // How a server says "fixed". Leaving the old squiggles up would mean the editor
        // shows problems the server no longer believes in.
        let mut lsp = Lsp::new();
        let text = "<?php\n$x = 1;\n";
        lsp.set_diagnostics(uri(), &[diagnostic(1, 0, 2, DiagnosticSeverity::ERROR)], text);
        assert!(lsp.diagnostics_for(&uri()).is_some());

        lsp.set_diagnostics(uri(), &[], text);
        assert!(lsp.diagnostics_for(&uri()).is_none());
        assert_eq!(lsp.totals(), (0, 0));
    }

    #[test]
    fn shutting_down_forgets_every_diagnostic() {
        // Stale squiggles from a dead server are worse than none: they look current, and
        // the user cannot tell that nothing is updating them any more.
        let mut lsp = Lsp::new();
        let text = "<?php\n$x = 1;\n";
        lsp.set_diagnostics(uri(), &[diagnostic(1, 0, 2, DiagnosticSeverity::ERROR)], text);

        lsp.shut_down();

        assert!(lsp.diagnostics_for(&uri()).is_none());
        assert_eq!(lsp.totals(), (0, 0));
    }

    #[test]
    fn only_errors_and_warnings_reach_the_status_bar_counts() {
        // A count that includes hints says "7 problems" for seven unused imports, which
        // trains people to stop reading it.
        let mut lsp = Lsp::new();
        let text = "<?php\naaaa bbbb cccc dddd\n";
        lsp.set_diagnostics(
            uri(),
            &[
                diagnostic(1, 0, 4, DiagnosticSeverity::ERROR),
                diagnostic(1, 5, 9, DiagnosticSeverity::WARNING),
                diagnostic(1, 10, 14, DiagnosticSeverity::INFORMATION),
                diagnostic(1, 15, 19, DiagnosticSeverity::HINT),
            ],
            text,
        );

        assert_eq!(lsp.totals(), (1, 1));
        // But all four are still stored, because all four get a squiggle.
        assert_eq!(lsp.diagnostics_for(&uri()).unwrap().items.len(), 4);
    }

    #[test]
    fn a_diagnostic_with_no_severity_is_treated_as_an_error() {
        // The specification leaves it to the client. Under-reporting a real problem is
        // worse than over-colouring an advisory one.
        let mut lsp = Lsp::new();
        let text = "<?php\n$x = 1;\n";
        let mut d = diagnostic(1, 0, 2, DiagnosticSeverity::ERROR);
        d.severity = None;
        lsp.set_diagnostics(uri(), &[d], text);

        assert_eq!(lsp.diagnostics_for(&uri()).unwrap().items[0].severity, Severity::Error);
        assert_eq!(lsp.totals(), (1, 0));
    }

    #[test]
    fn the_message_under_the_cursor_is_the_most_specific_one() {
        // Servers routinely report a broad "this expression is invalid" over a precise
        // "undefined variable $x". The precise one is what the user needs; picking the
        // first match would show whichever happened to arrive first.
        let mut lsp = Lsp::new();
        let text = "<?php\n$a + $undefined;\n";

        let mut broad = diagnostic(1, 0, 16, DiagnosticSeverity::ERROR);
        broad.message = "invalid expression".into();
        let mut precise = diagnostic(1, 5, 15, DiagnosticSeverity::ERROR);
        precise.message = "undefined variable".into();

        lsp.set_diagnostics(uri(), &[broad, precise], text);

        let file = lsp.diagnostics_for(&uri()).unwrap();
        let offset = text.find("$undefined").unwrap() + 1;
        assert_eq!(file.at(offset).unwrap().message, "undefined variable");
    }

    #[test]
    fn the_cursor_just_past_a_problem_still_reads_it() {
        // Where the cursor lands after typing the thing being complained about. An
        // exclusive end would go silent exactly when the user is looking at it.
        let mut lsp = Lsp::new();
        let text = "<?php\n$x\n";
        lsp.set_diagnostics(uri(), &[diagnostic(1, 0, 2, DiagnosticSeverity::ERROR)], text);

        let file = lsp.diagnostics_for(&uri()).unwrap();
        let end = file.items[0].range.end;
        assert!(file.at(end).is_some(), "the offset just past the range must still match");
        assert!(file.at(end + 1).is_none(), "but not one beyond that");
    }

    // --- document symbols ----------------------------------------------------------

    fn range(line: u32) -> Range {
        Range { start: Position { line, character: 0 }, end: Position { line, character: 10 } }
    }

    #[allow(deprecated)]
    fn nested(name: &str, line: u32, children: Vec<DocumentSymbol>) -> DocumentSymbol {
        DocumentSymbol {
            name: name.to_string(),
            detail: None,
            kind: elle_lsp::lsp_types::SymbolKind::CLASS,
            tags: None,
            deprecated: None,
            // Deliberately different from the selection range: the enclosing range starts
            // at the doc comment, and jumping there rather than to the identifier is the
            // bug this fixture exists to catch.
            range: range(line.saturating_sub(2)),
            selection_range: range(line),
            children: if children.is_empty() { None } else { Some(children) },
        }
    }

    #[test]
    fn nested_symbols_flatten_in_document_order_with_depth() {
        // A class's methods must read as its methods, in the order they are written.
        let response = DocumentSymbolResponse::Nested(vec![
            nested("User", 10, vec![nested("save", 12, vec![]), nested("delete", 20, vec![])]),
            nested("Post", 40, vec![]),
        ]);

        let symbols = flatten_symbols(&response);
        let seen: Vec<_> = symbols.iter().map(|s| (s.label.as_str(), s.line, s.depth)).collect();
        assert_eq!(seen, [("User", 10, 0), ("save", 12, 1), ("delete", 20, 1), ("Post", 40, 0)]);
    }

    #[test]
    fn a_nested_symbol_points_at_its_name_not_its_doc_comment() {
        // `range` encloses the whole declaration including comments; `selection_range` is
        // the identifier. Landing on the comment two lines above looks like an off-by-two
        // to the user and is the easier of the two fields to reach for.
        let response = DocumentSymbolResponse::Nested(vec![nested("User", 10, vec![])]);
        assert_eq!(flatten_symbols(&response)[0].line, 10);
    }

    #[allow(deprecated)]
    #[test]
    fn the_legacy_flat_shape_is_handled_too() {
        // The protocol lets the server choose the shape. Handling only the modern one works
        // until somebody swaps their server and the palette silently goes empty — which is
        // exactly the substitutability failure RISKS.md #2 is about.
        let location = elle_lsp::lsp_types::Location { uri: uri(), range: range(7) };
        let response = DocumentSymbolResponse::Flat(vec![
            elle_lsp::lsp_types::SymbolInformation {
                name: "save".into(),
                kind: elle_lsp::lsp_types::SymbolKind::METHOD,
                tags: None,
                deprecated: None,
                location: location.clone(),
                container_name: Some("User".into()),
            },
            elle_lsp::lsp_types::SymbolInformation {
                name: "helper".into(),
                kind: elle_lsp::lsp_types::SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                location,
                container_name: None,
            },
        ]);

        let symbols = flatten_symbols(&response);
        // The container qualifies the name, which is what keeps two `save` methods in one
        // file apart. A symbol with no container is shown bare rather than as `::save`.
        assert_eq!(symbols[0].label, "User::save");
        assert_eq!(symbols[1].label, "helper");
        assert_eq!(symbols[0].line, 7);
    }

    #[test]
    fn a_server_with_nothing_to_say_yields_no_rows() {
        // A file the server has not indexed yet answers with an empty list, not an error.
        assert!(flatten_symbols(&DocumentSymbolResponse::Nested(vec![])).is_empty());
        assert!(flatten_symbols(&DocumentSymbolResponse::Flat(vec![])).is_empty());
    }

    #[test]
    fn workspace_symbols_become_rows_with_container_and_line() {
        use elle_lsp::lsp_types::{
            Location, OneOf, SymbolKind, WorkspaceSymbol, WorkspaceSymbolResponse,
        };
        let range = Range {
            start: Position { line: 7, character: 0 },
            end: Position { line: 7, character: 6 },
        };
        let response = WorkspaceSymbolResponse::Nested(vec![
            WorkspaceSymbol {
                name: "handle".into(),
                kind: SymbolKind::METHOD,
                tags: None,
                container_name: Some("UserController".into()),
                location: OneOf::Left(Location { uri: uri(), range }),
                data: None,
            },
            // The spec allows a URI-only location; it must land at the top of its file
            // rather than being dropped or guessed at.
            WorkspaceSymbol {
                name: "Post".into(),
                kind: SymbolKind::CLASS,
                tags: None,
                container_name: None,
                location: OneOf::Right(elle_lsp::lsp_types::WorkspaceLocation { uri: uri() }),
                data: None,
            },
        ]);

        let rows = workspace_symbol_items(&response);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "UserController::handle", "the container disambiguates");
        assert_eq!(rows[0].2, 7);
        assert_eq!(rows[1].0, "Post");
        assert_eq!(rows[1].2, 0, "URI-only lands at the top of the file, not dropped");
        assert!(rows[0].1.ends_with("User.php"));
    }

    #[test]
    fn lsp_edits_apply_to_text_through_utf16_positions() {
        // The multibyte trap, stated as a fixture: `ação` is 4 UTF-16 units but 6 bytes,
        // so a byte-naive application lands mid-codepoint and corrupts the line.
        let text = "<?php\n$ação = 1;\n$ação = 2;\n";
        let edits = vec![
            TextEdit {
                range: Range {
                    start: Position { line: 1, character: 1 },
                    end: Position { line: 1, character: 5 },
                },
                new_text: "nome".into(),
            },
            TextEdit {
                range: Range {
                    start: Position { line: 2, character: 1 },
                    end: Position { line: 2, character: 5 },
                },
                new_text: "nome".into(),
            },
        ];
        let renamed = apply_lsp_edits_to_text(text, edits).expect("edits apply");
        assert_eq!(renamed, "<?php\n$nome = 1;\n$nome = 2;\n");
    }

    #[test]
    fn overlapping_lsp_edits_are_refused_whole() {
        // A protocol violation must not half-apply — same rule as Document::apply_edits.
        let text = "abcdef";
        let overlapping = vec![
            TextEdit {
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 3 },
                },
                new_text: "X".into(),
            },
            TextEdit {
                range: Range {
                    start: Position { line: 0, character: 2 },
                    end: Position { line: 0, character: 4 },
                },
                new_text: "Y".into(),
            },
        ];
        assert_eq!(apply_lsp_edits_to_text(text, overlapping), None);
    }

    #[test]
    fn an_out_of_range_position_is_clamped_rather_than_panicking() {
        // A server whose copy of the file is ahead of ours sends positions past our end.
        // §24: that is a stale notification, not a crash.
        let mut lsp = Lsp::new();
        let text = "<?php\n";
        lsp.set_diagnostics(uri(), &[diagnostic(99, 0, 400, DiagnosticSeverity::ERROR)], text);

        let file = lsp.diagnostics_for(&uri()).unwrap();
        assert!(file.items[0].range.end <= text.len(), "must clamp into the buffer");
    }
}
