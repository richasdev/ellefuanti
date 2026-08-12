//! The AI chat panel (#99): a conversation on the right, and nothing sent by surprise.
//!
//! # Everything here is opt-in, twice
//!
//! The panel does not exist until `ToggleAiChat` creates it, and it talks to nobody until
//! `ai.chat` is enabled *and* the user presses send. Context is the same story at a finer
//! grain: the Selection and Current-file chips are off by default, attach only on a click,
//! and the current file is checked against [`ai::deny_reason`] **at attach time**, with no
//! override — #99's rule, enforced where the user can see it refuse. The chips are visible
//! while attached, so what a send will carry is never a surprise.
//!
//! # The transport is `crate::ai`'s, not this file's
//!
//! Everything wire-shaped — auth, body, curl argv, SSE parsing, the denylist — lives in
//! [`crate::ai`] and is only *called* from here. This file owns what a panel owns: turns,
//! an input line, chips, a scroll position, and the child process it may need to kill.
//!
//! # Streaming repaints are batched (#93)
//!
//! A reply arrives as dozens of deltas per second, and repainting per delta is exactly the
//! per-token notify the perf gate forbids. The drain task sleeps ~50ms after the first
//! event of a burst, sweeps everything that accumulated, and applies the batch with one
//! `cx.notify()` — the same debounce shape the tree watcher uses for FSEvents bursts.
//!
//! # Cancel is a kill
//!
//! The `curl` child is held behind an `Arc<Mutex<Option<Child>>>` (the test runner's
//! pattern): cancel takes it out and kills it, which also closes the pipe and unblocks the
//! background reader. The half-received turn is kept and marked "(cancelled)" rather than
//! deleted — the user saw those words arrive; making them vanish would be a lie about what
//! happened.
//!
//! # Two transports behind one UI
//!
//! Most providers are an HTTP POST via `curl`. Codex (#99) is not: it is a long-lived
//! `codex app-server` child speaking JSON-RPC, carrying the conversation itself. So the
//! send path branches once — on [`ai::Provider::wire`] being `Some` or `None` — and both
//! branches feed the *same* [`StreamEvent`] channel, the same 50ms batched drain, and the
//! same kill handle. The UI below that seam cannot tell the two apart, which is the point:
//! one panel, one cancel story, one repaint budget.
//!
//! The Codex child outlives a single turn on purpose — one thread is one conversation, so
//! history lives in the CLI rather than being re-sent as a message array. It dies with the
//! panel or with a cancel.

use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{
    App, Context, FocusHandle, Focusable, KeyDownEvent, MouseButton, ScrollHandle, SharedString,
    Task, Window, div, prelude::*, px,
};

use crate::actions::{Backspace, Cancel, Confirm, context};
use crate::ai::{self, StreamEvent, Wire};
use crate::fonts::Fonts;
use crate::theme::{Theme, Themed};

/// Short on purpose: the panel is an assistant inside an editor, not a product persona,
/// and every token of preamble is paid for on every send.
const SYSTEM_PROMPT: &str = "You are the AI assistant inside the ellefuanti IDE. \
     Answer concisely; use fenced code blocks for code.";

/// The reply budget. Matches the chat default the provider layer's tests use; a chat
/// answer that needs more than this needs a follow-up question more.
const MAX_TOKENS: u32 = 4096;

/// How long deltas are allowed to pool before one repaint sweeps them (#93).
const BATCH_INTERVAL: Duration = Duration::from_millis(50);

/// Who said a turn — including the panel itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    User,
    Assistant,
    /// The panel speaking for itself: an auth failure, a provider refusal. Rendered like
    /// an assistant row but muted, and **never** sent back to the provider — an error
    /// string in the conversation history would be the model's next context.
    Note,
}

/// One row of the conversation.
#[derive(Clone, Debug)]
pub struct ChatTurn {
    pub role: Role,
    pub text: String,
}

/// What the workspace can tell the panel about the active editor, gathered at send or
/// attach time — never earlier, because a snapshot taken at panel-open describes a tab
/// the user has since left.
#[derive(Default)]
pub struct EditorSnapshot {
    /// The active editor's selected text, if any.
    pub selection: Option<String>,
    /// The active tab's path and full buffer text. The *buffer*, not the disk: what the
    /// user is looking at is what they mean by "current file".
    pub file: Option<(PathBuf, String)>,
}

/// How the panel reads the editor: a closure the workspace hands it at construction.
///
/// A closure rather than events both ways because the read is synchronous — send needs
/// the selection *now*, and an event round-trip would answer a frame late. The workspace
/// already owns the tabs, so it is the only party that can write this closure.
pub type SnapshotFn = Box<dyn Fn(&App) -> EditorSnapshot + 'static>;

/// A live `codex app-server` child and the thread it opened (#99).
///
/// Held across turns because the *thread* is the conversation: Codex keeps the history,
/// so a second question is a `turn/start` on the same id rather than a re-sent transcript.
/// Both fields are behind the panel's `Arc<Mutex<..>>` so the background reader and the
/// foreground cancel can reach them without either blocking a paint.
struct CodexSession {
    /// The child's stdin, for `turn/start` and `turn/interrupt`. Separate from the kill
    /// handle because writing a request must not need the lock that a kill takes.
    stdin: ChildStdin,
    /// The child's stdout, mid-stream. It has to live in the session rather than in the
    /// turn that opened it: a `BufReader` owns a buffer, and the bytes of turn two are
    /// routinely already sitting in it when turn one's `turn/completed` is parsed.
    /// Dropping the reader between turns would drop those bytes.
    stdout: BufReader<ChildStdout>,
    thread_id: String,
}

/// What the panel knows about its Codex child between turns.
#[derive(Default)]
struct CodexState {
    session: Option<CodexSession>,
    /// The model the CLI reported at `thread/start`, shown in the header so the user can
    /// see which one their subscription handed them.
    model: Option<String>,
}

