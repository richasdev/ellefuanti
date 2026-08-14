//! The AI provider layer (#29, #99): who we talk to, how, and what may never be sent.
//!
//! # The constraint that outranks every feature
//!
//! **Nothing leaves the machine without explicit consent.** Everything here is plumbing
//! for requests the *user* initiates; nothing in this module fires on its own. The
//! denylist at the bottom is the enforcement half: paths that look like secrets are
//! refused as context with no override.
//!
//! # Zero new dependencies, on purpose
//!
//! The transport is macOS's own `curl` run as a child process — the same pattern the
//! self-updater uses — and keys live in the macOS Keychain via the system `security`
//! tool. An HTTP client crate was measured against the 17 MB binary limit in #99 and
//! declined; `curl -N --no-buffer` streams SSE line-by-line and a kill is a cancel.
//!
//! # Two wire formats, four providers
//!
//! | Provider  | Wire      | Auth |
//! |-----------|-----------|------|
//! | Anthropic | Anthropic | API key from the Keychain (`x-api-key`) |
//! | `ant` CLI | Anthropic | OAuth token from `ant auth print-credentials` (Bearer) |
//! | Custom    | OpenAI    | Base URL + optional key — OpenAI, OpenRouter, local Ollama |
//! | Codex     | *none*    | The user's own `codex login` — see [`crate::ai_codex`] |
//!
//! The two wire formats are deliberately *not* one abstraction with adapters: each is a
//! body builder and an SSE parser, four small functions total, and #99 warns that one
//! interface stretched over both fits neither.
//!
//! Codex is the odd one out and stays that way: it is a subprocess speaking JSON-RPC, not
//! HTTP, so it has no [`Wire`] and never reaches [`resolve_auth`] or [`curl_args`]. That
//! is why [`Provider::wire`] answers `Option` — "which wire" has no answer for a provider
//! that is not on a wire, and a made-up one would send a chat body to a `curl` that has
//! no URL to POST it to.

use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

/// Which backend the user configured (`ai.provider`, or `ai.chat_provider` for chat).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider {
    Anthropic,
    AntCli,
    Custom,
    /// The user's own `codex` CLI, driven as a subprocess (#99). Not HTTP, no key.
    Codex,
}

impl Provider {
    pub fn from_setting(value: &str) -> Provider {
        match value {
            "ant" => Provider::AntCli,
            "custom" => Provider::Custom,
            "codex" => Provider::Codex,
            _ => Provider::Anthropic,
        }
    }

    pub fn setting_name(self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::AntCli => "ant",
            Provider::Custom => "custom",
            Provider::Codex => "codex",
        }
    }

    /// The next provider in the settings panel's cycler.
    pub fn next(self) -> Provider {
        match self {
            Provider::Anthropic => Provider::AntCli,
            Provider::AntCli => Provider::Custom,
            Provider::Custom => Provider::Codex,
            Provider::Codex => Provider::Anthropic,
        }
    }

    /// Which request format this provider speaks, or `None` when it is not on a wire at
    /// all. Codex is a subprocess (#99): the callers that build bodies and `curl` argv
    /// must branch on this rather than assume every provider POSTs somewhere.
    pub fn wire(self) -> Option<Wire> {
        match self {
            Provider::Anthropic | Provider::AntCli => Some(Wire::Anthropic),
            Provider::Custom => Some(Wire::OpenAi),
            Provider::Codex => None,
        }
    }

    /// Whether a key can even be set for this provider. Codex holds its own login, so the
    /// "Set API key…" button has nothing to write for it.
    pub fn takes_api_key(self) -> bool {
        self.wire().is_some()
    }
}

/// The request/response format on the wire.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wire {
    Anthropic,
    OpenAi,
}

