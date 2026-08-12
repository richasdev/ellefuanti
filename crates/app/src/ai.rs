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
//! # Two wire formats, three providers
//!
//! | Provider  | Wire      | Auth |
//! |-----------|-----------|------|
//! | Anthropic | Anthropic | API key from the Keychain (`x-api-key`) |
//! | `ant` CLI | Anthropic | OAuth token from `ant auth print-credentials` (Bearer) |
//! | Custom    | OpenAI    | Base URL + optional key — OpenAI, OpenRouter, local Ollama |
//!
//! The two wire formats are deliberately *not* one abstraction with adapters: each is a
//! body builder and an SSE parser, four small functions total, and #99 warns that one
//! interface stretched over both fits neither.

use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

/// Which backend the user configured (`ai.provider` in settings).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider {
    Anthropic,
    AntCli,
    Custom,
}

impl Provider {
    pub fn from_setting(value: &str) -> Provider {
        match value {
            "ant" => Provider::AntCli,
            "custom" => Provider::Custom,
            _ => Provider::Anthropic,
        }
    }

    pub fn setting_name(self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::AntCli => "ant",
            Provider::Custom => "custom",
        }
    }

    /// The next provider in the settings panel's cycler.
    pub fn next(self) -> Provider {
        match self {
            Provider::Anthropic => Provider::AntCli,
            Provider::AntCli => Provider::Custom,
            Provider::Custom => Provider::Anthropic,
        }
    }

    pub fn wire(self) -> Wire {
        match self {
            Provider::Anthropic | Provider::AntCli => Wire::Anthropic,
            Provider::Custom => Wire::OpenAi,
        }
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
}

/// The streaming request body for either wire.
pub fn chat_body(wire: Wire, model: &str, system: &str, turns: &[Turn], max_tokens: u32) -> String {
    let turn_values: Vec<Value> =
        turns.iter().map(|t| json!({"role": t.role, "content": t.content})).collect();
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamEvent {
    /// A piece of assistant text to append.
    Delta(String),
    /// The reply finished normally.
    Done,
    /// The server said no; the string is for the user.
    Error(String),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn providers_round_trip_their_setting_names() {
        for provider in [Provider::Anthropic, Provider::AntCli, Provider::Custom] {
            assert_eq!(Provider::from_setting(provider.setting_name()), provider);
        }
        assert_eq!(Provider::from_setting("gibberish"), Provider::Anthropic, "unknown → default");
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
        let turns = [Turn { role: "user", content: "hi".to_string() }];
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
}