pub struct AiChatPanel {
    focus_handle: FocusHandle,
    turns: Vec<ChatTurn>,
    input: String,
    /// Whether a reply is arriving right now. Drives the Cancel button and the dim "…".
    streaming: bool,
    /// The curl child, for cancel. `None` between sends and after a kill.
    kill: Arc<Mutex<Option<Child>>>,
    /// The drain task. Held so it lives; finished tasks are replaced on the next send
    /// rather than cleared mid-poll, because a task must not drop itself from inside
    /// its own `update`.
    stream_task: Option<Task<()>>,
    /// The context chips (#99): off by default, explicit, visible.
    attach_selection: bool,
    attach_file: bool,
    /// A transient line above the input: a deny reason, "no file open", and the like.
    note: Option<SharedString>,
    scroll: ScrollHandle,
    snapshot: SnapshotFn,
    /// The open folder, so a Codex thread can be rooted at the project the user is
    /// actually looking at. `None` when nothing is open — then the thread gets no `cwd`
    /// rather than one pointing wherever the app was launched from.
    project_root: Option<PathBuf>,
    /// The Codex child and thread, when that is the chat provider (#99). Shared because
    /// the background handshake fills it in and the foreground cancel empties it.
    codex: Arc<Mutex<CodexState>>,
    /// Codex availability, resolved off the main thread once per panel and cached: the
    /// check spawns `codex --version`, which a render must never do.
    codex_status: Option<Result<(), String>>,
    /// Stops `send` just before the curl spawn, recording the body instead. What lets a
    /// test exercise the whole send path — settings read, context build, turn pushed,
    /// body built — with no network and no child process.
    #[cfg(test)]
    transport_disabled: bool,
    #[cfg(test)]
    sent_bodies: Vec<String>,
    /// Render tests run without a `LiveSettings` global, so the real enabled flag cannot
    /// be turned on. This forces the check, and only the check — nothing else differs.
    #[cfg(test)]
    force_enabled: bool,
}

impl AiChatPanel {
    pub fn new(
        snapshot: SnapshotFn,
        project_root: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel = Self {
            focus_handle: cx.focus_handle(),
            turns: Vec::new(),
            input: String::new(),
            streaming: false,
            kill: Arc::default(),
            stream_task: None,
            attach_selection: false,
            attach_file: false,
            note: None,
            scroll: ScrollHandle::new(),
            snapshot,
            project_root,
            codex: Arc::default(),
            codex_status: None,
            #[cfg(test)]
            transport_disabled: false,
            #[cfg(test)]
            sent_bodies: Vec::new(),
            #[cfg(test)]
            force_enabled: false,
        };
        // Probed at open, not at render: `codex --version` is a subprocess, and a render
        // that spawns one would pay for it every frame. Only when Codex is the chat
        // provider — nobody else's panel should run a CLI they did not ask for.
        #[cfg(not(test))]
        if ai::Provider::from_setting(crate::settings::current(cx).ai_chat_provider())
            == ai::Provider::Codex
        {
            panel.probe_codex(cx);
        }
        panel
    }