/// A resolved connection: where to POST and with which headers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Auth {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// Resolves the configured provider to a URL and headers. **Blocking** — the Keychain
/// and the `ant` CLI are subprocesses — so call it off the main thread.
///
/// The error strings are user-facing: each says what is missing and where to fix it.
pub fn resolve_auth(provider: Provider, base_url: &str) -> Result<Auth, String> {
    match provider {
        Provider::Anthropic => {
            let key = keychain_get("anthropic")
                .ok_or("No Anthropic API key — set one in Settings (⌘,) → AI")?;
            Ok(Auth {
                url: "https://api.anthropic.com/v1/messages".to_string(),
                headers: vec![
                    ("x-api-key".to_string(), key),
                    ("anthropic-version".to_string(), "2023-06-01".to_string()),
                ],
            })
        }
        Provider::AntCli => {
            let token = ant_access_token()
                .ok_or("`ant` CLI not logged in — run `ant auth login` in a terminal")?;
            Ok(Auth {
                url: "https://api.anthropic.com/v1/messages".to_string(),
                headers: vec![
                    ("Authorization".to_string(), format!("Bearer {token}")),
                    // OAuth tokens ride Bearer and need this opt-in header; an API key
                    // conversion is a header change, not a key swap.
                    ("anthropic-beta".to_string(), "oauth-2025-04-20".to_string()),
                    ("anthropic-version".to_string(), "2023-06-01".to_string()),
                ],
            })
        }
        Provider::Custom => {
            if base_url.trim().is_empty() {
                return Err("No base URL — set \"ai.base_url\" in settings.json (e.g. \
                     http://localhost:11434/v1 for Ollama)"
                    .to_string());
            }
            let mut headers = vec![];
            // A key is optional here: a local Ollama has none, and that is the honest
            // default #99 wanted to build against.
            if let Some(key) = keychain_get("custom") {
                headers.push(("Authorization".to_string(), format!("Bearer {key}")));
            }
            Ok(Auth { url: openai_endpoint(base_url), headers })
        }
        // Unreachable through the chat panel, which branches to the subprocess path
        // before ever asking for auth — but a silent `Ok` with an empty URL would send a
        // `curl` at nothing, so the impossible case says so instead of guessing.
        Provider::Codex => {
            Err("Codex is not an HTTP provider — it runs the `codex` CLI (#99)".to_string())
        }
    }
}

/// Joins a base URL to the OpenAI-compatible chat endpoint, tolerating both spellings
/// people paste: with and without the `/v1`.
fn openai_endpoint(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

/// One conversation turn, provider-neutral.
#[derive(Clone, Debug)]
pub struct Turn {
    /// `"user"` or `"assistant"` — both wires use the same two words.
    pub role: &'static str,
    pub content: String,
    /// Images riding along with this turn's text. Empty for every turn that is not a
    /// send carrying attachments, which is why `content` stays the primary field: a turn
    /// with no images serialises to exactly the string form it always did.
    pub images: Vec<Image>,
}

impl Turn {
    /// The common case — a turn that is only words.
    pub fn text(role: &'static str, content: String) -> Turn {
        Turn { role, content, images: Vec::new() }
    }
}

/// An image attachment, already read and encoded, ready for either wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    /// An IANA type both providers accept: `image/png`, `image/jpeg`, `image/gif`,
    /// `image/webp`.
    pub media_type: String,
    /// Standard base64, no line breaks — what both wires want inside JSON.
    pub data: String,
}

/// The streaming request body for either wire.
pub fn chat_body(wire: Wire, model: &str, system: &str, turns: &[Turn], max_tokens: u32) -> String {
    let turn_values: Vec<Value> =
        turns.iter().map(|t| json!({"role": t.role, "content": turn_content(wire, t)})).collect();
    match wire {
        Wire::Anthropic => json!({
            "model": model,
            "max_tokens": max_tokens,
            "stream": true,
            "system": system,
            "messages": turn_values,
        })
        .to_string(),
        Wire::OpenAi => {
            let mut messages = vec![json!({"role": "system", "content": system})];
            messages.extend(turn_values);
            json!({
                "model": model,
                "max_tokens": max_tokens,
                "stream": true,
                "messages": messages,
            })
            .to_string()
        }
    }
}

