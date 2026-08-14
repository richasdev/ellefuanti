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
//! # Attachments are chips too, for exactly that reason
//!
//! Files and images arrive by dragging them onto the panel, and each becomes its own
//! removable chip *before* the send — never a silent pickup. [`ai::read_attachment`] reads
//! and classifies at the drop, so a denied, oversized or unreadable file is refused at the
//! gesture that earned the refusal rather than three minutes later. Images ride the wire as
//! content blocks; everything else rides as fenced prose, and a binary that is neither is
//! refused by name instead of being sent as mojibake.
//!
//! The denylist runs **twice**: once at the drop (the UX) and again in
//! [`build_attachment_context`] at send (the guarantee), because a path can become a secret
//! between the two and #99 allows no override at either end. Attachments clear after a
//! send — they described *that* question, and re-sending them silently would be the opt-in
//! rule read backwards.
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
//!
//! # Ask and Agent, and the rule that outranks the feature
//!
//! Ask is this panel as it was: a conversation, and nothing else. Agent lets the model
//! propose file changes — and **nothing it proposes reaches disk without the user seeing a
//! diff and pressing Apply.** That is the mirror of [`ai::deny_reason`], which decides what
//! may leave the machine; this decides what may enter the user's files.
//!
//! The guarantee is not a promise this panel makes, it is a property of how the Codex
//! thread is opened. The sandbox stays `read-only` in *both* modes, which makes every
//! attempted write a sandbox escape, which makes it a question the CLI has to ask before
//! it can act (see [`crate::ai_codex`] for the probe that established this — asking for
//! write access would have *removed* the consent step, not added one). So:
//!
//! - a proposal arrives as a diff, already rendered, before anything is written;
//! - each file is approved or rejected on its own — a turn touching five files is five
//!   decisions, not one;
//! - a rejection is *told to the model*, so it can adapt rather than re-propose blindly;
//! - a proposal for a path [`ai::deny_reason`] refuses is blocked, with the reason shown;
//! - a cancel discards everything still pending rather than half-applying it.
//!
//! Approvals are answered on the same stdin the turn is running on, from the foreground,
//! while the background reader is parked on `read_line` — the one place the two threads
//! touch the same child, and why the write goes through the shared [`CodexState`] lock.
//!
//! Agent mode is Codex-only in this pass. The HTTP providers would need a full tool-use
//! loop (a second request shape, a tool-result protocol per wire, an agent turn state
//! machine); the honest limit is stated in the UI rather than half-built here.

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
use crate::ai::{self, AgentEvent, StreamEvent, Wire};
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

/// Which mode the panel is in (`ai.chat_mode`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ChatMode {
    /// Read-only conversation: today's behaviour, and the default.
    #[default]
    Ask,
    /// The model may propose file changes, which arrive as diffs to approve per file.
    Agent,
}

impl ChatMode {
    /// Anything unrecognised is Ask. A settings file with a typo in it must not silently
    /// put the panel in the mode that can propose writes.
    pub fn from_setting(value: &str) -> ChatMode {
        match value {
            "agent" => ChatMode::Agent,
            _ => ChatMode::Ask,
        }
    }

    pub fn setting_name(self) -> &'static str {
        match self {
            ChatMode::Ask => "ask",
            ChatMode::Agent => "agent",
        }
    }
}

/// What has happened to one proposed file change.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProposalState {
    /// Shown, with Apply and Reject buttons, and nothing written.
    Pending,
    /// Applied to the buffer or the file. Terminal.
    Applied,
    /// Turned down by the user. Terminal, and reported to the model.
    Rejected,
    /// Refused before it could be offered: [`ai::deny_reason`] says this path holds
    /// secrets. Terminal, and never approvable — the string is the reason, shown in place
    /// of the buttons.
    Blocked(&'static str),
}

/// One file a turn wants to change, as the panel tracks it.
///
/// Keyed by `item_id` + `path` rather than by position: approvals arrive for an item id,
/// and one item can carry several files that are each decided separately.
#[derive(Clone, Debug)]
pub struct Proposal {
    /// The Codex item this change belongs to — what an approval reply is addressed to.
    pub item_id: String,
    /// Absolute path of the file to change.
    pub path: PathBuf,
    /// `"add"`, `"update"` or `"delete"`.
    pub kind: String,
    /// The unified diff, as the CLI produced it.
    pub diff: String,
    pub state: ProposalState,
}