    /// Resolves Codex availability off the main thread and caches the answer.
    fn probe_codex(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let status = cx.background_spawn(async { crate::ai_codex::availability() }).await;
            this.update(cx, |this, cx| {
                this.codex_status = Some(status);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // --- sending ---------------------------------------------------------------------

    /// Enter, or the Send button. The whole pipeline hangs off this one deliberate act.
    fn send(&mut self, cx: &mut Context<Self>) {
        if self.streaming {
            return; // one reply at a time; the button says Cancel right now anyway
        }
        let settings = crate::settings::current(cx);
        let enabled = {
            #[cfg(test)]
            {
                settings.ai_chat_enabled() || self.force_enabled
            }
            #[cfg(not(test))]
            {
                settings.ai_chat_enabled()
            }
        };
        if !enabled {
            // Unreachable through the UI (the input is not rendered while disabled), but
            // the guard is what makes "off means off" true rather than layout-dependent.
            self.note = Some("AI chat is off — enable it in Settings (⌘,) → AI".into());
            cx.notify();
            return;
        }
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            return;
        }

        // Context, gathered now: the chips say what may go, the snapshot says what is
        // there, and the deny check runs again here — attach-time checking is the UX,
        // this is the guarantee (a file renamed to `.env` after attaching is still
        // refused).
        let snap = (self.snapshot)(cx);
        let file = if self.attach_file { snap.file } else { None };
        let selection = if self.attach_selection { snap.selection } else { None };
        let context_blocks = match build_context(
            file.as_ref().map(|(path, text)| (path.as_path(), text.as_str())),
            selection.as_deref(),
        ) {
            Ok(blocks) => blocks,
            Err(refusal) => {
                self.note = Some(refusal.into());
                cx.notify();
                return;
            }
        };

        // Chat reads its *own* provider key, falling back to `ai.provider` when unset
        // (#99): the owner wants ghost text on an API key and chat on the subscription.
        let provider = ai::Provider::from_setting(settings.ai_chat_provider());
        let base_url = settings.ai_base_url().to_string();

        let message = format!("{context_blocks}{prompt}");
        self.input.clear();
        self.note = None;
        self.turns.push(ChatTurn { role: Role::User, text: message.clone() });

        // The body carries the history *up to and including* the new user turn; the empty
        // assistant turn below is a UI placeholder the wire must not see. Codex has no
        // body at all — the CLI holds the conversation, so a turn is just the new text.
        let body = provider.wire().map(|wire| {
            ai::chat_body(
                wire,
                settings.ai_chat_model(),
                SYSTEM_PROMPT,
                &self.wire_turns(),
                MAX_TOKENS,
            )
        });

        self.turns.push(ChatTurn { role: Role::Assistant, text: String::new() });
        self.streaming = true;
        self.scroll.scroll_to_bottom();
        cx.notify();

        #[cfg(test)]
        if self.transport_disabled {
            // Codex's "body" for the test seam is the message itself: there is no wire
            // body, and what a test needs to pin is what would reach the CLI.
            self.sent_bodies.push(body.unwrap_or(message));
            return;
        }

        match (provider.wire(), body) {
            (Some(wire), Some(body)) => self.start_stream(provider, wire, base_url, body, cx),
            // Codex: the long-lived child, driven from the same channel and drain.
            _ => self.start_codex_stream(message, cx),
        }
    }

    /// The conversation as the wire wants it: user and assistant turns only. Notes are
    /// the panel talking to the user, and the placeholder being streamed into is empty.
    fn wire_turns(&self) -> Vec<ai::Turn> {
        self.turns
            .iter()
            .filter(|turn| turn.role != Role::Note && !turn.text.is_empty())
            .map(|turn| ai::Turn {
                role: if turn.role == Role::User { "user" } else { "assistant" },
                content: turn.text.clone(),
            })
            .collect()
    }

    /// Spawns the producer (curl, on the background pool) and the drain (foreground,
    /// batched). The channel between them is the seam #93 cares about: events pool there
    /// instead of each becoming a repaint.
    fn start_stream(
        &mut self,
        provider: ai::Provider,
        wire: Wire,
        base_url: String,
        body: String,
        cx: &mut Context<Self>,
    ) {
        let (tx, rx) = smol::channel::unbounded::<StreamEvent>();
        let kill: Arc<Mutex<Option<Child>>> = Arc::default();
        self.kill = kill.clone();

        let producer = cx.background_spawn(async move {
            stream_reply(provider, wire, &base_url, &body, &kill, &tx);
        });

        let timer = cx.background_executor().clone();
        self.stream_task = Some(cx.spawn(async move |this, cx| {
            // Moved in so the producer lives exactly as long as someone is draining it.
            let _producer = producer;
            loop {
                let Ok(first) = rx.recv().await else { break };
                // The batching (#93): sleep out the burst, then sweep it into one apply.
                timer.timer(BATCH_INTERVAL).await;
                let mut batch = vec![first];
                while let Ok(event) = rx.try_recv() {
                    batch.push(event);
                }
                let finished =
                    this.update(cx, |this, cx| this.apply_events(batch, cx)).unwrap_or(true);
                if finished {
                    break;
                }
            }
        }));
    }

    /// The Codex half of the send path (#99): ensure a child and a thread, then one turn.
    ///
    /// Deliberately the *same* shape as [`Self::start_stream`] — one channel of
    /// [`StreamEvent`], one batched drain — so everything downstream (the placeholder,
    /// the ellipsis, cancel, the note rows) is shared rather than reimplemented.
    fn start_codex_stream(&mut self, message: String, cx: &mut Context<Self>) {
        let (tx, rx) = smol::channel::unbounded::<StreamEvent>();
        let codex = self.codex.clone();
        let kill: Arc<Mutex<Option<Child>>> = Arc::default();
        self.kill = kill.clone();
        let root = self.project_root.clone();

        let producer = cx.background_spawn(async move {
            codex_turn(&codex, &kill, root.as_deref(), &message, &tx);
        });

        let timer = cx.background_executor().clone();
        self.stream_task = Some(cx.spawn(async move |this, cx| {
            let _producer = producer;
            loop {
                let Ok(first) = rx.recv().await else { break };
                timer.timer(BATCH_INTERVAL).await;
                let mut batch = vec![first];
                while let Ok(event) = rx.try_recv() {
                    batch.push(event);
                }
                let finished =
                    this.update(cx, |this, cx| this.apply_events(batch, cx)).unwrap_or(true);
                if finished {
                    break;
                }
            }
        }));
    }

    /// Applies one batch of stream events. Returns whether the reply is over.
    fn apply_events(&mut self, batch: Vec<StreamEvent>, cx: &mut Context<Self>) -> bool {
        let mut finished = false;
        for event in batch {
            match event {
                StreamEvent::Delta(text) => {
                    if let Some(turn) =
                        self.turns.last_mut().filter(|turn| turn.role == Role::Assistant)
                    {
                        turn.text.push_str(&text);
                    }
                }
                StreamEvent::Done => finished = true,
                StreamEvent::Error(message) => {
                    // A reply that never started leaves no empty bubble behind; one that
                    // half-arrived keeps its words, with the error after them.
                    if self
                        .turns
                        .last()
                        .is_some_and(|t| t.role == Role::Assistant && t.text.is_empty())
                    {
                        self.turns.pop();
                    }
                    self.turns.push(ChatTurn { role: Role::Note, text: message });
                    finished = true;
                }
            }
        }
        if finished {
            self.streaming = false;
            self.kill.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
        }
        // Growth keeps the newest text on screen — the reply is what the user is reading.
        self.scroll.scroll_to_bottom();
        cx.notify();
        finished
    }

    /// The Cancel button, and Esc while streaming: kill the child, keep the words that
    /// made it, say so.
    fn cancel_stream(&mut self, cx: &mut Context<Self>) {
        if !self.streaming {
            return;
        }
        // Codex gets the polite cancel first: `turn/interrupt` stops the turn server-side
        // so the subscription is not billed for tokens nobody will read. The kill below
        // is still the guarantee — an interrupt the CLI never gets around to reading must
        // not leave a turn running behind a panel the user has stopped listening to.
        self.interrupt_codex();
        if let Some(mut child) =
            self.kill.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take()
        {
            // Killing also closes the pipe, which is what unblocks the background reader
            // (the test runner learned this the 30-second way).
            let _ = child.kill();
            let _ = child.wait();
        }
        // Dropping the drain stops the await; the producer ends when its send fails.
        self.stream_task = None;
        self.streaming = false;
        if let Some(turn) = self.turns.last_mut().filter(|turn| turn.role == Role::Assistant) {
            if turn.text.is_empty() {
                turn.text = "(cancelled)".to_string();
            } else {
                turn.text.push_str("\n(cancelled)");
            }
        }
        cx.notify();
    }

    /// Asks the Codex CLI to stop the running turn, and drops the session with it.
    ///
    /// The session goes because the child is about to be killed: keeping a thread id
    /// whose process is gone would make the next send write into a closed pipe. The next
    /// turn re-handshakes, which costs a second and is the honest price of a cancel.
    ///
    /// A failed write is deliberately ignored — the child may already be dead, and that
    /// is the outcome this method wanted anyway.
    fn interrupt_codex(&mut self) {
        let mut state = self.codex.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(mut session) = state.session.take() {
            let request = crate::ai_codex::interrupt_request(&session.thread_id);
            let _ = session.stdin.write_all(request.as_bytes());
            let _ = session.stdin.flush();
        }
        state.model = None;
    }

    // --- chips -----------------------------------------------------------------------

    fn toggle_selection_chip(&mut self, cx: &mut Context<Self>) {
        if self.attach_selection {
            self.attach_selection = false;
        } else {
            let snap = (self.snapshot)(cx);
            if snap.selection.as_deref().is_none_or(str::is_empty) {
                self.note = Some("Nothing is selected in the editor".into());
            } else {
                self.attach_selection = true;
                self.note = None;
            }
        }
        cx.notify();
    }

    /// The denylist runs *here*, at the click, so the refusal is immediate and attached
    /// to the gesture that earned it — not discovered at send time three minutes later.
    fn toggle_file_chip(&mut self, cx: &mut Context<Self>) {
        if self.attach_file {
            self.attach_file = false;
        } else {
            let snap = (self.snapshot)(cx);
            match snap.file {
                None => self.note = Some("No file is open to attach".into()),
                Some((path, _)) => match ai::deny_reason(&path) {
                    Some(reason) => self.note = Some(deny_note(&path, reason).into()),
                    None => {
                        self.attach_file = true;
                        self.note = None;
                    }
                },
            }
        }
        cx.notify();
    }

    // ponytail: no "+ file" picker chip in this pass (#99 lists one). It needs a file
    // palette wired into a sibling entity, and the two chips above establish the
    // attach/deny mechanics it will reuse.

    // --- keyboard --------------------------------------------------------------------

    /// The find bar's keystroke filter, for the same reason it exists there: modified
    /// keys and named keys belong to actions, `key_char` is what typing means.
    fn on_key_down(&mut self, event: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform
            || keystroke.modifiers.control
            || keystroke.modifiers.function
        {
            return;
        }
        if matches!(
            keystroke.key.as_str(),
            "enter" | "escape" | "up" | "down" | "backspace" | "tab" | "left" | "right"
        ) {
            return;
        }
        let Some(text) = keystroke.key_char.as_deref() else { return };
        if text.is_empty() || text.chars().all(|c| c.is_control()) {
            return;
        }
        self.input.push_str(text);
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, _w: &mut Window, cx: &mut Context<Self>) {
        self.input.pop();
        cx.notify();
    }

    fn confirm(&mut self, _: &Confirm, _w: &mut Window, cx: &mut Context<Self>) {
        self.send(cx);
    }

    fn cancel(&mut self, _: &Cancel, _w: &mut Window, cx: &mut Context<Self>) {
        self.cancel_stream(cx);
    }
}

// --- test seams ------------------------------------------------------------------------

#[cfg(test)]
impl AiChatPanel {
    /// Types into the input the way `on_key_down` would — the find bar's seam, for the
    /// find bar's reason (the test harness cannot synthesise a `key_char`).
    pub fn type_input_for_test(&mut self, text: &str, cx: &mut Context<Self>) {
        self.input.push_str(text);
        cx.notify();
    }

    /// Runs the real send path with the transport stopped at the curl spawn.
    pub fn send_for_test(&mut self, cx: &mut Context<Self>) {
        self.transport_disabled = true;
        self.force_enabled = true;
        self.send(cx);
    }

    pub fn turns_for_test(&self) -> &[ChatTurn] {
        &self.turns
    }

    pub fn sent_bodies_for_test(&self) -> &[String] {
        &self.sent_bodies
    }

    /// Seeds a conversation so a render test can draw a populated panel.
    pub fn seed_turns_for_test(&mut self, turns: Vec<ChatTurn>, cx: &mut Context<Self>) {
        self.turns = turns;
        cx.notify();
    }
}

// --- pure logic ------------------------------------------------------------------------

/// Which context chip a click landed on — a value the handler can match on, so the two
/// chips share one renderer without a boxed closure per chip.
#[derive(Clone, Copy)]
enum Chip {
    Selection,
    CurrentFile,
}

/// A piece of an assistant reply: prose, or the inside of a ``` fence.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Segment {
    Text(String),
    Code(String),
}

/// Splits reply text on ``` fences. No markdown beyond that — #99's scope is "plain text
/// plus code fences", and a half-markdown renderer misrenders more than it renders.
///
/// An *unclosed* trailing fence still yields a code segment: mid-stream, the closing
/// fence has simply not arrived yet, and the text should already render as code.
pub fn split_fences(text: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut in_code = false;
    for line in text.split('\n') {
        // The whole line is the fence; an opening fence's language tag is dropped
        // (highlighting is out of scope, the monospace box is the rendering).
        if line.trim_start().starts_with("```") {
            if !current.is_empty() {
                let segment = std::mem::take(&mut current);
                segments.push(if in_code {
                    Segment::Code(segment)
                } else {
                    Segment::Text(segment)
                });
            }
            in_code = !in_code;
            continue;
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        segments.push(if in_code { Segment::Code(current) } else { Segment::Text(current) });
    }
    segments
}

/// Builds the context blocks a send prepends to the user's prompt: fenced, each with a
/// header line naming what it is, so the model — and anyone reading the transcript —
/// can tell context from question.
///
/// The deny check lives *in* the builder, not only on the chip: everything that reaches
/// a request body goes through here, so here is where refusal is unforgeable.
pub fn build_context(
    file: Option<(&Path, &str)>,
    selection: Option<&str>,
) -> Result<String, String> {
    let mut out = String::new();
    if let Some((path, content)) = file {
        if let Some(reason) = ai::deny_reason(path) {
            return Err(deny_note(path, reason));
        }
        out.push_str(&format!(
            "Context — file {}:\n```\n{}\n```\n\n",
            path.display(),
            content.trim_end_matches('\n')
        ));
    }
    if let Some(selection) = selection.filter(|text| !text.is_empty()) {
        out.push_str(&format!(
            "Context — selection:\n```\n{}\n```\n\n",
            selection.trim_end_matches('\n')
        ));
    }
    Ok(out)
}

/// The refusal, worded as a statement rather than an apology: there is no override (#99).
fn deny_note(path: &Path, reason: &str) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    format!("{name} stays local — {reason}")
}

/// What stands in the conversation area before the panel can work: which provider is
/// selected and what is missing. `None` means the panel is ready for an input row.
///
/// `codex_status` is the cached [`crate::ai_codex::availability`] answer: `None` while the
/// probe is still running, and consulted only when Codex is the selected provider.
pub fn setup_guidance(
    enabled: bool,
    provider: ai::Provider,
    base_url: &str,
    codex_status: Option<&Result<(), String>>,
) -> Option<String> {
    if !enabled {
        return Some(format!(
            "AI chat is off. Provider: {}. Nothing is sent anywhere until you enable it \
             and press send.",
            provider.setting_name()
        ));
    }
    if provider == ai::Provider::Custom && base_url.trim().is_empty() {
        return Some(
            "The custom provider needs \"ai.base_url\" in settings.json \
             (e.g. http://localhost:11434/v1 for Ollama)."
                .to_string(),
        );
    }
    if provider == ai::Provider::Codex {
        return match codex_status {
            // The probe's own sentence already names the command to run — repeating it
            // here in different words is how two error messages start disagreeing.
            Some(Err(reason)) => Some(format!(
                "{reason}.\n\nChat runs your local `codex` CLI, which uses your own \
                 ChatGPT login. This editor never sees your credentials, and the thread \
                 is read-only — it can read the open project, not write to it."
            )),
            // Still probing: no input row yet, because a send would race the answer.
            None => Some("Checking for the Codex CLI…".to_string()),
            Some(Ok(())) => None,
        };
    }
    None
}

// --- the blocking producer -------------------------------------------------------------

/// Resolves auth, runs `curl`, and forwards parsed events. Blocking on purpose: it runs
/// inside `background_spawn`, and a blocked send on the channel is the drain applying a
/// batch — the natural backpressure.
fn stream_reply(
    provider: ai::Provider,
    wire: Wire,
    base_url: &str,
    body: &str,
    kill: &Arc<Mutex<Option<Child>>>,
    tx: &smol::channel::Sender<StreamEvent>,
) {
    // Keychain and `ant` CLI subprocesses — the reason this whole function is off the
    // main thread. The error strings are already user-facing (resolve_auth's contract).
    let auth = match ai::resolve_auth(provider, base_url) {
        Ok(auth) => auth,
        Err(message) => {
            let _ = tx.send_blocking(StreamEvent::Error(message));
            return;
        }
    };

    let mut command = std::process::Command::new("curl");
    command
        .args(ai::curl_args(&auth))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Silenced rather than piped: `-sS` keeps real errors on stderr, but the useful
        // failure text (auth refusals, bad models) arrives as a JSON *body* on stdout,
        // which is what parse_error_body below reads.
        .stderr(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            let _ = tx.send_blocking(StreamEvent::Error(format!("could not run curl: {err}")));
            return;
        }
    };