/// One turn's `content` field, in whichever shape the wire needs.
///
/// A turn with no images stays a plain JSON string. That is not an optimisation — it is
/// the compatibility guarantee: every provider on either wire accepts the string form,
/// while the block form is a newer spelling that a local Ollama or an older OpenAI-shaped
/// gateway may not parse. Attachments are opt-in, and so is the risk of the richer body.
fn turn_content(wire: Wire, turn: &Turn) -> Value {
    if turn.images.is_empty() {
        return Value::String(turn.content.clone());
    }
    match wire {
        // Anthropic: text first, then one `image` block per attachment, each carrying its
        // own base64 source. Text leads because the prompt is what frames the images.
        Wire::Anthropic => {
            let mut blocks = vec![json!({"type": "text", "text": turn.content})];
            blocks.extend(turn.images.iter().map(|image| {
                json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": image.media_type,
                        "data": image.data,
                    },
                })
            }));
            Value::Array(blocks)
        }
        // OpenAI: the same idea with different nouns — `image_url` whose url is a `data:`
        // URI rather than a fetchable address, which is how the format carries bytes.
        Wire::OpenAi => {
            let mut blocks = vec![json!({"type": "text", "text": turn.content})];
            blocks.extend(turn.images.iter().map(|image| {
                json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{};base64,{}", image.media_type, image.data),
                    },
                })
            }));
            Value::Array(blocks)
        }
    }
}

/// The argv for the streaming `curl`, ready for a child process.
///
/// `-N`/`--no-buffer` is what makes SSE arrive line-by-line instead of in 16 KB gulps;
/// `-sS` keeps progress noise out of the stream while letting real errors through on
/// stderr. The body goes via stdin (`--data-binary @-`) so a long conversation never
/// hits an argv length limit and never shows up in `ps` output.
pub fn curl_args(auth: &Auth) -> Vec<String> {
    let mut args = vec![
        "-N".to_string(),
        "-sS".to_string(),
        "--no-buffer".to_string(),
        "-X".to_string(),
        "POST".to_string(),
        "-H".to_string(),
        "content-type: application/json".to_string(),
    ];
    for (name, value) in &auth.headers {
        args.push("-H".to_string());
        args.push(format!("{name}: {value}"));
    }
    args.push("--data-binary".to_string());
    args.push("@-".to_string());
    args.push(auth.url.clone());
    args
}

/// One parsed server-sent event, reduced to what a panel needs.
///
/// Deliberately *only* the three things an HTTP wire can say. Agent-mode events live in
/// [`AgentEvent`] instead: they arrive from the Codex subprocess and can never come off an
/// SSE stream, so putting them here would force every `parse_sse` caller — including
/// ghost-text autocomplete, which has no concept of a proposal — to match on cases its
/// transport cannot produce.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamEvent {
    /// A piece of assistant text to append.
    Delta(String),
    /// The reply finished normally.
    Done,
    /// The server said no; the string is for the user.
    Error(String),
}

/// What a panel's producer sends: reply text, or — Codex only — an agent proposal (#99).
///
/// One channel carries both, because the panel has exactly one drain and one cancel: a
/// second channel for proposals would mean a second batching story and a second way to
/// cancel half of a turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentEvent {
    /// Anything an HTTP wire could also have said.
    Stream(StreamEvent),
    /// The agent wants to change files, and has said which and how. Nothing is written:
    /// this is the proposal, and the approval below is the question about it.
    Proposed { item_id: String, changes: Vec<ProposedFileChange> },
    /// The agent is blocked waiting for a decision on `item_id`, at JSON-RPC `request_id`.
    ApprovalRequested { request_id: u64, item_id: String },
    /// The agent is blocked on a decision that is **not** about writing files: a command it
    /// wants to run, or permissions (an MCP server) it wants granted.
    ///
    /// Separate from `ApprovalRequested` because there is nothing to diff — the panel shows
    /// the sentence and two or three buttons, not a patch. `summary` is rendered verbatim;
    /// `allow_for_session` says whether the CLI offered a session-wide accept for this kind.
    /// The turn is visibly doing something; the label is display-ready ("Thinking…",
    /// "Running: composer install"). One at a time — a new one supersedes.
    Activity(String),
    /// The running item finished; the panel checks off its activity rows.
    ActivityEnded,
    ActionApprovalRequested {
        request_id: u64,
        summary: String,
        allow_for_session: bool,
        /// Which wire shape the answer takes — command decisions and MCP elicitations
        /// encode differently, and the button click is where that choice lands.
        kind: crate::ai_codex::ApprovalKind,
    },
}

impl From<StreamEvent> for AgentEvent {
    fn from(event: StreamEvent) -> Self {
        AgentEvent::Stream(event)
    }
}

/// One file an agent turn proposes to change, transport-neutral.
///
/// A copy of the Codex shape rather than a re-export, so [`StreamEvent`] — which every
/// provider's producer sends — does not drag a CLI-specific type into the HTTP path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProposedFileChange {
    pub path: String,
    /// `"add"`, `"update"` or `"delete"`.
    pub kind: String,
    /// The unified diff for this file.
    pub diff: String,
}