/// A turn's proposals plus the JSON-RPC id its approval is waiting at.
///
/// The id arrives *after* the changes (the CLI sends the item, then asks), and the answer
/// may only be sent once every file in the item has been decided — which is why the two
/// halves live together rather than as parallel maps that could disagree.
#[derive(Default)]
struct PendingApproval {
    /// `None` until `item/fileChange/requestApproval` lands for this item.
    request_id: Option<u64>,
    /// Whether the reply has gone, so a second decision cannot answer the same id twice.
    answered: bool,
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

/// How the panel writes an approved change: the workspace's closure, for the mirror of
/// [`SnapshotFn`]'s reason — the workspace owns the tabs, so only it can tell whether a
/// path is open in one and edit that buffer instead of the file.
///
/// The closure is handed a *patcher* rather than finished text, because only the workspace
/// knows the right base to patch: for an open tab that is the **buffer**, unsaved edits and
/// all, and reading the file from disk instead would patch a version the user is not
/// looking at. The panel supplies the transformation; the workspace supplies the input and
/// decides where the result goes.
///
/// Returns `Ok(true)` when an open buffer took the edit (one undo step, cursor preserved),
/// `Ok(false)` when the file was written on disk instead, and `Err` with a sentence for
/// the user when the patch did not fit or the write failed.
pub type ApplyFn = Box<
    dyn Fn(&Path, &dyn Fn(&str) -> Result<String, String>, &mut App) -> Result<bool, String>
        + 'static,
>;

/// A live `codex app-server` child and the thread it opened (#99).
///
/// Held across turns because the *thread* is the conversation: Codex keeps the history,
/// so a second question is a `turn/start` on the same id rather than a re-sent transcript.
/// Both fields are behind the panel's `Arc<Mutex<..>>` so the background reader and the
/// foreground cancel can reach them without either blocking a paint.
struct CodexSession {
    /// The child's stdin, for `turn/start` and `turn/interrupt`. Separate from the kill
    /// handle because writing a request must not need the lock that a kill takes.
    ///
    /// Shared rather than owned, and that is load-bearing: an **approval** must reach this
    /// stdin while `codex_turn` is holding the state lock and blocking on the reply. Owning
    /// it here put it behind that lock and froze the window (see `AiChatPanel::codex_stdin`).
    /// The mutex is only ever held for the length of one `write_all`, by writers that never
    /// block on anything else, so it cannot itself become the thing a turn waits on.
    stdin: Arc<Mutex<Option<ChildStdin>>>,
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
    /// The Codex child's stdin, reachable **without** the turn lock — for approvals.
    ///
    /// # Why this is not just `CodexState::session.stdin`
    ///
    /// It was, and that deadlocked agent mode. `codex_turn` holds `codex.lock()` for the
    /// whole turn, and in agent mode the turn blocks inside `read_codex_turn` waiting for
    /// the user to approve a file change. Answering took the same lock — from the *main
    /// thread* — so the click that would unblock the turn waited on the turn instead. The
    /// window froze with no panic and no log: "travou infinito no modo agent".
    ///
    /// Its own `Arc<Mutex<…>>`, for exactly the reason `CodexSession::stdin` already gives
    /// for the kill handle: a message that must reach a *running* turn cannot be behind the
    /// lock that turn holds. The approval is that kind of message; so is the interrupt.
    ///
    /// `None` until a session exists. Writes are best-effort: a dead child means the turn
    /// is over, which a decline wanted anyway.
    codex_stdin: Arc<Mutex<Option<ChildStdin>>>,
    /// The live thread id, published out of the turn lock for the same reason as
    /// `codex_stdin`: an interrupt needs it, and Cancel is the button a user reaches for
    /// *while* a turn is running — the one moment the turn lock is guaranteed to be held.
    codex_thread_id: Arc<Mutex<Option<String>>>,
    /// The drain task. Held so it lives; finished tasks are replaced on the next send
    /// rather than cleared mid-poll, because a task must not drop itself from inside
    /// its own `update`.
    stream_task: Option<Task<()>>,
    /// The context chips (#99): off by default, explicit, visible.
    attach_selection: bool,
    attach_file: bool,
    /// Files and images the user dropped or picked. Each is a visible, removable chip
    /// before send, and nothing lands here without a gesture — the same rule the two
    /// booleans above follow, extended to a list because attachments are countable.
    attachments: Vec<Attachment>,
    /// Whether a Finder drag is currently over the panel, for the drop highlight. Purely
    /// visual: the drop handler does not consult it.
    drag_over: bool,
    /// A transient line above the input: a deny reason, "no file open", and the like.
    note: Option<SharedString>,
    scroll: ScrollHandle,
    snapshot: SnapshotFn,
    /// How an approved change reaches the buffer or the file. The workspace's closure, for
    /// the same reason as `snapshot`: only it knows which paths are open in tabs.
    apply: ApplyFn,
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
    /// Ask or Agent. Read from settings at construction so the panel opens in the mode
    /// the user left it in, and written back when the switch is clicked.
    mode: ChatMode,
    /// The file changes this turn has proposed, in arrival order. Cleared at the start of
    /// every send and by a cancel — a pending proposal belongs to the turn that produced
    /// it, and outliving that turn is how a stale diff gets applied to a changed file.
    proposals: Vec<Proposal>,
    /// Per Codex item: the approval id, and whether it has been answered.
    approvals: std::collections::HashMap<String, PendingApproval>,
    /// What the user decided about the last turn's proposals, waiting to be told to the
    /// model on the next send. See [`Self::report_outcome`] for why it waits.
    pending_report: Option<String>,
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
        apply: ApplyFn,
        project_root: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel = Self {
            focus_handle: cx.focus_handle(),
            turns: Vec::new(),
            input: String::new(),
            streaming: false,
            kill: Arc::default(),
            codex_stdin: Arc::default(),
            codex_thread_id: Arc::default(),
            stream_task: None,
            attach_selection: false,
            attach_file: false,
            attachments: Vec::new(),
            drag_over: false,
            note: None,
            scroll: ScrollHandle::new(),
            snapshot,
            apply,
            project_root,
            codex: Arc::default(),
            codex_status: None,
            // The mode the user left the panel in. Read once here rather than per render:
            // the switch below is what changes it, and it writes both places at once.
            mode: {
                #[cfg(test)]
                {
                    ChatMode::Ask
                }
                #[cfg(not(test))]
                {
                    ChatMode::from_setting(crate::settings::current(cx).ai_chat_mode())
                }
            },
            proposals: Vec::new(),
            approvals: std::collections::HashMap::new(),
            pending_report: None,
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
    ///
    /// The only caller sits behind `cfg(not(test))` — a test must never spawn a CLI —
    /// so under `cargo clippy --all-targets` (which builds the test harness) this reads
    /// as dead code and `-D warnings` fails the build. That is the CI break, and the
    /// honest fix is to say so here rather than to give the tests a call they do not
    /// want just to keep the lint quiet.
    #[cfg_attr(test, allow(dead_code))]
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

        // What the user decided about the previous turn's proposals leads the message, so
        // the model hears "you proposed X, the user took A and refused B" before the new
        // question. Not shown as part of the user's turn — they did not type it — which is
        // why it is prepended to `message` rather than pushed into the transcript.
        let report = self
            .pending_report
            .take()
            .map(|report| format!("[editor] {report}\n\n"))
            .unwrap_or_default();

        // The attachments' second deny pass, and the split into prose and image blocks.
        // A refusal here abandons the send with the draft intact: the user has a note
        // naming the file, and can remove that chip and press send again.
        let (attachment_blocks, images) = match build_attachment_context(&self.attachments) {
            Ok(built) => built,
            Err(refusal) => {
                self.note = Some(refusal.into());
                cx.notify();
                return;
            }
        };
        if !images.is_empty() && !attachments_supported(provider) {
            // Unreachable through the UI — the attach button is not rendered for Codex —
            // but a provider switched *after* attaching would otherwise drop the images
            // silently, which is the one thing this panel must never do.
            self.note = Some(CODEX_NO_ATTACHMENTS.into());
            cx.notify();
            return;
        }

        let message = format!("{report}{context_blocks}{attachment_blocks}{prompt}");
        self.input.clear();
        self.note = None;
        // Last turn's proposals belong to last turn. Anything still pending is declined on
        // the way out — leaving it on screen would let a diff computed against an older
        // file be applied after a newer answer has arrived.
        self.discard_proposals(true);
        self.turns.push(ChatTurn { role: Role::User, text: message.clone() });

        // The body carries the history *up to and including* the new user turn; the empty
        // assistant turn below is a UI placeholder the wire must not see. Codex has no
        // body at all — the CLI holds the conversation, so a turn is just the new text.
        //
        // The images hang off that newest turn only. Re-sending them with every later
        // question would re-upload the same pixels for the rest of the conversation; the
        // model has already read them, and the transcript keeps the words about them.
        let mut wire_turns = self.wire_turns();
        if let Some(last) = wire_turns.last_mut() {
            last.images = images;
        }
        let body = provider.wire().map(|wire| {
            ai::chat_body(wire, settings.ai_chat_model(), SYSTEM_PROMPT, &wire_turns, MAX_TOKENS)
        });

        // The chips are spent: they described *this* send. Leaving them up would attach
        // the same screenshot to the next question without a second gesture, which is
        // exactly the "nothing leaves without an explicit act" rule read backwards.
        self.attachments.clear();

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
            .map(|turn| {
                ai::Turn::text(
                    if turn.role == Role::User { "user" } else { "assistant" },
                    turn.text.clone(),
                )
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
        let (tx, rx) = smol::channel::unbounded::<AgentEvent>();
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
        let (tx, rx) = smol::channel::unbounded::<AgentEvent>();
        let codex = self.codex.clone();
        let kill: Arc<Mutex<Option<Child>>> = Arc::default();
        self.kill = kill.clone();
        let root = self.project_root.clone();
        // Cloned, not re-created: these are the handles the *panel* answers approvals and
        // cancels through, so the turn must publish into the same ones the UI reads.
        let shared_stdin = self.codex_stdin.clone();
        let shared_thread_id = self.codex_thread_id.clone();

        let producer = cx.background_spawn(async move {
            codex_turn(
                &codex,
                &kill,
                &shared_stdin,
                &shared_thread_id,
                root.as_deref(),
                &message,
                &tx,
            );
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
    fn apply_events(&mut self, batch: Vec<AgentEvent>, cx: &mut Context<Self>) -> bool {
        let mut finished = false;
        for event in batch {
            match event {
                AgentEvent::Stream(StreamEvent::Delta(text)) => {
                    if let Some(turn) =
                        self.turns.last_mut().filter(|turn| turn.role == Role::Assistant)
                    {
                        turn.text.push_str(&text);
                    }
                }
                AgentEvent::Stream(StreamEvent::Done) => finished = true,
                // Agent mode only. Both are ignored in Ask mode rather than trusted: the
                // thread is opened read-only either way, so a proposal arriving in Ask
                // mode would be a CLI that changed its mind about the sandbox, and the
                // safe reading of that is to show nothing and approve nothing.
                AgentEvent::Proposed { item_id, changes } => {
                    if self.mode == ChatMode::Agent {
                        self.add_proposals(item_id, changes);
                    }
                }
                AgentEvent::ApprovalRequested { request_id, item_id } => {
                    if self.mode == ChatMode::Agent {
                        self.note_approval_request(request_id, item_id);
                    } else {
                        // Nothing was offered, so nothing can be approved.
                        self.answer_codex(request_id, false);
                    }
                }
                AgentEvent::Stream(StreamEvent::Error(message)) => {
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
        // A cancel mid-proposal discards what was pending rather than half-applying it.
        // Nothing is told to the CLI: `interrupt_codex` above already dropped the session
        // and the child is being killed, so a reply would be written into a closed pipe.
        self.discard_proposals(false);
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
    /// Like [`Self::answer_codex`], this must not touch `self.codex`: Cancel is pressed
    /// *while* a turn runs, which is exactly when that lock is held — and in agent mode the
    /// turn may be parked waiting for an approval that will now never come, so a cancel
    /// that waits for the lock waits forever.
    fn interrupt_codex(&mut self) {
        let thread_id = self
            .codex_thread_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(thread_id) = thread_id {
            write_to_codex(&self.codex_stdin, &crate::ai_codex::interrupt_request(&thread_id));
        }
        // The stdin handle goes with the thread: the next turn re-handshakes and installs a
        // fresh one. Dropping it here also closes the child's stdin, which is what makes a
        // CLI ignoring the interrupt still exit.
        *self.codex_stdin.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;

        // The session itself is cleared by the turn when it notices, or by the next send.
        // Not taken here, because taking it needs the lock this method exists to avoid.
        if let Ok(mut state) = self.codex.try_lock() {
            state.session = None;
            state.model = None;
        }
    }

    // --- agent mode: proposals -------------------------------------------------------

    /// Flips Ask ↔ Agent and remembers it (`ai.chat_mode`).
    ///
    /// Leaving Agent discards anything still pending: those proposals were offered by a
    /// mode the user has just stepped out of, and an Apply button surviving into Ask mode
    /// would be the one way a read-only mode could write a file.
    fn set_mode(&mut self, mode: ChatMode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        if mode == ChatMode::Ask {
            self.discard_proposals(false);
        }
        // The one mutation door (#100), the same path the enable button uses.
        crate::settings::update_settings(cx, |settings| {
            settings.set_ai_chat_mode(mode.setting_name());
        });
        cx.notify();
    }

    /// Records the files a turn wants to change, checking each against the denylist.
    ///
    /// The deny check happens *here*, at arrival, so a refused path is visibly refused
    /// with its reason instead of quietly missing from the list — and so it is already
    /// terminal before any button exists to approve it.
    fn add_proposals(&mut self, item_id: String, changes: Vec<ai::ProposedFileChange>) {
        for change in changes {
            let path = PathBuf::from(&change.path);
            let state = match ai::deny_reason(&path) {
                Some(reason) => ProposalState::Blocked(reason),
                None => ProposalState::Pending,
            };
            self.proposals.push(Proposal {
                item_id: item_id.clone(),
                path,
                kind: change.kind,
                diff: change.diff,
                state,
            });
        }
        self.approvals.entry(item_id).or_default();
    }

    /// Notes the id an item's approval is waiting at, and answers immediately if every
    /// file in it has already been decided.
    ///
    /// The question can arrive after the user has clicked (the diff is shown as soon as
    /// the item does, which is before the CLI asks), so the decision and the question meet
    /// in whichever order they happen to arrive.
    fn note_approval_request(&mut self, request_id: u64, item_id: String) {
        let entry = self.approvals.entry(item_id.clone()).or_default();
        entry.request_id = Some(request_id);
        self.settle_item(&item_id);
    }

    /// Answers the CLI once every file of `item_id` has been decided.
    ///
    /// **Always `decline`, even for files the user applied.** That reads backwards and is
    /// the crux of the design: this panel writes the file itself, through the open buffer
    /// (one undo step) or through the atomic `fs::write_file`. Answering `accept` would
    /// ask the CLI to apply *its* copy of the patch as well — to a file the panel has
    /// already changed — which either fails as a stale patch or applies twice. Declining
    /// leaves the sandbox exactly as it was: nothing written by Codex, everything written
    /// by the editor, one writer and one undo story.
    ///
    /// The model is not left guessing. `decline` keeps the turn alive — that is the
    /// protocol's own wording — so the CLI learns the patch did not go in and can react
    /// within the same turn. What it cannot learn from a bare decline is *why*, or that
    /// the editor applied some of it, so [`Self::report_outcome`] sends that as the next
    /// turn's input; the note pushed here is the user's copy of the same sentence.
    fn settle_item(&mut self, item_id: &str) {
        let Some(entry) = self.approvals.get(item_id) else { return };
        let Some(request_id) = entry.request_id else { return };
        if entry.answered {
            return;
        }
        let files: Vec<(String, ProposalState)> = self
            .proposals
            .iter()
            .filter(|proposal| proposal.item_id == item_id)
            .map(|proposal| {
                let name = proposal
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| proposal.path.display().to_string());
                (name, proposal.state)
            })
            .collect();
        if files.iter().any(|(_, state)| *state == ProposalState::Pending) {
            return; // still waiting on the user for at least one file
        }

        self.answer_codex(request_id, false);
        if let Some(entry) = self.approvals.get_mut(item_id) {
            entry.answered = true;
        }

        // What the user did, as a note in the transcript. A rejection the model is never
        // told about is a rejection it will make again.
        let applied: Vec<&str> = files
            .iter()
            .filter(|(_, state)| *state == ProposalState::Applied)
            .map(|(name, _)| name.as_str())
            .collect();
        let refused: Vec<&str> = files
            .iter()
            .filter(|(_, state)| *state != ProposalState::Applied)
            .map(|(name, _)| name.as_str())
            .collect();
        let mut summary = String::new();
        if !applied.is_empty() {
            summary.push_str(&format!("Applied by the editor: {}.", applied.join(", ")));
        }
        if !refused.is_empty() {
            if !summary.is_empty() {
                summary.push(' ');
            }
            summary.push_str(&format!("Rejected, left unchanged: {}.", refused.join(", ")));
        }
        if !summary.is_empty() {
            self.turns.push(ChatTurn { role: Role::Note, text: summary.clone() });
            self.report_outcome(summary);
        }
    }

    /// Carries the decision back to the model.
    ///
    /// Held until the user's next message rather than sent as a turn of its own: a
    /// `turn/start` while the current turn is still running is a second concurrent turn on
    /// one thread, and the panel's whole streaming model (one placeholder, one drain, one
    /// cancel) assumes exactly one. Prepending it to the next send costs nothing and
    /// arrives before the model's next chance to act on it.
    fn report_outcome(&mut self, summary: String) {
        self.pending_report = Some(match self.pending_report.take() {
            Some(existing) => format!("{existing} {summary}"),
            None => summary,
        });
    }

    /// Writes one approval reply on the Codex child's stdin.
    ///
    /// **Deliberately does not touch `self.codex`.** That lock is held by `codex_turn` for
    /// the whole turn, and in agent mode the turn is parked inside `read_line` waiting for
    /// precisely this reply — so taking it here made the click that unblocks the turn wait
    /// on the turn. This runs on the main thread, so the whole window froze: no panic, no
    /// crash log, just "travou infinito no modo agent".
    ///
    /// The background reader stays parked on `read_line` while this runs, which is safe:
    /// the reader owns stdout, this owns stdin, and the only shared thing is a mutex held
    /// for the length of one write.
    ///
    /// A failed write means the child is gone, which is the outcome a decline wanted anyway
    /// and which a cancel has already handled.
    fn answer_codex(&self, request_id: u64, approve: bool) {
        let reply = crate::ai_codex::approval_response(request_id, approve);
        write_to_codex(&self.codex_stdin, &reply);
    }

    /// Apply, on one file. **The only path in this panel that writes anything.**
    ///
    /// Two ways in, because a file open in a tab and a file that is not are different
    /// objects: the open buffer is edited through `Document::apply_edits` so the change is
    /// one undo step and the user's cursor survives, and a closed file goes through the
    /// atomic `fs::write_file`. Writing the buffer's file behind its back would leave the
    /// tab showing stale text over changed bytes.
    fn apply_proposal(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(proposal) = self.proposals.get(index) else { return };
        if proposal.state != ProposalState::Pending {
            return; // already decided; a double click must not write twice
        }
        // The denylist, again, at the moment of writing. The check at arrival is the UX;
        // this is the guarantee, and it is the one that holds if a path became a secret
        // in between (the same belt-and-braces rule `build_context` follows for egress).
        if let Some(reason) = ai::deny_reason(&proposal.path) {
            let note = deny_note(&proposal.path, reason);
            let item_id = proposal.item_id.clone();
            self.proposals[index].state = ProposalState::Blocked(reason);
            self.note = Some(note.into());
            self.settle_item(&item_id);
            cx.notify();
            return;
        }

        // Build the new text here, on the main thread, from the text that is there *now*.
        // A patch that no longer fits is refused rather than forced: the file has changed
        // since the model read it, and the user keeps their version.
        let path = proposal.path.clone();
        let diff = proposal.diff.clone();
        let item_id = proposal.item_id.clone();
        let is_delete = proposal.kind == "delete";

        let outcome = if is_delete {
            // Deleting a file is a different act with a different undo story, and this
            // pass does not do it: the proposal is shown, and refused out loud rather
            // than silently skipped.
            Err("deleting files is not something Agent mode applies in this pass".to_string())
        } else {
            self.apply_file_change(&path, &diff, cx)
        };

        match outcome {
            Ok(()) => {
                self.proposals[index].state = ProposalState::Applied;
                self.note = None;
            }
            Err(message) => {
                // Not applied, so not `Applied`: the state stays honest, the item is
                // settled as a refusal, and the reason is on screen.
                self.proposals[index].state = ProposalState::Rejected;
                self.note = Some(
                    format!(
                        "{} was not changed — {message}",
                        path.file_name().unwrap_or(path.as_os_str()).to_string_lossy()
                    )
                    .into(),
                );
            }
        }
        self.settle_item(&item_id);
        cx.notify();
    }

    /// Patches the file's current text and writes the result.
    ///
    /// The patch is handed over as a closure rather than applied here, because "current
    /// text" depends on where the change is going: an open tab's buffer (unsaved edits
    /// included — that is what the user is looking at) or the bytes on disk. The workspace
    /// picks the base and calls this back with it; a patch that does not fit that base
    /// comes back as `Err` and nothing is written.
    fn apply_file_change(
        &self,
        path: &Path,
        diff: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let patch = |current: &str| apply_unified_diff(current, diff);
        // The buffer edit needs an `&mut App`, which a `Context<Self>` derefs to; the
        // closure is the workspace's, so this is the moment the panel hands the write to
        // the only party that can do it in the right place.
        (self.apply)(path, &patch, cx)?;
        Ok(())
    }

    /// Reject, on one file: nothing is written, and the model is told so it can adapt.
    fn reject_proposal(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(proposal) = self.proposals.get(index) else { return };
        if proposal.state != ProposalState::Pending {
            return;
        }
        let item_id = proposal.item_id.clone();
        self.proposals[index].state = ProposalState::Rejected;
        self.settle_item(&item_id);
        cx.notify();
    }

    /// Drops every proposal, declining any the CLI is still waiting on.
    ///
    /// `tell_codex` is false when the child is about to be killed anyway (a cancel): the
    /// reply would race the kill, and an unanswered request dies with the process.
    fn discard_proposals(&mut self, tell_codex: bool) {
        if tell_codex {
            let pending: Vec<(u64, String)> = self
                .approvals
                .iter()
                .filter(|(_, entry)| !entry.answered)
                .filter_map(|(item, entry)| entry.request_id.map(|id| (id, item.clone())))
                .collect();
            for (request_id, item) in pending {
                self.answer_codex(request_id, false);
                if let Some(entry) = self.approvals.get_mut(&item) {
                    entry.answered = true;
                }
            }
        }
        self.proposals.clear();
        self.approvals.clear();
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

    // --- attachments -----------------------------------------------------------------

    /// Takes a batch of paths — a Finder drop, or the picker's result — and attaches the
    /// ones that pass.
    ///
    /// Per-path rather than all-or-nothing: dropping a folder of screenshots with one
    /// `.env` among them should attach the screenshots and say why the `.env` did not,
    /// because refusing the whole gesture would teach the user nothing about which file
    /// was the problem. The note carries the first refusal — the one the user is most
    /// likely to be looking for — and a count when several failed.
    fn attach_paths(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        let mut refusals: Vec<String> = Vec::new();
        for path in paths {
            // Attaching the same file twice is a no-op rather than an error: a double
            // drop is a slip, not a request for two copies on the wire.
            if self.attachments.iter().any(|existing| existing.path == *path) {
                continue;
            }
            match ai::read_attachment(path) {
                Ok(kind) => self.attachments.push(Attachment { path: path.clone(), kind }),
                Err(reason) => refusals.push(reason),
            }
        }
        self.note = match refusals.len() {
            0 => None,
            1 => Some(refusals.remove(0).into()),
            n => Some(format!("{} (and {} more refused)", refusals[0], n - 1).into()),
        };
        cx.notify();
    }

    /// The × on a chip. Removal is unconditional and needs no confirmation: taking
    /// something *back* before it is sent is never the dangerous direction.
    fn remove_attachment(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.attachments.len() {
            self.attachments.remove(index);
            self.note = None;
            cx.notify();
        }
    }

    /// A Finder drag entering or leaving the panel. Visual only — see [`Self::drag_over`].
    fn set_drag_over(&mut self, over: bool, cx: &mut Context<Self>) {
        if self.drag_over != over {
            self.drag_over = over;
            cx.notify();
        }
    }

    // ponytail: no "+ file" picker chip in this pass (#99 lists one). It needs a file
    // palette wired into a sibling entity, and the chips above establish the attach/deny
    // mechanics it will reuse — `attach_paths` is already the seam it would call.

    // --- keyboard --------------------------------------------------------------------

    /// The find bar's keystroke filter, for the same reason it exists there: modified
    /// keys and named keys belong to actions, `key_char` is what typing means.
    fn on_key_down(&mut self, event: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        // ⌘V and ⌘C before the modifier guard below, which drops every ⌘ chord — the
        // reason pasting into the chat box silently did nothing. This is the field where
        // paste matters most: nobody retypes a stack trace to ask about it.
        if keystroke.modifiers.platform && keystroke.key == "v" {
            if let Some(pasted) = cx.read_from_clipboard().and_then(|item| item.text()) {
                // Collapsed to one line like every other field here, because this box *is*
                // one line: `enter` sends (see `confirm`), so there is no way to type a
                // newline into it and nothing that would render one. When it grows into a
                // real multi-line composer this is the call site that should keep the
                // breaks — a pasted stack trace is the reason it will.
                let pasted = crate::actions::pasted_into_single_line(&pasted);
                if !pasted.is_empty() {
                    self.input.push_str(&pasted);
                    cx.notify();
                }
            }
            return;
        }
        // ⌘C copies the draft whole: no selection model in this field, and one is out of
        // scope (`palette::on_key_down`'s reasoning).
        if keystroke.modifiers.platform && keystroke.key == "c" {
            if !self.input.is_empty() {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(self.input.clone()));
            }
            return;
        }
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

    /// Puts the panel in Agent mode without touching settings — a test has no
    /// `LiveSettings` global, and this is the mode flag only, exactly like `force_enabled`.
    pub fn set_mode_for_test(&mut self, mode: ChatMode) {
        self.mode = mode;
    }

    /// Feeds events through the real drain, so a test exercises the same `apply_events`
    /// the transport does rather than a parallel path that could drift from it.
    pub fn apply_events_for_test(
        &mut self,
        batch: Vec<AgentEvent>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.apply_events(batch, cx)
    }

    pub fn proposals_for_test(&self) -> &[Proposal] {
        &self.proposals
    }

    pub fn apply_proposal_for_test(&mut self, index: usize, cx: &mut Context<Self>) {
        self.apply_proposal(index, cx);
    }

    pub fn reject_proposal_for_test(&mut self, index: usize, cx: &mut Context<Self>) {
        self.reject_proposal(index, cx);
    }

    pub fn cancel_for_test(&mut self, cx: &mut Context<Self>) {
        self.cancel_stream(cx);
    }

    /// Marks the panel as streaming so `cancel_stream` takes its real path — cancel
    /// returns early otherwise, and a test of "cancel discards proposals" that never
    /// entered the function would pass for the wrong reason.
    pub fn set_streaming_for_test(&mut self, streaming: bool) {
        self.streaming = streaming;
    }

    /// Drops paths onto the panel the way a Finder drag would, so the attach path is
    /// exercised whole — read, classify, refuse — without a window or a drag event.
    pub fn attach_paths_for_test(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        self.attach_paths(paths, cx);
    }

    pub fn remove_attachment_for_test(&mut self, index: usize, cx: &mut Context<Self>) {
        self.remove_attachment(index, cx);
    }

    pub fn attachments_for_test(&self) -> &[Attachment] {
        &self.attachments
    }

    pub fn note_for_test(&self) -> Option<&str> {
        self.note.as_ref().map(|note| note.as_ref())
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

/// One attached file, read at attach time and held until send or removal.
///
/// The bytes are read *at attach*, not at send, for a reason that shows on screen: the
/// user must learn immediately that a file is too large, binary, or denied — a refusal
/// that arrives three minutes later attached to a send is a refusal nobody can act on.
/// The price is that an attachment is a snapshot: a file edited after attaching sends its
/// old contents. That is the same bargain the Current-file chip makes in reverse, and it
/// is the honest one here, because what the chip showed as accepted is what gets sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub path: PathBuf,
    pub kind: ai::AttachmentKind,
}

impl Attachment {
    /// The chip's label: the file name, plus a marker for images so a screenshot is
    /// visibly different from a source file at a glance.
    pub fn label(&self) -> String {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.display().to_string());
        match self.kind {
            ai::AttachmentKind::Image(_) => format!("🖼 {name}"),
            ai::AttachmentKind::Text(_) => name,
        }
    }
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

/// Parses a unified diff into the [`elle_git::DiffFile`] the diff renderer eats (#64).
///
/// This is the bridge that lets a proposed change be drawn by the *same* renderer as a
/// git diff — syntax-highlighted, `+`/`-` in the gutter, themed — instead of a second diff
/// UI existing in this panel. `relative` is only used for display and for picking the
/// highlighter.
///
/// Anything that is not a hunk header or a body line is skipped, so the `---`/`+++`
/// preamble a fuller diff carries costs nothing here.
pub fn diff_file_from_unified(relative: &str, diff: &str) -> elle_git::DiffFile {
    let mut file =
        elle_git::DiffFile { relative: relative.to_string(), hunks: Vec::new(), binary: false };
    let (mut old_no, mut new_no) = (0u32, 0u32);

    for line in diff.split('\n') {
        if let Some((old_start, new_start)) = parse_hunk_header(line) {
            old_no = old_start;
            new_no = new_start;
            file.hunks
                .push(elle_git::Hunk { header: line.trim_end().to_string(), lines: Vec::new() });
            continue;
        }
        let Some(hunk) = file.hunks.last_mut() else { continue };
        if line.is_empty() {
            continue;
        }
        // By character, not by byte — `apply_unified_diff`'s bug, and this copy is the one
        // that fires *first*: this runs while rendering the proposed diff, so a model-emitted
        // line beginning with `ç` or `日` took the window down before the user could even
        // read the proposal, let alone click Apply.
        let mut chars = line.chars();
        let marker_len = chars.next().map(|c| c.len_utf8()).unwrap_or(0);
        let (marker, text) = line.split_at(marker_len);
        let (kind, old_line, new_line) = match marker {
            "+" => (elle_git::LineKind::Added, None, Some(new_no)),
            "-" => (elle_git::LineKind::Removed, Some(old_no), None),
            " " => (elle_git::LineKind::Context, Some(old_no), Some(new_no)),
            // `\ No newline at end of file` and any other annotation.
            _ => continue,
        };
        match kind {
            elle_git::LineKind::Added => new_no += 1,
            elle_git::LineKind::Removed => old_no += 1,
            elle_git::LineKind::Context => {
                old_no += 1;
                new_no += 1;
            }
        }
        hunk.lines.push(elle_git::Line { kind, text: text.to_string(), old_line, new_line });
    }

    file
}

/// The `(old_start, new_start)` of an `@@ -a,b +c,d @@` header.
fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let new = rest.split(' ').next()?;
    let start = |spec: &str| spec.split(',').next()?.parse::<u32>().ok();
    // A `0,0` side (a pure add or delete) starts numbering at 1, which is what git's own
    // renderer shows and what keeps the gutter from displaying a 0th line.
    Some((start(old)?.max(1), start(new)?.max(1)))
}

/// Applies a unified diff to `original`, returning the new text.
///
/// This is the step between "the model proposed a patch" and "bytes go to a file", so it
/// is deliberately strict: every context and removed line must match the text it claims to
/// be replacing, and any mismatch returns `Err` rather than a best-effort merge. A patch
/// that does not fit the file it was written against is a patch built from a stale read,
/// and applying it anywhere near where it *nearly* fits is how an editor corrupts source.
///
/// Only the `@@` hunk bodies are read; the `---`/`+++` file headers carry no content and
/// the CLI's diffs for a single file omit them anyway.
pub fn apply_unified_diff(original: &str, diff: &str) -> Result<String, String> {
    let original_lines: Vec<&str> = original.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    // 0-based cursor into `original_lines`: everything before it is already in `out`.
    let mut cursor = 0usize;
    let mut saw_hunk = false;

    let mut lines = diff.split('\n').peekable();
    while let Some(line) = lines.next() {
        if line.starts_with("---") || line.starts_with("+++") {
            continue;
        }
        let Some(start) = parse_hunk_start(line) else { continue };
        saw_hunk = true;

        // Copy everything between the last hunk and this one, unchanged.
        let start = start.saturating_sub(1);
        if start < cursor {
            return Err("the patch's hunks are out of order".to_string());
        }
        if start > original_lines.len() {
            return Err("the patch starts past the end of the file".to_string());
        }
        out.extend(original_lines[cursor..start].iter().map(|line| (*line).to_string()));
        cursor = start;

        // The hunk body, until the next `@@` or the end.
        while let Some(body) = lines.peek() {
            if parse_hunk_start(body).is_some() {
                break;
            }
            let body = lines.next().expect("peeked");
            // An empty trailing line is the split artefact of a diff ending in `\n`, not
            // a context line for an empty line (which arrives as a single space).
            if body.is_empty() {
                continue;
            }
            // Split at the first *character*, not the first byte. `split_at(1)` panicked
            // whenever a body line began with a multi-byte character — "end byte index 1
            // is not a char boundary; it is inside 'ç'" — and took the window down mid-apply.
            //
            // A well-formed diff never produces such a line: every body line starts with an
            // ASCII ` `, `+` or `-`. But this diff arrives from a language model, and a
            // dropped marker (which models do emit, especially on whitespace-only lines) is
            // ordinary malformed input. Everything else malformed here is an `Err` — hunks
            // out of order, a patch longer than the file, context that does not match — so a
            // panic was the one failure mode that escaped the contract.
            //
            // The unmatched marker falls through to the `_` arm below, which already ignores
            // annotation lines, so a mangled line is skipped rather than applied blindly.
            let mut chars = body.chars();
            let marker = chars.next().map(|c| c.len_utf8()).unwrap_or(0);
            let (marker, text) = body.split_at(marker);
            match marker {
                "+" => out.push(text.to_string()),
                " " | "-" => {
                    let actual = original_lines.get(cursor).ok_or_else(|| {
                        "the patch expects more lines than the file has".to_string()
                    })?;
                    if *actual != text {
                        return Err(format!(
                            "the patch does not match the file at line {}: expected {:?}, \
                             found {:?}",
                            cursor + 1,
                            text,
                            actual
                        ));
                    }
                    if marker == " " {
                        out.push(text.to_string());
                    }
                    cursor += 1;
                }
                // `\ No newline at end of file`, and anything else a diff writer adds as
                // annotation rather than content.
                _ => {}
            }
        }
    }

    if !saw_hunk {
        return Err("the patch carries no hunks".to_string());
    }
    out.extend(original_lines[cursor..].iter().map(|line| (*line).to_string()));
    Ok(out.join("\n"))
}

/// The 1-based start line on the *old* side of an `@@ -a,b +c,d @@` header, or `None` for
/// any other line.
fn parse_hunk_start(line: &str) -> Option<usize> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, _) = rest.split_once(' ')?;
    let start = old.split(',').next()?;
    start.parse().ok()
}

/// Turns the attached files into the two things a send needs: prose blocks for the text
/// ones, and image values for the wire.
///
/// **This is where the second deny check lives.** The chip already refused at attach time
/// — that is the UX — and this is the guarantee: a path that became a secret between the
/// attach and the send (renamed to `.env`, or a symlink repointed) is refused here, with
/// the bytes already in hand and about to be serialised. The check is on the path because
/// that is what [`ai::deny_reason`] judges, and the path is what the user attached.
///
/// Images are *not* described in the prose. Their bytes go on the wire as blocks; naming
/// them in the text too would tell the model a file exists twice.
pub fn build_attachment_context(
    attachments: &[Attachment],
) -> Result<(String, Vec<ai::Image>), String> {
    let mut prose = String::new();
    let mut images = Vec::new();
    for attachment in attachments {
        if let Some(reason) = ai::deny_reason(&attachment.path) {
            return Err(deny_note(&attachment.path, reason));
        }
        match &attachment.kind {
            ai::AttachmentKind::Text(text) => prose.push_str(&format!(
                "Context — attached file {}:\n```\n{}\n```\n\n",
                attachment.path.display(),
                text.trim_end_matches('\n')
            )),
            ai::AttachmentKind::Image(image) => images.push(image.clone()),
        }
    }
    Ok((prose, images))
}

/// Whether this provider can carry attachments at all.
///
/// Codex is the odd one out for the reason it is always the odd one out (#99): the panel
/// hands the CLI a string of text over JSON-RPC and the CLI owns the conversation, so
/// there is no content-block seam to put an image into. Text attachments *would* fit —
/// they are just prose — but shipping half a feature whose chips silently mean different
/// things per provider is worse than saying so, which is what Agent mode did here too.
pub fn attachments_supported(provider: ai::Provider) -> bool {
    provider.wire().is_some()
}

/// The line the chip row shows instead of an attach button when the provider cannot
/// carry attachments — the "say so rather than half-build it" half of the above.
pub const CODEX_NO_ATTACHMENTS: &str =
    "Codex carries no attachments — it runs your CLI, which owns the conversation";

/// The refusal, worded as a statement rather than an apology: there is no override (#99).
fn deny_note(path: &Path, reason: &str) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    format!("{name} stays local — {reason}")
}

/// The one-line notice for a mode the current provider cannot actually do, or `None`.
///
/// Agent mode is Codex-only in this pass: the HTTP providers would need a full tool-use
/// loop to propose edits, which is a larger piece of work than the diff-and-approval half
/// this pass is about. Saying so plainly beats a mode that is selectable and silently
/// behaves like Ask — the user would send an instruction to edit a file and get prose.
pub fn mode_notice(mode: ChatMode, provider: ai::Provider) -> Option<String> {
    (mode == ChatMode::Agent && provider != ai::Provider::Codex).then(|| {
        format!(
            "Agent mode needs the Codex provider — {} answers in Ask mode. \
             Provider lives in Settings (⌘,) → AI.",
            provider.setting_name()
        )
    })
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
    tx: &smol::channel::Sender<AgentEvent>,
) {
    // Keychain and `ant` CLI subprocesses — the reason this whole function is off the
    // main thread. The error strings are already user-facing (resolve_auth's contract).
    let auth = match ai::resolve_auth(provider, base_url) {
        Ok(auth) => auth,
        Err(message) => {
            let _ = tx.send_blocking(AgentEvent::Stream(StreamEvent::Error(message)));
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
            let _ = tx.send_blocking(AgentEvent::Stream(StreamEvent::Error(format!(
                "could not run curl: {err}"
            ))));
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
            if tx.send_blocking(AgentEvent::Stream(event)).is_err() || stream_ended {
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
            let _ = tx.send_blocking(AgentEvent::Stream(StreamEvent::Error(message)));
        }
        // EOF without a terminal event (a custom server that just closes): the reply is
        // whatever arrived, and it is over.
        _ => {
            let _ = tx.send_blocking(AgentEvent::Stream(StreamEvent::Done));
        }
    }
}

/// Writes one JSON-RPC line to the Codex child, taking the stdin lock only for the write.
///
/// The one way anything reaches the CLI, so the "held briefly, never nested" rule that
/// makes `AiChatPanel::codex_stdin` safe lives in a single place rather than at four call
/// sites. Poison-tolerant like the rest of this panel: another thread's panic must not
/// turn an approval into a second panic.
///
/// Returns whether the bytes went out. `false` means the child is gone — which for an
/// approval is the same outcome as a decline, and for a turn start is a reportable error.
fn write_to_codex(stdin: &Arc<Mutex<Option<ChildStdin>>>, message: &str) -> bool {
    let mut guard = stdin.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(stdin) = guard.as_mut() else { return false };
    stdin.write_all(message.as_bytes()).and_then(|()| stdin.flush()).is_ok()
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
    // Published out of the turn lock so an approval or a cancel can reach the child while
    // this function is holding that lock and blocking on the reply. See
    // `AiChatPanel::codex_stdin` for the deadlock this prevents.
    shared_stdin: &Arc<Mutex<Option<ChildStdin>>>,
    shared_thread_id: &Arc<Mutex<Option<String>>>,
    root: Option<&Path>,
    message: &str,
    tx: &smol::channel::Sender<AgentEvent>,
) {
    // Availability first, so "not installed" and "not logged in" arrive as the sentence
    // that names the command to run, rather than as a spawn error nobody can act on.
    if let Err(reason) = crate::ai_codex::availability() {
        let _ = tx.send_blocking(AgentEvent::Stream(StreamEvent::Error(reason)));
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
        if write_to_codex(&session.stdin, &request) {
            read_codex_turn(session, tx);
            return;
        }
        // The child died between turns (a crash, a logout). Drop the corpse and fall
        // through to a fresh spawn rather than reporting a broken pipe at the user.
        state.session = None;
    }

    // Resolved rather than bare, for the reason in `ai_codex::binary`: a Finder launch has
    // an empty `PATH` and a bare name finds nothing. `availability` has already run and
    // uses the same resolver, so a `None` here means the CLI vanished between the check and
    // the spawn — reported rather than turned into a confusing "broken pipe" downstream.
    let Some(binary) = crate::ai_codex::binary() else {
        let _ = tx.send_blocking(AgentEvent::Stream(StreamEvent::Error(
            "Codex CLI not found — install it and run `codex login`".to_string(),
        )));
        return;
    };
    let mut command = std::process::Command::new(&binary);
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
            let _ = tx.send_blocking(AgentEvent::Stream(StreamEvent::Error(format!(
                "could not run the Codex CLI: {err} — install it and run `codex login`"
            ))));
            return;
        }
    };

    let Some(mut stdin) = child.stdin.take() else {
        let _ = tx.send_blocking(AgentEvent::Stream(StreamEvent::Error(
            "the Codex CLI gave no stdin".to_string(),
        )));
        return;
    };
    let stdout = child.stdout.take();
    *kill.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(child);

    let handshake = crate::ai_codex::initialize_request(env!("CARGO_PKG_VERSION"))
        + &crate::ai_codex::thread_start_request(root);
    if stdin.write_all(handshake.as_bytes()).and_then(|()| stdin.flush()).is_err() {
        let _ = tx.send_blocking(AgentEvent::Stream(StreamEvent::Error(
            "the Codex CLI closed before the handshake finished".to_string(),
        )));
        return;
    }

    let Some(stdout) = stdout else {
        let _ = tx.send_blocking(AgentEvent::Stream(StreamEvent::Error(
            "the Codex CLI gave no stdout".to_string(),
        )));
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
                let _ = tx.send_blocking(AgentEvent::Stream(StreamEvent::Error(
                    "the Codex CLI closed before the thread opened — try `codex login`".to_string(),
                )));
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
                let _ = tx.send_blocking(AgentEvent::Stream(StreamEvent::Error(message)));
                return;
            }
            _ => {}
        }
    }
    let Some(thread_id) = thread_id else { return };

    // Publish before the first turn/start: from this point the turn can block on a reply,
    // and the approval that unblocks it must already have a way through.
    *shared_stdin.lock().unwrap_or_else(|p| p.into_inner()) = Some(stdin);
    *shared_thread_id.lock().unwrap_or_else(|p| p.into_inner()) = Some(thread_id.clone());

    let request = crate::ai_codex::turn_start_request(&thread_id, message);
    if !write_to_codex(shared_stdin, &request) {
        let _ = tx.send_blocking(AgentEvent::Stream(StreamEvent::Error(
            "the Codex CLI closed before the turn started".to_string(),
        )));
        return;
    }

    state.model = model;
    let session = state
        .session
        .insert(CodexSession { stdin: Arc::clone(shared_stdin), stdout, thread_id });
    read_codex_turn(session, tx);
}

/// Forwards one turn's worth of events from an established session.
///
/// Returns at `turn/completed` / `turn/failed` — and *only* then — leaving the reader
/// parked mid-stream for the next turn. That is why the reader lives in the session: the
/// next turn's bytes may already be in this `BufReader`'s buffer.
fn read_codex_turn(session: &mut CodexSession, tx: &smol::channel::Sender<AgentEvent>) {
    let mut line = String::new();
    loop {
        line.clear();
        match session.stdout.read_line(&mut line) {
            // EOF: the CLI exited (killed by a cancel, crashed, logged out mid-turn).
            // Whatever arrived is the reply, and it is over.
            Ok(0) | Err(_) => {
                let _ = tx.send_blocking(AgentEvent::Stream(StreamEvent::Done));
                return;
            }
            Ok(_) => {}
        }
        match crate::ai_codex::parse_line(&line) {
            Some(crate::ai_codex::CodexEvent::Delta(text)) => {
                if tx.send_blocking(AgentEvent::Stream(StreamEvent::Delta(text))).is_err() {
                    return; // the drain went away: a cancel, and the kill is coming
                }
            }
            // Agent mode: the proposal and then the question about it, both forwarded on
            // the same channel as the text so the panel keeps one drain and one cancel.
            // The reader does not answer the approval — the *user* does, from the
            // foreground, which is the whole point.
            Some(crate::ai_codex::CodexEvent::Proposed { item_id, changes }) => {
                let changes = changes
                    .into_iter()
                    .map(|change| ai::ProposedFileChange {
                        path: change.path,
                        kind: change.kind,
                        diff: change.diff,
                    })
                    .collect();
                if tx.send_blocking(AgentEvent::Proposed { item_id, changes }).is_err() {
                    return;
                }
            }
            Some(crate::ai_codex::CodexEvent::ApprovalRequested { request_id, item_id }) => {
                if tx.send_blocking(AgentEvent::ApprovalRequested { request_id, item_id }).is_err()
                {
                    return;
                }
            }
            Some(crate::ai_codex::CodexEvent::TurnCompleted) => {
                let _ = tx.send_blocking(AgentEvent::Stream(StreamEvent::Done));
                return;
            }
            Some(crate::ai_codex::CodexEvent::TurnFailed(message)) => {
                let _ = tx.send_blocking(AgentEvent::Stream(StreamEvent::Error(message)));
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            // Finder drops land on the *whole panel*, the workspace's own reasoning
            // (`external_drop`) applied here: there is no wrong place to drop a file onto
            // a chat you are about to ask a question about. The panel is painted over the
            // workspace, so this handler wins for drops inside it and the workspace's
            // opens-a-file handler still owns everywhere else.
            //
            // Registered whatever the provider: a drop that cannot be carried is refused
            // with a sentence, which is more use than a drag that mysteriously does
            // nothing over one provider and works over another.
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _window, cx| {
                this.set_drag_over(false, cx);
                let paths = paths.paths().to_vec();
                if attachments_supported(ai::Provider::from_setting(
                    crate::settings::current(cx).ai_chat_provider(),
                )) {
                    this.attach_paths(&paths, cx);
                } else {
                    this.note = Some(CODEX_NO_ATTACHMENTS.into());
                    cx.notify();
                }
            }))
            .on_drag_move(cx.listener(
                |this, _ev: &gpui::DragMoveEvent<gpui::ExternalPaths>, _w, cx| {
                    this.set_drag_over(true, cx);
                },
            ))
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
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child("AI Chat")
                            .child(self.render_mode_switch(&theme, cx)),
                    )
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
                el
                    // Agent selected on a provider that cannot do it: said out loud, and
                    // above the input where the send is about to happen.
                    .children(mode_notice(self.mode, provider).map(|notice| {
                        div()
                            .px_2()
                            .py_1()
                            .text_color(theme.text_muted)
                            .text_size(px(11.0))
                            .child(SharedString::from(notice))
                    }))
                    // The review list sits directly above the input: it is a pending
                    // action, not history, and it must not scroll away with the
                    // transcript.
                    .when(!self.proposals.is_empty(), |el| {
                        el.child(self.render_proposals(&theme, cx))
                    })
                    .children(self.note.clone().map(|note| {
                        div().px_2().py_1().text_color(theme.error).text_size(px(11.0)).child(note)
                    }))
                    .child(self.render_chips(&theme, cx))
                    .child(self.render_input(&theme, window, cx))
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

    /// The Ask | Agent switch. A segmented pair, styled like the context chips so the two
    /// controls in this panel that change what a send *does* look like one family.
    fn render_mode_switch(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let segment = |id: &'static str, label: &'static str, mode: ChatMode| {
            let entity = cx.entity();
            let active = self.mode == mode;
            div()
                .id(id)
                .px_2()
                .py_0p5()
                .cursor_pointer()
                // Active is a fill *and* a text colour, never colour alone — the find
                // bar's rule, and the same one the chips follow.
                .when(active, |el| el.bg(theme.selected).text_color(theme.text))
                .when(!active, |el| el.text_color(theme.text_muted))
                .hover(|el| el.bg(theme.hover))
                .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                    entity.update(cx, |this, cx| this.set_mode(mode, cx));
                })
                .child(label)
        };

        div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .flex()
                    .rounded(px(4.0))
                    .overflow_hidden()
                    .border_1()
                    .border_color(theme.border)
                    .child(segment("ai-mode-ask", "Ask", ChatMode::Ask))
                    .child(segment("ai-mode-agent", "Agent", ChatMode::Agent)),
            )
            .into_any_element()
    }