    // The body goes via stdin (never argv — `ps` shows argv). Dropping the handle closes
    // the pipe, which is what tells curl the body is complete.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(body.as_bytes());
    }
    let stdout = child.stdout.take();
    // Parked in the shared slot so cancel can reach it while the read below blocks.
    *kill.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(child);

    let mut saw_delta = false;
    let mut stream_ended = false;
    let mut raw = String::new();
    if let Some(stdout) = stdout {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            // Kept for parse_error_body; capped so a runaway stream cannot hoard memory.
            if raw.len() < 64 * 1024 {
                raw.push_str(&line);
                raw.push('\n');
            }
            let Some(event) = ai::parse_sse(wire, &line) else { continue };
            match &event {
                StreamEvent::Delta(_) => saw_delta = true,
                StreamEvent::Done | StreamEvent::Error(_) => stream_ended = true,
            }
            // A closed channel means the drain was dropped — a cancel — and the kill
            // that caused it is also about to end this read.
            if tx.send_blocking(event).is_err() || stream_ended {
                break;
            }
        }
    }

    // Reap the child. `None` here means cancel already took and killed it.
    let status = kill
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .and_then(|mut child| child.wait().ok());
    if stream_ended {
        return;
    }
    match status {
        // curl failed with no SSE stream: the collected stdout is the server's refusal
        // as a plain JSON body, or nothing useful at all — say which.
        Some(status) if !status.success() && !saw_delta => {
            let message = ai::parse_error_body(&raw)
                .unwrap_or_else(|| format!("the request failed ({status})"));
            let _ = tx.send_blocking(StreamEvent::Error(message));
        }
        // EOF without a terminal event (a custom server that just closes): the reply is
        // whatever arrived, and it is over.
        _ => {
            let _ = tx.send_blocking(StreamEvent::Done);
        }
    }
}