/// Parses one line of an SSE stream for the given wire. `None` for lines that carry
/// nothing a consumer acts on (event names, heartbeats, block boundaries).
pub fn parse_sse(wire: Wire, line: &str) -> Option<StreamEvent> {
    let data = line.strip_prefix("data:")?.trim();
    if data.is_empty() {
        return None;
    }
    match wire {
        Wire::Anthropic => {
            let value: Value = serde_json::from_str(data).ok()?;
            match value.get("type")?.as_str()? {
                "content_block_delta" => {
                    let text = value.get("delta")?.get("text")?.as_str()?;
                    Some(StreamEvent::Delta(text.to_string()))
                }
                "message_stop" => Some(StreamEvent::Done),
                "error" => {
                    let message = value
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("the provider returned an error");
                    Some(StreamEvent::Error(message.to_string()))
                }
                _ => None,
            }
        }
        Wire::OpenAi => {
            if data == "[DONE]" {
                return Some(StreamEvent::Done);
            }
            let value: Value = serde_json::from_str(data).ok()?;
            if let Some(message) =
                value.get("error").and_then(|e| e.get("message")).and_then(Value::as_str)
            {
                return Some(StreamEvent::Error(message.to_string()));
            }
            let text = value.get("choices")?.get(0)?.get("delta")?.get("content")?.as_str()?;
            Some(StreamEvent::Delta(text.to_string()))
        }
    }
}

/// A non-streaming error body, for when `curl` exits with the server's refusal on
/// stdout instead of an SSE stream (auth failures, bad models).
pub fn parse_error_body(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body.trim()).ok()?;
    let error = value.get("error")?;
    error.get("message").and_then(Value::as_str).map(str::to_string)
}

// --- Keychain -----------------------------------------------------------------------

const KEYCHAIN_SERVICE: &str = "ellefuanti-ai";