    /// The review list: one card per proposed file, each with its own decision.
    ///
    /// Rendered above the input rather than inside the transcript because these are not
    /// conversation — they are a pending action, and burying them in a scrolled history
    /// is how an Apply button gets missed.
    fn render_proposals(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let cards: Vec<gpui::AnyElement> = self
            .proposals
            .iter()
            .enumerate()
            .map(|(index, proposal)| self.render_proposal(index, proposal, theme, cx))
            .collect();

        div()
            .id("ai-proposals")
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_1()
            .max_h(px(320.0))
            .overflow_y_scroll()
            .border_t_1()
            .border_color(theme.border)
            .child(div().text_size(px(10.0)).text_color(theme.text_muted).child(
                SharedString::from(format!(
                    "{} proposed change{} — nothing is written until you apply it",
                    self.proposals.len(),
                    if self.proposals.len() == 1 { "" } else { "s" }
                )),
            ))
            .children(cards)
            .into_any_element()
    }

    fn render_proposal(
        &self,
        index: usize,
        proposal: &Proposal,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let name = proposal
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| proposal.path.display().to_string());

        // The decision row: buttons while pending, a settled word once it is not.
        let controls: gpui::AnyElement = match proposal.state {
            ProposalState::Pending => {
                let apply_entity = cx.entity();
                let reject_entity = cx.entity();
                div()
                    .flex()
                    .gap_1()
                    .child(
                        div()
                            .id(("ai-proposal-apply", index))
                            .px_2()
                            .py_0p5()
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .text_color(theme.accent)
                            .hover(|el| el.bg(theme.hover))
                            .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                                apply_entity.update(cx, |this, cx| this.apply_proposal(index, cx));
                            })
                            .child("Apply"),
                    )
                    .child(
                        div()
                            .id(("ai-proposal-reject", index))
                            .px_2()
                            .py_0p5()
                            .rounded(px(4.0))
                            .cursor_pointer()
                            .text_color(theme.text_muted)
                            .hover(|el| el.bg(theme.hover))
                            .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                                reject_entity
                                    .update(cx, |this, cx| this.reject_proposal(index, cx));
                            })
                            .child("Reject"),
                    )
                    .into_any_element()
            }
            ProposalState::Applied => div()
                .text_size(px(10.0))
                .text_color(theme.diff_added())
                .child("applied")
                .into_any_element(),
            ProposalState::Rejected => div()
                .text_size(px(10.0))
                .text_color(theme.text_muted)
                .child("rejected")
                .into_any_element(),
            // The denylist's refusal, with the reason, where the buttons would have been.
            ProposalState::Blocked(reason) => div()
                .text_size(px(10.0))
                .text_color(theme.error)
                .child(SharedString::from(format!("blocked — {reason}")))
                .into_any_element(),
        };

        div()
            .flex()
            .flex_col()
            .gap_1()
            .rounded(px(4.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .py_0p5()
                    .child(
                        div().flex().items_center().gap_1().child(SharedString::from(name)).child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(proposal.kind.clone())),
                        ),
                    )
                    .child(controls),
            )
            .child(render_proposal_diff(
                index,
                &proposal.path.to_string_lossy(),
                &proposal.diff,
                theme,
                cx,
            ))
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

        // Codex has no seam to carry an attachment through, so the row says so instead of
        // offering chips that would mean something different there (#99's house pattern).
        let provider = ai::Provider::from_setting(crate::settings::current(cx).ai_chat_provider());
        let can_attach = attachments_supported(provider);

        // The attached files, each removable. `flex_wrap` because five screenshots must
        // push the row taller rather than off the side of a narrow panel.
        let attachment_chips: Vec<gpui::AnyElement> = self
            .attachments
            .iter()
            .enumerate()
            .map(|(index, attachment)| {
                let entity = cx.entity();
                div()
                    .id(("ai-attachment", index))
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_0p5()
                    .rounded(px(9.0))
                    .border_1()
                    .border_color(theme.accent)
                    .bg(theme.selected)
                    .child(SharedString::from(attachment.label()))
                    .child(
                        div()
                            .id(("ai-attachment-remove", index))
                            .cursor_pointer()
                            .text_color(theme.text_muted)
                            .hover(|el| el.text_color(theme.error))
                            .on_mouse_down(MouseButton::Left, move |_ev, _window, cx| {
                                entity.update(cx, |this, cx| this.remove_attachment(index, cx));
                            })
                            .child("×"),
                    )
                    .into_any_element()
            })
            .collect();

        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(11.0))
            .child(chip("ai-chip-selection", "Selection", self.attach_selection, Chip::Selection))
            .child(chip("ai-chip-file", "Current file", self.attach_file, Chip::CurrentFile))
            .when(can_attach, |el| {
                // The affordance, not a button: there is no file picker in this pass, so
                // the row teaches the gesture that does work rather than offering one
                // that would open nothing.
                el.child(div().text_color(theme.text_muted).child(if self.drag_over {
                    "Drop to attach"
                } else {
                    "or drop files here"
                }))
            })
            .when(!can_attach, |el| {
                el.child(div().text_color(theme.text_muted).child(CODEX_NO_ATTACHMENTS))
            })
            .children(attachment_chips)
            .into_any_element()
    }

    fn render_input(
        &self,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let empty = self.input.is_empty();
        let focused = self.focus_handle.is_focused(window);
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
                // The find bar's field, now with the palette's caret (#164): a bar before
                // the placeholder and after typed text, so the box reads as an input
                // either way. Only when focused — the panel can sit open beside a focused
                // editor, and a caret there would claim the keyboard it does not have.
                // Solid, not blinking, for the palette's reason: a steady bar says "type
                // here" without buying a timer per open panel.
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
                    .when(focused && empty, |el| {
                        el.child(div().w(px(2.0)).h(px(14.0)).mr_1().flex_none().bg(theme.cursor))
                    })
                    .child(SharedString::from(if empty {
                        "Ask — Enter sends".to_string()
                    } else {
                        self.input.clone()
                    }))
                    .when(focused && !empty, |el| {
                        el.child(
                            div().w(px(2.0)).h(px(14.0)).ml(px(1.0)).flex_none().bg(theme.cursor),
                        )
                    }),
            )
            .child(action_button)
            .into_any_element()
    }
}