// --- the Codex producer -----------------------------------------------------------------

/// Runs one Codex turn, spawning and handshaking the child first if there is not one yet.
///
/// Blocking, like [`stream_reply`] and for the same reason: it lives inside
/// `background_spawn`, and a blocked channel send is the drain applying a batch.
///
/// The child is *not* killed on the way out — that is the whole point of a session. It is
/// killed by a cancel, or when the panel drops and takes the `Arc` with it.
fn codex_turn(
    codex: &Arc<Mutex<CodexState>>,
    kill: &Arc<Mutex<Option<Child>>>,
    root: Option<&Path>,
    message: &str,
    tx: &smol::channel::Sender<StreamEvent>,
) {
    // Availability first, so "not installed" and "not logged in" arrive as the sentence
    // that names the command to run, rather than as a spawn error nobody can act on.
    if let Err(reason) = crate::ai_codex::availability() {
        let _ = tx.send_blocking(StreamEvent::Error(reason));
        return;
    }

    // The lock is held for the whole turn. That is deliberate and cheap: only one turn
    // runs at a time (the panel refuses a second while `streaming`), and the alternative —
    // taking the session out and putting it back — loses the child on every early return.
    let mut state = codex.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    // An existing session means the CLI already holds this conversation, so a second
    // question is one `turn/start` on the same thread rather than a fresh handshake.
    if let Some(session) = state.session.as_mut() {
        let request = crate::ai_codex::turn_start_request(&session.thread_id, message);
        if session.stdin.write_all(request.as_bytes()).and_then(|()| session.stdin.flush()).is_ok()
        {
            read_codex_turn(session, tx);
            return;
        }
        // The child died between turns (a crash, a logout). Drop the corpse and fall
        // through to a fresh spawn rather than reporting a broken pipe at the user.
        state.session = None;
    }

    let mut command = std::process::Command::new("codex");
    command
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // The CLI's own logging goes to stderr and is none of the panel's business; the
        // protocol is entirely on stdout.
        .stderr(Stdio::null());
    // Rooting the child at the project is the fallback that actually works if a future
    // CLI stops honouring the `cwd` param — belt and braces, per the probe.
    if let Some(root) = root {
        command.current_dir(root);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            let _ = tx.send_blocking(StreamEvent::Error(format!(
                "could not run the Codex CLI: {err} — install it and run `codex login`"
            )));
            return;
        }
    };

    let Some(mut stdin) = child.stdin.take() else {
        let _ = tx.send_blocking(StreamEvent::Error("the Codex CLI gave no stdin".to_string()));
        return;
    };
    let stdout = child.stdout.take();
    *kill.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(child);

    let handshake = crate::ai_codex::initialize_request(env!("CARGO_PKG_VERSION"))
        + &crate::ai_codex::thread_start_request(root);
    if stdin.write_all(handshake.as_bytes()).and_then(|()| stdin.flush()).is_err() {
        let _ = tx.send_blocking(StreamEvent::Error(
            "the Codex CLI closed before the handshake finished".to_string(),
        ));
        return;
    }

    let Some(stdout) = stdout else {
        let _ = tx.send_blocking(StreamEvent::Error("the Codex CLI gave no stdout".to_string()));
        return;
    };
    let mut stdout = BufReader::new(stdout);

    // Read until the thread id arrives, then send the message that has been waiting for
    // an id to address. Nothing before `thread/start` can carry reply text, so this loop
    // forwards nothing — it is the handshake, not the turn.
    let mut thread_id = None;
    let mut model = None;
    let mut line = String::new();
    while thread_id.is_none() {
        line.clear();
        match stdout.read_line(&mut line) {
            Ok(0) | Err(_) => {
                let _ = tx.send_blocking(StreamEvent::Error(
                    "the Codex CLI closed before the thread opened — try `codex login`".to_string(),
                ));
                return;
            }
            Ok(_) => {}
        }
        match crate::ai_codex::parse_line(&line) {
            Some(crate::ai_codex::CodexEvent::ThreadStarted(id)) => {
                model = model_from_thread_start(&line);
                thread_id = Some(id);
            }
            // A refusal during the handshake (a bad param, a logged-out CLI) is the
            // user's answer, not a hang.
            Some(crate::ai_codex::CodexEvent::TurnFailed(message)) => {
                let _ = tx.send_blocking(StreamEvent::Error(message));
                return;
            }
            _ => {}
        }
    }
    let Some(thread_id) = thread_id else { return };

    let request = crate::ai_codex::turn_start_request(&thread_id, message);
    if stdin.write_all(request.as_bytes()).and_then(|()| stdin.flush()).is_err() {
        let _ = tx.send_blocking(StreamEvent::Error(
            "the Codex CLI closed before the turn started".to_string(),
        ));
        return;
    }

    state.model = model;
    let session = state.session.insert(CodexSession { stdin, stdout, thread_id });
    read_codex_turn(session, tx);
}