/// Stores a key in the macOS Keychain. Never the settings file — #99's hard rule.
/// Blocking; `security` ships with macOS.
pub fn keychain_set(account: &str, key: &str) -> Result<(), String> {
    let output = Command::new("security")
        .args(["add-generic-password", "-U", "-s", KEYCHAIN_SERVICE, "-a", account, "-w", key])
        .output()
        .map_err(|err| format!("could not run security: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Reads a key back. `None` covers both "not set" and "Keychain said no".
pub fn keychain_get(account: &str) -> Option<String> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-a", account, "-w"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if key.is_empty() { None } else { Some(key) }
}

/// A short-lived OAuth token from the `ant` CLI's active profile, when the user chose
/// the subscription/login route instead of pasting a key.
fn ant_access_token() -> Option<String> {
    let output =
        Command::new("ant").args(["auth", "print-credentials", "--access-token"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() { None } else { Some(token) }
}

// --- The denylist -------------------------------------------------------------------

/// Why a path may never be sent as context, or `None` when it is fine.
///
/// A Laravel root is full of exactly the files this exists for: `.env` with database
/// passwords, SSH keys, service-account JSON, the sqlite database itself. The check is
/// on the *name*, deliberately cheap and deliberately without an override — #99: "an
/// explicit denylist is cheap and the absence of one is not defensible."
pub fn deny_reason(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();

    if name == ".env" || name.starts_with(".env.") {
        return Some(".env files carry credentials");
    }
    if name.starts_with("id_rsa") || name.starts_with("id_ed25519") || name.starts_with("id_ecdsa")
    {
        return Some("SSH keys never leave the machine");
    }
    for ext in [".pem", ".key", ".p12", ".pfx", ".keystore", ".jks"] {
        if name.ends_with(ext) {
            return Some("private key material");
        }
    }
    for ext in [".sqlite", ".sqlite3", ".db"] {
        if name.ends_with(ext) {
            return Some("database contents");
        }
    }
    for marker in ["credential", "secret", "token", "password"] {
        if name.contains(marker) {
            return Some("the name says it holds secrets");
        }
    }
    if name == "auth.json" || name == ".npmrc" || name == ".netrc" {
        return Some("package-manager credentials");
    }
    None
}

// --- attachments --------------------------------------------------------------------

/// The largest file that may ride along, before encoding.
///
/// Chosen against the failure mode rather than a provider limit: base64 inflates by 4/3,
/// so 5 MB of pixels is ~6.7 MB of JSON in one request body that `curl` must buffer and
/// the provider must accept. Anthropic's own ceiling for an image block is 5 MB encoded,
/// which this stays under for anything a screenshot actually weighs. A refusal names this
/// number (see [`attachment_too_large`]) so the limit is a fact on screen, not a mystery.
pub const MAX_ATTACHMENT_BYTES: u64 = 5 * 1024 * 1024;

/// The refusal for a file over [`MAX_ATTACHMENT_BYTES`], worded like the denylist's: what
/// happened and why, no apology and no "try again".
pub fn attachment_too_large(name: &str, bytes: u64) -> String {
    format!(
        "{name} is {:.1} MB — over the {} MB attachment limit",
        bytes as f64 / (1024.0 * 1024.0),
        MAX_ATTACHMENT_BYTES / (1024 * 1024)
    )
}

/// The image media type for these bytes, or `None` when they are not an image we can
/// send.
///
/// Magic bytes, not the extension: a `.png` that is really a JPEG would be rejected by the
/// provider with an unhelpful error, and a screenshot dragged out of a browser can arrive
/// with any name at all. The four types are exactly the four both wires accept — anything
/// else (BMP, TIFF, HEIC, SVG) is not an image *for this purpose* even though it is one
/// for a human, so it falls through to `None` and gets refused by name.
pub fn image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    // RIFF....WEBP — the size field sits between the two markers, so both are checked.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Whether these bytes are text a model can read, as opposed to a binary blob that would
/// arrive as mojibake.
///
/// Valid UTF-8 is most of the answer, but not all of it: a small binary can be accidentally
/// valid UTF-8, and a NUL byte is the oldest and most reliable tell that a file is not
/// text. Both checks are cheap and both are needed.
pub fn looks_like_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

/// What an attached file turned out to be, once its bytes were read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachmentKind {
    /// An image, base64'd for a content block.
    Image(Image),
    /// Text, which rides in the prompt as a fenced block like the Current-file chip's —
    /// base64 would only cost tokens and hide the content from the transcript.
    Text(String),
}

/// Reads a file and decides what it can be attached as, or refuses with a reason meant
/// for the user's eyes.
///
/// Every refusal path in this function is a sentence that gets rendered next to the chip.
/// The order matters: the denylist first, because a secret must be refused *as* a secret
/// even when it would also be too large or binary, and the reason the user sees should be
/// the one that outranks the others.
pub fn read_attachment(path: &Path) -> Result<AttachmentKind, String> {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();

    if let Some(reason) = deny_reason(path) {
        return Err(format!("{name} stays local — {reason}"));
    }

    // The size check reads metadata rather than the file: refusing a 400 MB video should
    // not mean reading 400 MB first.
    let metadata =
        std::fs::metadata(path).map_err(|err| format!("could not read {name}: {err}"))?;
    if metadata.is_dir() {
        return Err(format!("{name} is a folder — attach files, not directories"));
    }
    if metadata.len() > MAX_ATTACHMENT_BYTES {
        return Err(attachment_too_large(&name, metadata.len()));
    }

    let bytes = std::fs::read(path).map_err(|err| format!("could not read {name}: {err}"))?;
    if let Some(media_type) = image_media_type(&bytes) {
        return Ok(AttachmentKind::Image(Image {
            media_type: media_type.to_string(),
            data: base64_encode(&bytes),
        }));
    }
    if looks_like_text(&bytes) {
        return Ok(AttachmentKind::Text(String::from_utf8_lossy(&bytes).into_owned()));
    }
    // A binary that is not one of the four image types: refused by name rather than sent
    // as mojibake, which is the failure mode that wastes a request and confuses the model.
    Err(format!("{name} is binary and not a PNG, JPEG, GIF or WebP — nothing to send"))
}

/// Standard base64 (RFC 4648) with padding and no line breaks.
///
/// Hand-rolled because a crate for twenty lines is not affordable: #99 measured an HTTP
/// client against the binary limit and declined, and the same arithmetic applies to an
/// encoder. The tests below pin it against the RFC's own vectors, which is the whole
/// reason it is safe to write this by hand.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    // Exact: four output characters per three input bytes, rounded up.
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        // The three bytes as one 24-bit number, short chunks zero-filled on the right.
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3F] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3F] as char);
        // The padding: a one-byte tail encodes two characters, a two-byte tail three.
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 0x3F] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 { ALPHABET[triple as usize & 0x3F] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn providers_round_trip_their_setting_names() {
        for provider in [Provider::Anthropic, Provider::AntCli, Provider::Custom, Provider::Codex] {
            assert_eq!(Provider::from_setting(provider.setting_name()), provider);
        }
        assert_eq!(Provider::from_setting("gibberish"), Provider::Anthropic, "unknown → default");
    }

    #[test]
    fn the_cycler_reaches_every_provider_and_comes_home() {
        // A provider the cycler cannot reach is a provider the settings panel cannot
        // select — which is how Codex would ship unusable.
        let mut seen = vec![Provider::Anthropic];
        let mut provider = Provider::Anthropic;
        for _ in 0..4 {
            provider = provider.next();
            if provider != Provider::Anthropic {
                seen.push(provider);
            }
        }
        assert_eq!(seen.len(), 4, "every provider is one ‹ › away: {seen:?}");
        assert!(seen.contains(&Provider::Codex));
        assert_eq!(provider, Provider::Anthropic, "the cycle wraps");
    }

    #[test]
    fn codex_is_not_on_a_wire_and_takes_no_key() {
        // The two facts every caller branches on: no body to build, no key to store.
        assert_eq!(Provider::Codex.wire(), None);
        assert!(!Provider::Codex.takes_api_key());
        for http in [Provider::Anthropic, Provider::AntCli, Provider::Custom] {
            assert!(http.wire().is_some(), "{http:?} POSTs somewhere");
            assert!(http.takes_api_key());
        }
        // And the HTTP auth path refuses it out loud rather than handing back an empty URL.
        assert!(resolve_auth(Provider::Codex, "").is_err());
    }

    #[test]
    fn the_custom_endpoint_tolerates_both_base_url_spellings() {
        assert_eq!(
            openai_endpoint("http://localhost:11434/v1"),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            openai_endpoint("https://openrouter.ai/api/v1/"),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            openai_endpoint("http://localhost:11434"),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn anthropic_deltas_and_stop_parse() {
        let wire = Wire::Anthropic;
        assert_eq!(
            parse_sse(
                wire,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Olá"}}"#
            ),
            Some(StreamEvent::Delta("Olá".to_string()))
        );
        assert_eq!(parse_sse(wire, r#"data: {"type":"message_stop"}"#), Some(StreamEvent::Done));
        assert_eq!(
            parse_sse(wire, "event: content_block_delta"),
            None,
            "event names carry nothing"
        );
        assert_eq!(
            parse_sse(wire, r#"data: {"type":"error","error":{"message":"overloaded"}}"#),
            Some(StreamEvent::Error("overloaded".to_string()))
        );
    }

    #[test]
    fn openai_deltas_and_done_parse() {
        let wire = Wire::OpenAi;
        assert_eq!(
            parse_sse(wire, r#"data: {"choices":[{"delta":{"content":"Oi"}}]}"#),
            Some(StreamEvent::Delta("Oi".to_string()))
        );
        assert_eq!(parse_sse(wire, "data: [DONE]"), Some(StreamEvent::Done));
        assert_eq!(
            parse_sse(wire, r#"data: {"choices":[{"delta":{}}]}"#),
            None,
            "role-only first chunk carries no text"
        );
    }

    #[test]
    fn the_two_bodies_carry_the_system_prompt_their_own_way() {
        let turns = [Turn::text("user", "hi".to_string())];
        let anthropic: Value =
            serde_json::from_str(&chat_body(Wire::Anthropic, "m", "sys", &turns, 100)).unwrap();
        assert_eq!(anthropic["system"], "sys");
        assert_eq!(anthropic["messages"].as_array().unwrap().len(), 1);
        assert_eq!(anthropic["stream"], true);

        let openai: Value =
            serde_json::from_str(&chat_body(Wire::OpenAi, "m", "sys", &turns, 100)).unwrap();
        let messages = openai["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system", "OpenAI's system prompt is a message");
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn curl_args_stream_and_carry_every_header() {
        let auth = Auth {
            url: "https://api.anthropic.com/v1/messages".to_string(),
            headers: vec![("x-api-key".to_string(), "sk-test".to_string())],
        };
        let args = curl_args(&auth);
        assert!(args.contains(&"-N".to_string()), "unbuffered, or it is not streaming");
        assert!(args.contains(&"x-api-key: sk-test".to_string()));
        assert!(args.contains(&"@-".to_string()), "body via stdin, never argv");
        assert_eq!(args.last().unwrap(), &auth.url);
    }

    #[test]
    fn the_denylist_refuses_what_a_laravel_root_is_full_of() {
        for secret in [
            ".env",
            ".env.local",
            "id_rsa",
            "id_ed25519.pub",
            "server.pem",
            "private.key",
            "database.sqlite",
            "credentials.json",
            "auth.json",
            ".npmrc",
            "api_token.txt",
        ] {
            assert!(deny_reason(&PathBuf::from(secret)).is_some(), "{secret} must be refused");
        }
    }

    #[test]
    fn the_denylist_lets_ordinary_source_through() {
        for fine in ["UserController.php", "web.php", "composer.json", "app.blade.php", ".envoy"] {
            assert!(deny_reason(&PathBuf::from(fine)).is_none(), "{fine} is ordinary source");
        }
    }

    // --- attachments ------------------------------------------------------------------

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        // The RFC's own table, which is why hand-rolling this is defensible: every tail
        // length and both padding cases are pinned against a published answer.
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(input.as_bytes()), expected, "encoding {input:?}");
        }
    }

    #[test]
    fn base64_covers_the_whole_alphabet_including_the_high_bytes() {
        // A PNG is mostly non-ASCII, so the `+` and `/` end of the alphabet is the part
        // that actually runs in production — an off-by-one there would corrupt an image
        // silently rather than failing loudly.
        assert_eq!(base64_encode(&[0xFB, 0xFF, 0xFE]), "+//+");
        assert_eq!(base64_encode(&[0x00, 0x00, 0x00]), "AAAA");
        assert_eq!(base64_encode(&[0xFF; 3]), "////");
        // Length is exactly 4/3 rounded up to a multiple of four, for every tail.
        for len in 0..16usize {
            let encoded = base64_encode(&vec![0xA5; len]);
            assert_eq!(encoded.len(), len.div_ceil(3) * 4, "length for {len} bytes");
        }
    }

    #[test]
    fn images_are_recognised_by_their_magic_bytes_not_their_names() {
        assert_eq!(
            image_media_type(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            Some("image/png")
        );
        assert_eq!(image_media_type(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(image_media_type(b"GIF89a....."), Some("image/gif"));

        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0x20, 0x00, 0x00, 0x00]); // the size field between markers
        webp.extend_from_slice(b"WEBP");
        assert_eq!(image_media_type(&webp), Some("image/webp"));

        // Not the four the wires accept, and not images at all: both are `None`, and the
        // caller turns that into a named refusal rather than a guess.
        assert_eq!(image_media_type(b"<?php echo 1;"), None);
        assert_eq!(image_media_type(b"BM\x00\x00"), None, "BMP is an image, but not one we send");
        assert_eq!(image_media_type(b"RIFF____AVI "), None, "a RIFF that is not a WebP");
        assert_eq!(image_media_type(b""), None);
    }

    #[test]
    fn text_and_binary_are_told_apart_by_nul_and_utf8() {
        assert!(looks_like_text(b"<?php\nclass User {}\n"));
        assert!(looks_like_text("acentuação — em UTF-8".as_bytes()));
        assert!(looks_like_text(b""));
        assert!(!looks_like_text(b"has a \x00 nul"), "a NUL is the oldest tell of a binary");
        assert!(!looks_like_text(&[0xFF, 0xFE, 0xFD]), "invalid UTF-8 is not text");
    }

    #[test]
    fn a_turn_without_images_still_serialises_as_a_plain_string() {
        // The compatibility guarantee: nothing about the existing body changed, so a
        // local Ollama that never saw a content block keeps working.
        let turns = [Turn::text("user", "hi".to_string())];
        for wire in [Wire::Anthropic, Wire::OpenAi] {
            let body: Value =
                serde_json::from_str(&chat_body(wire, "m", "sys", &turns, 100)).unwrap();
            let messages = body["messages"].as_array().unwrap();
            let user = messages.last().unwrap();
            assert_eq!(user["content"], "hi", "{wire:?} keeps the string form");
        }
    }

    #[test]
    fn an_image_turn_becomes_the_block_form_each_wire_understands() {
        let turns = [Turn {
            role: "user",
            content: "what is this?".to_string(),
            images: vec![Image { media_type: "image/png".to_string(), data: "Zm9v".to_string() }],
        }];

        let anthropic: Value =
            serde_json::from_str(&chat_body(Wire::Anthropic, "m", "sys", &turns, 100)).unwrap();
        let blocks = anthropic["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text", "the prompt leads, it frames the image");
        assert_eq!(blocks[0]["text"], "what is this?");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "Zm9v");

        let openai: Value =
            serde_json::from_str(&chat_body(Wire::OpenAi, "m", "sys", &turns, 100)).unwrap();
        // [0] is the system message; the user turn follows it.
        let blocks = openai["messages"][1]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "image_url");
        assert_eq!(
            blocks[1]["image_url"]["url"], "data:image/png;base64,Zm9v",
            "OpenAI carries bytes as a data: URI"
        );
    }

    #[test]
    fn several_images_all_ride_along_in_order() {
        let turns = [Turn {
            role: "user",
            content: "compare".to_string(),
            images: vec![
                Image { media_type: "image/png".to_string(), data: "AAAA".to_string() },
                Image { media_type: "image/jpeg".to_string(), data: "BBBB".to_string() },
            ],
        }];
        let body: Value =
            serde_json::from_str(&chat_body(Wire::Anthropic, "m", "sys", &turns, 100)).unwrap();
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3, "one text block plus both images");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[2]["source"]["media_type"], "image/jpeg");
    }

    #[test]
    fn reading_an_attachment_refuses_secrets_before_anything_else() {
        // The denylist outranks every other refusal, and it does so without the file
        // needing to exist — the name alone is the answer, which is what makes it cheap
        // and unforgeable.
        let refusal = read_attachment(&PathBuf::from("/nowhere/.env")).unwrap_err();
        assert!(refusal.contains("stays local"), "{refusal}");
        assert!(refusal.contains("credentials"), "the reason travels with it: {refusal}");
    }

    #[test]
    fn reading_an_attachment_classifies_text_images_and_binaries() {
        let dir = std::env::temp_dir().join(format!("elle-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let php = dir.join("User.php");
        std::fs::write(&php, "<?php\nclass User {}\n").unwrap();
        assert_eq!(
            read_attachment(&php),
            Ok(AttachmentKind::Text("<?php\nclass User {}\n".to_string())),
            "source attaches as text, never base64 — it would only cost tokens"
        );

        let png = dir.join("shot.png");
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(b"rest");
        std::fs::write(&png, &bytes).unwrap();
        match read_attachment(&png).unwrap() {
            AttachmentKind::Image(image) => {
                assert_eq!(image.media_type, "image/png");
                assert_eq!(image.data, base64_encode(&bytes));
            }
            other => panic!("a PNG must attach as an image, got {other:?}"),
        }

        // A binary that is not one of the four types is refused out loud rather than sent
        // as mojibake — the requirement that a wasted request never happens quietly.
        let blob = dir.join("compiled.bin");
        std::fs::write(&blob, [0x00, 0x01, 0x02, 0xFF]).unwrap();
        let refusal = read_attachment(&blob).unwrap_err();
        assert!(refusal.contains("binary"), "{refusal}");
        assert!(refusal.contains("compiled.bin"), "the refusal names the file: {refusal}");

        // A folder dragged in from Finder is a plausible gesture with no meaning here.
        let refusal = read_attachment(&dir).unwrap_err();
        assert!(refusal.contains("folder"), "{refusal}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_oversized_attachment_is_refused_with_the_number_on_screen() {
        let dir = std::env::temp_dir().join(format!("elle-attach-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("huge.png");
        std::fs::write(&big, vec![0u8; (MAX_ATTACHMENT_BYTES + 1) as usize]).unwrap();

        let refusal = read_attachment(&big).unwrap_err();
        assert!(refusal.contains("huge.png"), "{refusal}");
        assert!(refusal.contains("5 MB"), "the limit is a fact on screen: {refusal}");
        // And the message is the shared one, so the chip and this path cannot disagree.
        assert_eq!(refusal, attachment_too_large("huge.png", MAX_ATTACHMENT_BYTES + 1));

        std::fs::remove_dir_all(&dir).ok();
    }
}