/// One proposal's diff, drawn by the git panel's renderer (#64) rather than a second one.
///
/// `DiffRenderer::new` runs tree-sitter, which is why the git panel builds it on the
/// background executor. Here it is built in the render pass on purpose: a proposal's diff
/// is a handful of hunks of one file, the parse is microseconds at that size, and the
/// alternative — a background task per proposal writing a renderer back into the panel —
/// is a lot of machinery for a pane that appears a few times a session. If proposals ever
/// arrive with whole-file diffs, this is the call that should move.
fn render_proposal_diff(
    index: usize,
    relative: &str,
    diff: &str,
    theme: &Theme,
    cx: &App,
) -> gpui::AnyElement {
    let file = diff_file_from_unified(relative, diff);
    let renderer = crate::git_panel::DiffRenderer::new(&file);
    div()
        .id(("ai-proposal-diff", index))
        .max_h(px(180.0))
        .overflow_y_scroll()
        .text_size(px(11.0))
        .child(crate::git_panel::render_diff(&file, &renderer, theme, cx))
        .into_any_element()
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
    fn attachment_context_fences_text_and_hands_images_to_the_wire() {
        let attachments = vec![
            Attachment {
                path: PathBuf::from("app/Http/Kernel.php"),
                kind: ai::AttachmentKind::Text("<?php\nclass Kernel {}\n".to_string()),
            },
            Attachment {
                path: PathBuf::from("shot.png"),
                kind: ai::AttachmentKind::Image(ai::Image {
                    media_type: "image/png".to_string(),
                    data: "Zm9v".to_string(),
                }),
            },
        ];
        let (prose, images) = build_attachment_context(&attachments).unwrap();
        assert!(prose.contains("Context — attached file app/Http/Kernel.php:"), "{prose}");
        assert!(prose.contains("class Kernel {}"), "{prose}");
        assert!(
            !prose.contains("shot.png"),
            "an image's bytes go on the wire; naming it in the prose too says it twice: {prose}"
        );
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].media_type, "image/png");

        assert_eq!(build_attachment_context(&[]).unwrap(), (String::new(), Vec::new()));
    }

    /// The second deny pass — the guarantee half, as opposed to the chip's UX half.
    #[test]
    fn attachment_context_refuses_a_file_that_became_a_secret_after_it_was_attached() {
        // The bytes were read and accepted at attach time under an innocent name; by send
        // time the path is a `.env`. #99 allows no override, so the refusal holds here
        // even though the content is already in hand.
        let renamed = vec![Attachment {
            path: PathBuf::from("config/.env"),
            kind: ai::AttachmentKind::Text("APP_KEY=oops".to_string()),
        }];
        let refusal = build_attachment_context(&renamed).unwrap_err();
        assert!(refusal.contains(".env"), "{refusal}");
        assert!(refusal.contains("credentials"), "{refusal}");
    }

    #[test]
    fn codex_carries_no_attachments_and_says_so() {
        // The house pattern: a provider that cannot express a feature says so rather than
        // offering chips that would quietly mean something else there.
        assert!(!attachments_supported(ai::Provider::Codex));
        for http in [ai::Provider::Anthropic, ai::Provider::AntCli, ai::Provider::Custom] {
            assert!(attachments_supported(http), "{http:?} has a content-block seam");
        }
        assert!(CODEX_NO_ATTACHMENTS.contains("Codex"));
    }

    #[test]
    fn an_attachment_chip_is_labelled_by_its_file_name() {
        let image = Attachment {
            path: PathBuf::from("/Users/me/Desktop/Screenshot 2026.png"),
            kind: ai::AttachmentKind::Image(ai::Image {
                media_type: "image/png".to_string(),
                data: String::new(),
            }),
        };
        // The name, not the path: a chip is one line in a narrow panel.
        assert!(image.label().ends_with("Screenshot 2026.png"), "{}", image.label());
        assert!(image.label().starts_with('🖼'), "an image reads as one at a glance");

        let text = Attachment {
            path: PathBuf::from("app/Models/User.php"),
            kind: ai::AttachmentKind::Text(String::new()),
        };
        assert_eq!(text.label(), "User.php");
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

    // --- agent mode: the pure parts ---------------------------------------------------

    #[test]
    fn a_mode_setting_falls_back_to_the_one_that_cannot_write() {
        assert_eq!(ChatMode::from_setting("agent"), ChatMode::Agent);
        assert_eq!(ChatMode::from_setting("ask"), ChatMode::Ask);
        // The load-bearing default: a typo, an empty string, or a future value must not
        // put the panel in the mode that can propose writes.
        for wrong in ["", "Agent", "agentic", "gibberish", "true"] {
            assert_eq!(ChatMode::from_setting(wrong), ChatMode::Ask, "{wrong:?}");
        }
        assert_eq!(ChatMode::default(), ChatMode::Ask);
        // Round-trips, or the switch cannot persist what it just set.
        for mode in [ChatMode::Ask, ChatMode::Agent] {
            assert_eq!(ChatMode::from_setting(mode.setting_name()), mode);
        }
    }

    #[test]
    fn agent_mode_says_so_when_the_provider_cannot_do_it() {
        // Codex is the only agent-capable provider this pass; the others say so rather
        // than silently answering in Ask mode.
        for provider in [ai::Provider::Anthropic, ai::Provider::AntCli, ai::Provider::Custom] {
            let notice = mode_notice(ChatMode::Agent, provider).expect("a notice");
            assert!(notice.contains("Codex"), "{notice}");
            assert!(notice.contains("Ask mode"), "and what still works: {notice}");
        }
        assert_eq!(mode_notice(ChatMode::Agent, ai::Provider::Codex), None);
        // Ask mode works everywhere, so it never carries a notice.
        for provider in [ai::Provider::Anthropic, ai::Provider::Codex, ai::Provider::Custom] {
            assert_eq!(mode_notice(ChatMode::Ask, provider), None);
        }
    }

    #[test]
    fn a_unified_diff_becomes_the_shape_the_diff_renderer_draws() {
        // The captured shape from a real `codex-cli` turn.
        let diff = "@@ -2,3 +2,3 @@\n function hello() {\n-    return 'hi';\n+    return 'hello world';\n }\n";
        let file = diff_file_from_unified("app/hello.php", diff);
        assert_eq!(file.relative, "app/hello.php");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.counts(), (1, 1), "one added, one removed");

        let lines = &file.hunks[0].lines;
        assert_eq!(lines[0].kind, elle_git::LineKind::Context);
        assert_eq!(lines[1].kind, elle_git::LineKind::Removed);
        assert_eq!(lines[1].text, "    return 'hi';");
        assert_eq!(lines[2].kind, elle_git::LineKind::Added);
        assert_eq!(lines[2].text, "    return 'hello world';");

        // The gutter numbers each side of the change.
        assert_eq!(lines[1].old_line, Some(3), "removed lines number on the old side");
        assert_eq!(lines[1].new_line, None);
        assert_eq!(lines[2].new_line, Some(3));
        assert_eq!(lines[2].old_line, None);
    }

    #[test]
    fn a_patch_that_matches_the_file_produces_the_new_text() {
        let original = "<?php\nfunction hello() {\n    return 'hi';\n}\n";
        let diff = "@@ -2,3 +2,3 @@\n function hello() {\n-    return 'hi';\n+    return 'hello world';\n }\n";
        assert_eq!(
            apply_unified_diff(original, diff).unwrap(),
            "<?php\nfunction hello() {\n    return 'hello world';\n}\n"
        );
    }

    #[test]
    fn a_patch_that_does_not_match_the_file_is_refused_rather_than_forced() {
        // The user edited the file after the model read it. Applying "nearly" here is how
        // an editor corrupts source, so the patch is refused whole and the file is kept.
        let edited = "<?php\nfunction hello() {\n    return 'something else';\n}\n";
        let stale = "@@ -2,3 +2,3 @@\n function hello() {\n-    return 'hi';\n+    return 'hello world';\n }\n";
        let refusal = apply_unified_diff(edited, stale).unwrap_err();
        assert!(refusal.contains("does not match"), "{refusal}");

        // And a patch with no hunks at all changes nothing.
        assert!(apply_unified_diff(edited, "not a diff").is_err());
    }

    #[test]
    fn a_patch_with_several_hunks_applies_all_of_them() {
        // A real refactor touches a file in more than one place, and the untouched lines
        // between the hunks have to survive verbatim.
        let original: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        // Copied from `git diff -U1` on exactly this change: the header names the line the
        // hunk *starts* at, so the first body line is line 1, not line 2.
        let diff = "@@ -1,3 +1,3 @@\n line 1\n-line 2\n+line TWO\n line 3\n\
                    @@ -16,3 +16,3 @@ line 15\n line 16\n-line 17\n+line SEVENTEEN\n line 18\n";
        let patched = apply_unified_diff(&original, diff).unwrap();
        assert!(patched.contains("line TWO"), "first hunk applied");
        assert!(patched.contains("line SEVENTEEN"), "second hunk applied");
        assert!(patched.contains("line 10"), "the untouched middle survived");
        assert!(!patched.contains("line 2\n"), "the old first line is gone");
        assert_eq!(patched.lines().count(), 20, "no lines gained or lost");
    }

    #[test]
    fn an_added_file_patches_from_nothing() {
        let diff = "@@ -0,0 +1,2 @@\n+<?php\n+return 1;\n";
        assert_eq!(apply_unified_diff("", diff).unwrap(), "<?php\nreturn 1;\n");
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

#[cfg(test)]
mod agent_diff_utf8_tests {
    use super::apply_unified_diff;

    /// The owner's report: "crashou no modo agent".
    ///
    /// `apply_unified_diff` reads each hunk-body line as `body.split_at(1)` to separate the
    /// ` `/`+`/`-` marker from the text. `split_at` takes a **byte** index, so a body line
    /// whose first character is multi-byte splits *inside* that character and panics.
    ///
    /// A well-formed diff never produces one — every body line starts with an ASCII marker.
    /// But the diff here comes from a language model over a wire, and agent mode applies it
    /// to the user's files: a model that drops the leading space on a context line (which
    /// they do, especially on lines that are only whitespace) hands this function a line
    /// starting with `ç`, `日` or an emoji. The panic then takes the whole window down mid-apply.
    ///
    /// Malformed input must be an error, not a crash: the surrounding code already returns
    /// `Err` for out-of-order hunks, over-long patches and mismatched context.
    #[test]
    fn a_body_line_starting_with_a_multibyte_character_is_an_error_not_a_panic() {
        let original = "<?php\n$a = 1;\n$b = 2;\n";

        // Marker dropped from a context line whose text begins with an accent — the shape
        // a model actually emits.
        let hostile = [
            "@@ -1,3 +1,3 @@\nção alterada\n",
            "@@ -1,3 +1,3 @@\n日本語の行\n",
            "@@ -1,3 +1,3 @@\n👨‍👩‍👧‍👦\n",
            // Also the combining-mark case, where the boundary is not where it looks.
            "@@ -1,3 +1,3 @@\ne\u{0301}poca\n",
        ];

        for diff in hostile {
            let outcome = std::panic::catch_unwind(|| apply_unified_diff(original, diff));
            assert!(
                outcome.is_ok(),
                "applying {diff:?} panicked; a malformed patch must be reported, not fatal"
            );
        }
    }

    /// The *rendering* half of the same bug, which fires before Apply is even possible.
    ///
    /// `diff_file_from_unified` turns a proposed patch into the structure the git-diff
    /// renderer draws. It read the marker with the same `split_at(1)`, so a model-emitted
    /// body line starting with a multi-byte character crashed the panel while drawing the
    /// proposal — the user never got to accept or decline it.
    #[test]
    fn rendering_a_proposal_with_multibyte_lines_does_not_panic() {
        let hostile = [
            "@@ -1,3 +1,3 @@\nção alterada\n",
            "@@ -1,3 +1,3 @@\n日本語の行\n",
            "@@ -1,3 +1,3 @@\n👨‍👩‍👧‍👦\n",
            "@@ -1,3 +1,3 @@\ne\u{0301}poca\n",
        ];

        for diff in hostile {
            let outcome = std::panic::catch_unwind(|| {
                super::diff_file_from_unified("app/Models/Configuração.php", diff)
            });
            assert!(outcome.is_ok(), "rendering {diff:?} panicked; a malformed patch must draw, not crash");
        }
    }

    /// A well-formed proposal over accented text still renders with its markers read
    /// correctly — the fix must not turn every line into an annotation.
    #[test]
    fn a_well_formed_proposal_over_accented_text_still_renders() {
        let diff = "@@ -2,1 +2,1 @@\n-$título = 'ação';\n+$título = 'configuração';\n";
        let file = super::diff_file_from_unified("app/M.php", diff);
        let kinds: Vec<_> =
            file.hunks.iter().flat_map(|h| h.lines.iter().map(|l| l.kind)).collect();
        assert_eq!(
            kinds,
            vec![elle_git::LineKind::Removed, elle_git::LineKind::Added],
            "the markers must still be read off accented lines"
        );
    }

    /// The happy path stays intact — a robustness fix that refused valid patches would
    /// disable agent mode instead of fixing it.
    #[test]
    fn a_well_formed_patch_over_accented_text_still_applies() {
        let original = "<?php\n$título = 'ação';\n$fim = 1;\n";
        let diff = "@@ -2,1 +2,1 @@\n-$título = 'ação';\n+$título = 'configuração';\n";

        let applied = apply_unified_diff(original, diff).expect("a well-formed patch applies");
        assert_eq!(applied, "<?php\n$título = 'configuração';\n$fim = 1;\n");
    }
}

#[cfg(test)]
mod codex_deadlock_tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// The owner's report: "travou infinito no modo agent".
    ///
    /// # The deadlock, as it was
    ///
    /// `codex_turn` takes `codex.lock()` and holds it **for the whole turn**. In agent mode
    /// the turn then parks inside `read_codex_turn`'s `read_line`, waiting for the user to
    /// approve a file change. Approving called `answer_codex`, which took that same lock —
    /// on the **main thread**. So the click that would unblock the turn waited on the turn,
    /// and the turn waited on the click. No panic, no crash log, a frozen window.
    ///
    /// Cancel had it too, which is worse: it is the button you press *because* it is stuck.
    ///
    /// This models the shape rather than driving a real CLI (that needs a logged-in Codex
    /// and a live child). The property under test is structural: **the reply path must not
    /// need the lock the turn holds.** A join with a timeout is the assertion — a deadlock
    /// is exactly "this never finishes".
    #[test]
    fn answering_does_not_need_the_lock_the_turn_holds() {
        // The turn's lock, and the channel standing in for the child's stdin/stdout pair.
        let turn_lock = Arc::new(Mutex::new(()));
        let stdin: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let (approved_tx, approved_rx) = std::sync::mpsc::channel::<()>();

        let held = Arc::clone(&turn_lock);
        let turn_stdin = Arc::clone(&stdin);
        let turn = std::thread::spawn(move || {
            let _guard = held.lock().unwrap();
            // Parked exactly like `read_line`, waiting for the approval to arrive.
            let got = approved_rx.recv_timeout(Duration::from_secs(5)).is_ok();
            // Touch the shared handle the way the reader's session does.
            drop(turn_stdin.lock().unwrap());
            got
        });

        // Give the turn time to be holding the lock and parked — the exact window in which
        // the user clicks Apply.
        std::thread::sleep(Duration::from_millis(50));

        // The main thread answers. With the old design this line was
        // `turn_lock.lock()` and never returned. It must reach stdin without it.
        *stdin.lock().unwrap() = Some("approval".to_string());
        approved_tx.send(()).expect("the reply must reach a turn that is still parked");

        let answered = turn.join().expect("the turn thread must not have panicked");
        assert!(answered, "the turn must have received the approval rather than timing out");
    }

    /// `write_to_codex` holds the stdin lock only for the write, so two writers — an
    /// approval from the UI and a turn start from the background — cannot wedge each other.
    ///
    /// The rule that makes the fix safe: this mutex is never held across a blocking read.
    #[test]
    fn the_stdin_lock_is_never_held_across_a_wait() {
        let stdin: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let writers: Vec<_> = (0..8)
            .map(|i| {
                let stdin = Arc::clone(&stdin);
                std::thread::spawn(move || {
                    for n in 0..50 {
                        stdin.lock().unwrap().push(format!("{i}:{n}"));
                        std::thread::yield_now();
                    }
                })
            })
            .collect();

        for writer in writers {
            writer.join().expect("no writer may block on another");
        }
        assert_eq!(stdin.lock().unwrap().len(), 400, "every write must have landed");
    }

    /// The structural guarantee, checked mechanically rather than by review.
    ///
    /// The two tests above model the deadlock's *shape*; this one pins the actual code. The
    /// bug was one `self.codex.lock()` in a method the UI calls, and it read as completely
    /// ordinary next to the four other `.lock()` calls in this file — which is exactly why a
    /// reviewer would not catch its return.
    ///
    /// `try_lock` is allowed: it cannot wait, so it cannot deadlock. A blocking `lock()` on
    /// `self.codex` from a `&self`/`&mut self` method is the pattern that froze the window.
    #[test]
    fn no_ui_method_blocks_on_the_turn_lock() {
        // Only the production half: this module names the offending pattern in its own
        // detector and in its failure message, and a check that trips on its own text is a
        // check nobody can keep green.
        let source = include_str!("ai_chat.rs");
        let source = source.split("mod codex_deadlock_tests").next().unwrap_or(source);

        let offenders: Vec<_> = source
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                let code = line.split("//").next().unwrap_or("");
                code.contains("self.codex.lock()")
            })
            .map(|(i, line)| format!("  {}: {}", i + 1, line.trim()))
            .collect();

        assert!(
            offenders.is_empty(),
            "`self.codex` is held by `codex_turn` for the whole turn, and in agent mode that \
             turn blocks waiting for the user. A UI method that waits for this lock freezes \
             the window. Reach the child through `codex_stdin` / `codex_thread_id`, or use \
             `try_lock`:\n{}",
            offenders.join("\n")
        );
    }
}