/// Forwards one turn's worth of events from an established session.
///
/// Returns at `turn/completed` / `turn/failed` — and *only* then — leaving the reader
/// parked mid-stream for the next turn. That is why the reader lives in the session: the
/// next turn's bytes may already be in this `BufReader`'s buffer.
fn read_codex_turn(session: &mut CodexSession, tx: &smol::channel::Sender<StreamEvent>) {
    let mut line = String::new();
    loop {
        line.clear();
        match session.stdout.read_line(&mut line) {
            // EOF: the CLI exited (killed by a cancel, crashed, logged out mid-turn).
            // Whatever arrived is the reply, and it is over.
            Ok(0) | Err(_) => {
                let _ = tx.send_blocking(StreamEvent::Done);
                return;
            }
            Ok(_) => {}
        }
        match crate::ai_codex::parse_line(&line) {
            Some(crate::ai_codex::CodexEvent::Delta(text)) => {
                if tx.send_blocking(StreamEvent::Delta(text)).is_err() {
                    return; // the drain went away: a cancel, and the kill is coming
                }
            }
            Some(crate::ai_codex::CodexEvent::TurnCompleted) => {
                let _ = tx.send_blocking(StreamEvent::Done);
                return;
            }
            Some(crate::ai_codex::CodexEvent::TurnFailed(message)) => {
                let _ = tx.send_blocking(StreamEvent::Error(message));
                return;
            }
            _ => {}
        }
    }
}

/// Digs the model name out of a `thread/start` reply, for the header row.
///
/// Parsed here rather than in [`crate::ai_codex`] because it is a display nicety, not
/// protocol: a missing model costs a label, never a turn.
fn model_from_thread_start(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    value.get("result")?.get("model")?.as_str().map(str::to_string)
}

// --- rendering -------------------------------------------------------------------------

impl Focusable for AiChatPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AiChatPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let fonts = Fonts::get(cx);
        let settings = crate::settings::current(cx);
        let enabled = {
            #[cfg(test)]
            {
                settings.ai_chat_enabled() || self.force_enabled
            }
            #[cfg(not(test))]
            {
                settings.ai_chat_enabled()
            }
        };
        let provider = ai::Provider::from_setting(settings.ai_chat_provider());
        let guidance =
            setup_guidance(enabled, provider, settings.ai_base_url(), self.codex_status.as_ref());
        let ready = guidance.is_none();

        // The header's right-hand label: which model is answering. Codex picks its own
        // (the subscription decides), so it is reported back from `thread/start` rather
        // than read from `ai.chat_model`, which it does not obey.
        let model = if provider == ai::Provider::Codex {
            self.codex
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .model
                .clone()
                .unwrap_or_else(|| "subscription".to_string())
        } else {
            settings.ai_chat_model().to_string()
        };

        div()
            .key_context(context::AI_CHAT)
            .track_focus(&self.focus_handle(cx))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .flex()
            .flex_col()
            // Fills the wrapper the workspace sizes, rather than declaring its own
            // width: since the owner's resize request the width lives on the workspace
            // and is dragged by a divider, and two owners of one number is how a panel
            // ends up ignoring the handle that is supposed to size it.
            .size_full()
            .overflow_hidden()
            .bg(theme.panel)
            .border_l_1()
            .border_color(theme.border)
            .text_size(fonts.ui_size)
            .text_color(theme.text)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child("AI Chat")
                    .child(div().text_color(theme.text_muted).text_size(px(11.0)).child(
                        SharedString::from(format!("{} · {model}", provider.setting_name())),
                    )),
            )
            .child(match guidance {
                // Setup guidance instead of an input that cannot work (#99's first-open).
                Some(guidance) => self.render_guidance(guidance, enabled, &theme, cx),
                None => self.render_conversation(&theme, &fonts),
            })
            .when(ready, |el| {
                el.children(self.note.clone().map(|note| {
                    div().px_2().py_1().text_color(theme.error).text_size(px(11.0)).child(note)
                }))
                .child(self.render_chips(&theme, cx))
                .child(self.render_input(&theme, cx))
            })
    }
}

impl AiChatPanel {
    fn render_guidance(
        &self,
        guidance: String,
        enabled: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let entity = cx.entity();
        // Codex has no key to set, so pointing at the key prompt would send the user
        // somewhere that cannot help them.
        let codex = ai::Provider::from_setting(crate::settings::current(cx).ai_chat_provider())
            == ai::Provider::Codex;
        let footer = if codex {
            "Provider lives in Settings (⌘,) → AI. Codex uses your own `codex login`."
        } else {
            "Provider and API key live in Settings (⌘,) → AI."
        };
        div()
            .flex_1()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .text_color(theme.text_muted)
            .child(SharedString::from(guidance))
            .when(!enabled, |el| {
                el.child(
                    div()
                        .id("ai-chat-enable")
                        .px_2()
                        .py_1()
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .border_1()
                        .border_color(theme.border)
                        .text_color(theme.text)
                        .hover(|el| el.bg(theme.hover))
                        .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                            // The one mutation door (#100): the same write path the
                            // settings panel's toggle uses.
                            crate::settings::update_settings(cx, |settings| {
                                settings.set_ai_chat_enabled(true);
                            });
                            entity.update(cx, |_this, cx| cx.notify());
                        })
                        .child("Enable AI chat"),
                )
            })
            .child(div().text_size(px(11.0)).child(footer))
            .into_any_element()
    }

    fn render_conversation(&self, theme: &Theme, fonts: &Fonts) -> gpui::AnyElement {
        let rows: Vec<gpui::AnyElement> = if self.turns.is_empty() {
            vec![
                div()
                    .p_3()
                    .text_color(theme.text_muted)
                    .child(
                        "Ask about the code you are looking at. Context is attached only \
                            via the chips below.",
                    )
                    .into_any_element(),
            ]
        } else {
            self.turns
                .iter()
                .enumerate()
                .map(|(index, turn)| self.render_turn(index, turn, theme, fonts))
                .collect()
        };

        div()
            .id("ai-chat-turns")
            .flex_1()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .children(rows)
            .into_any_element()
    }

    fn render_turn(
        &self,
        index: usize,
        turn: &ChatTurn,
        theme: &Theme,
        fonts: &Fonts,
    ) -> gpui::AnyElement {
        let is_last = index + 1 == self.turns.len();
        let (label, label_color) = match turn.role {
            Role::User => ("You", theme.accent),
            Role::Assistant => ("AI", theme.text_muted),
            Role::Note => ("!", theme.error),
        };

        let mut body: Vec<gpui::AnyElement> = Vec::new();
        if turn.role == Role::Assistant && turn.text.is_empty() && self.streaming && is_last {
            // The dim ellipsis while nothing has arrived: the reply exists, its words
            // do not yet.
            body.push(div().text_color(theme.text_muted).child("…").into_any_element());
        } else {
            for (seg_index, segment) in split_fences(&turn.text).into_iter().enumerate() {
                body.push(match segment {
                    Segment::Text(text) => render_prose(&text, theme),
                    Segment::Code(code) => render_code_block(index, seg_index, &code, theme, fonts),
                });
            }
        }

        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_size(px(10.0)).text_color(label_color).child(label))
            .children(body)
            .into_any_element()
    }

    fn render_chips(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let chip = |id: &'static str, label: &'static str, active: bool, which: Chip| {
            let entity = cx.entity();
            div()
                .id(id)
                .px_2()
                .py_0p5()
                .rounded(px(9.0))
                .cursor_pointer()
                .border_1()
                // Active is a border *and* a fill — the find bar's non-colour-alone rule.
                .border_color(if active { theme.accent } else { theme.border })
                .when(active, |el| el.bg(theme.selected))
                .when(!active, |el| el.text_color(theme.text_muted))
                .hover(|el| el.bg(theme.hover))
                .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                    entity.update(cx, |this, cx| match which {
                        Chip::Selection => this.toggle_selection_chip(cx),
                        Chip::CurrentFile => this.toggle_file_chip(cx),
                    });
                })
                .child(SharedString::from(if active {
                    format!("✓ {label}")
                } else {
                    label.to_string()
                }))
        };

        div()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(11.0))
            .child(chip("ai-chip-selection", "Selection", self.attach_selection, Chip::Selection))
            .child(chip("ai-chip-file", "Current file", self.attach_file, Chip::CurrentFile))
            .into_any_element()
    }

    fn render_input(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let empty = self.input.is_empty();
        let entity = cx.entity();
        let action_button = if self.streaming {
            let entity = entity.clone();
            div()
                .id("ai-chat-cancel")
                .px_2()
                .py_0p5()
                .rounded(px(4.0))
                .cursor_pointer()
                .text_color(theme.error)
                .hover(|el| el.bg(theme.hover))
                .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                    entity.update(cx, |this, cx| this.cancel_stream(cx));
                })
                .child("Cancel")
        } else {
            let entity = entity.clone();
            div()
                .id("ai-chat-send")
                .px_2()
                .py_0p5()
                .rounded(px(4.0))
                .cursor_pointer()
                .text_color(theme.accent)
                .hover(|el| el.bg(theme.hover))
                .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                    entity.update(cx, |this, cx| this.send(cx));
                })
                .child("Send")
        };

        div()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .child(
                // The find bar's field: no caret, focus shown by the border. Same
                // honesty — gpui has no text input element, and this does not fake one.
                div()
                    .flex_1()
                    .min_w(px(80.0))
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .px_2()
                    .rounded_sm()
                    .bg(theme.background)
                    .border_1()
                    .border_color(theme.accent)
                    .when(empty, |el| el.text_color(theme.text_muted))
                    .child(SharedString::from(if empty {
                        "Ask — Enter sends".to_string()
                    } else {
                        self.input.clone()
                    })),
            )
            .child(action_button)
            .into_any_element()
    }
}

/// Prose renders line by line so blank lines survive; a plain multi-line string child
/// would rely on text layout honouring `\n`, which is not a promise worth leaning on.
fn render_prose(text: &str, theme: &Theme) -> gpui::AnyElement {
    div()
        .flex()
        .flex_col()
        .text_color(theme.text)
        .children(text.split('\n').map(|line| {
            div().child(SharedString::from(if line.is_empty() {
                " ".to_string()
            } else {
                line.to_string()
            }))
        }))
        .into_any_element()
}

/// A code segment: monospace box on the editor's background, with a Copy button.
fn render_code_block(
    turn_index: usize,
    seg_index: usize,
    code: &str,
    theme: &Theme,
    fonts: &Fonts,
) -> gpui::AnyElement {
    let code_owned = code.to_string();
    div()
        .flex()
        .flex_col()
        .rounded(px(4.0))
        .bg(theme.background)
        .border_1()
        .border_color(theme.border)
        .child(
            div().flex().justify_end().px_1().child(
                div()
                    .id(("ai-copy", turn_index * 1000 + seg_index))
                    .px_1()
                    .cursor_pointer()
                    .text_size(px(10.0))
                    .text_color(theme.text_muted)
                    .hover(|el| el.text_color(theme.text))
                    .on_mouse_down(MouseButton::Left, {
                        let code = code_owned.clone();
                        move |_ev, _window, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.clone()));
                        }
                    })
                    .child("Copy"),
            ),
        )
        .child(
            div()
                .id(("ai-code", turn_index * 1000 + seg_index))
                .px_2()
                .pb_1()
                .overflow_x_scroll()
                .font_family(fonts.family.clone())
                .text_size(px(11.0))
                .flex()
                .flex_col()
                .children(code_owned.split('\n').map(|line| {
                    div().whitespace_nowrap().child(SharedString::from(if line.is_empty() {
                        " ".to_string()
                    } else {
                        line.to_string()
                    }))
                })),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn prose_and_fences_split_into_alternating_segments() {
        let reply = "Use this:\n```php\necho 1;\n```\nDone.";
        assert_eq!(
            split_fences(reply),
            vec![
                Segment::Text("Use this:".to_string()),
                Segment::Code("echo 1;".to_string()),
                Segment::Text("Done.".to_string()),
            ]
        );
    }

    #[test]
    fn an_unclosed_fence_is_code_because_streaming_has_not_finished_it() {
        // Mid-stream the closing fence has not arrived; the half-received block must
        // already render as code rather than flashing as prose until the fence lands.
        let partial = "Look:\n```\n$a = 1;";
        assert_eq!(
            split_fences(partial),
            vec![Segment::Text("Look:".to_string()), Segment::Code("$a = 1;".to_string())]
        );
    }

    #[test]
    fn a_reply_with_no_fences_is_one_text_segment() {
        assert_eq!(split_fences("hello\nworld"), vec![Segment::Text("hello\nworld".to_string())]);
        assert_eq!(split_fences(""), Vec::<Segment>::new());
    }

    #[test]
    fn context_blocks_carry_the_file_name_and_fence_both_attachments() {
        let blocks = build_context(
            Some((&PathBuf::from("app/Models/User.php"), "<?php\nclass User {}\n")),
            Some("class User {}"),
        )
        .unwrap();
        assert!(blocks.contains("Context — file app/Models/User.php:"), "{blocks}");
        assert!(blocks.contains("Context — selection:"), "{blocks}");
        assert_eq!(blocks.matches("```").count(), 4, "two fenced blocks: {blocks}");
    }

    #[test]
    fn the_context_builder_refuses_a_dotenv_no_matter_who_asks() {
        // The chip checks at attach time; this is the layer that makes the refusal hold
        // even if a path turns into a secret between attach and send (#99: no override).
        let refusal = build_context(Some((&PathBuf::from(".env"), "APP_KEY=oops")), None);
        let message = refusal.unwrap_err();
        assert!(message.contains(".env"), "{message}");
        assert!(message.contains("credentials"), "the reason travels with the refusal");
    }

    #[test]
    fn no_attachments_build_an_empty_prefix() {
        assert_eq!(build_context(None, None).unwrap(), "");
        assert_eq!(build_context(None, Some("")).unwrap(), "", "an empty selection is nothing");
    }

    #[test]
    fn guidance_stands_in_while_disabled_or_unconfigured_and_leaves_when_ready() {
        let disabled = setup_guidance(false, ai::Provider::Anthropic, "", None);
        assert!(disabled.unwrap().contains("anthropic"), "guidance names the provider");

        let no_url = setup_guidance(true, ai::Provider::Custom, "  ", None);
        assert!(no_url.unwrap().contains("ai.base_url"), "guidance names what is missing");

        assert_eq!(setup_guidance(true, ai::Provider::Anthropic, "", None), None);
        assert_eq!(
            setup_guidance(true, ai::Provider::Custom, "http://localhost:11434", None),
            None
        );
    }

    /// An unavailable Codex must say the command that fixes it, never fail silently (#99).
    #[test]
    fn codex_guidance_carries_the_command_that_fixes_it() {
        let missing = Err("Codex CLI not found — install it and run `codex login`".to_string());
        let guidance = setup_guidance(true, ai::Provider::Codex, "", Some(&missing)).unwrap();
        assert!(guidance.contains("codex login"), "the exact command to run: {guidance}");
        assert!(
            guidance.contains("never sees your credentials"),
            "and who owns the login: {guidance}"
        );

        let logged_out =
            Err("Codex is installed but not logged in — run `codex login` in a terminal"
                .to_string());
        let guidance = setup_guidance(true, ai::Provider::Codex, "", Some(&logged_out)).unwrap();
        assert!(guidance.contains("not logged in"), "{guidance}");
        assert!(guidance.contains("codex login"), "{guidance}");

        // While the probe is in flight there is still no input row: a send would race it.
        assert!(setup_guidance(true, ai::Provider::Codex, "", None).is_some());

        // Available: the panel is ready, and the base URL is none of Codex's business.
        assert_eq!(setup_guidance(true, ai::Provider::Codex, "", Some(&Ok(()))), None);
    }
}
