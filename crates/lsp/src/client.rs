//! The language server client: lifecycle, requests, and document sync.
//!
//! # The abstraction boundary
//!
//! This type is the boundary RISKS.md #2 is about. Everything above it — Laravel
//! intelligence, the editor, eventually a first-party PHP engine — talks to `Client`
//! and to `lsp-types`, never to a particular server. Everything a backend needs to
//! differ in lives in [`ServerConfig`], which is data.
//!
//! Concretely, the substitutability test is: **grep this crate for a backend name and
//! find only comments.** There is no `enum Backend`, no `if intelephense`, no quirk
//! table. Swapping in phpactor is a different `ServerConfig` value.
//!
//! # Blocking, like every other domain crate
//!
//! Every method here blocks (ADR-0007). The caller wraps them in
//! `cx.background_spawn`; this crate has no idea an executor exists. That is what makes
//! it testable at full speed against a mock server, with no runtime and no Intelephense
//! installed.

use std::time::Duration;

use anyhow::{Result, bail};
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;
use lsp_types::{
    ClientCapabilities, CompletionClientCapabilities, CompletionResponse, Diagnostic,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DocumentSymbolClientCapabilities, DocumentSymbolResponse, GeneralClientCapabilities,
    GotoDefinitionResponse, Hover, HoverClientCapabilities, InitializeParams, InitializeResult,
    Location, Position, PositionEncodingKind, PublishDiagnosticsParams, ServerCapabilities,
    SignatureHelp, SignatureHelpClientCapabilities, TextDocumentClientCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, Uri, WindowClientCapabilities,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use elle_text::Edit;

use crate::config::ServerConfig;
use crate::connection::{Connection, RequestOutcome, ServerMessage};
use crate::document::{SyncKind, TrackedDocument};
use crate::jsonrpc::{RequestId, Response, error_codes};
use crate::offset::OffsetEncoding;
use crate::process::{ServerProcess, path_to_uri, spawn};

/// How long to wait for `initialize` before giving up on the server.
///
/// Generous because a cold Intelephense indexing a large `vendor/` tree genuinely takes
/// this long on first run — but bounded, because §24 says a server that never starts
/// must not stop the user editing.
pub const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);

/// Default timeout for ordinary feature requests.
///
/// Short on purpose: a completion that arrives after ten seconds is not a completion,
/// it is a distraction. The caller can override per request.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to give a server to exit politely before it is killed.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// What the server said it can do, reduced to the parts we act on.
///
/// Deliberately not the raw `ServerCapabilities`: callers should ask "can this server
/// do completion" rather than pattern-match a deeply optional protocol struct, and
/// reducing it here is also where the encoding negotiation lands.
#[derive(Clone, Debug)]
pub struct Capabilities {
    pub encoding: OffsetEncoding,
    pub sync: SyncKind,
    pub completion: bool,
    pub hover: bool,
    pub definition: bool,
    pub references: bool,
    pub document_symbols: bool,
    pub signature_help: bool,
    /// Whether the server offers rename at all. Checked BEFORE the prompt opens (#19
    /// follow-up): Intelephense without a licence key does not, and a prompt whose
    /// Enter can only ever do nothing reads as a broken feature, not a missing one.
    pub rename: bool,
    /// Characters that should trigger completion, straight from the server.
    ///
    /// Read from the server rather than hardcoded as `["$", "-", ">", ":"]` — those are
    /// PHP's, and hardcoding them here would be exactly the backend-specific leak
    /// RISKS.md #2 forbids.
    pub completion_triggers: Vec<String>,
    pub signature_help_triggers: Vec<String>,
    /// The raw capabilities, for callers needing something not reduced above.
    pub raw: ServerCapabilities,
}

impl Capabilities {
    fn from_server(capabilities: ServerCapabilities) -> Self {
        let sync = match &capabilities.text_document_sync {
            Some(TextDocumentSyncCapability::Kind(kind)) => sync_kind(*kind),
            Some(TextDocumentSyncCapability::Options(options)) => {
                options.change.map_or(SyncKind::None, sync_kind)
            }
            // A server that says nothing about sync gets full documents. Guessing
            // incremental would corrupt its copy; full is slower but always correct.
            None => SyncKind::Full,
        };

        let completion_triggers = capabilities
            .completion_provider
            .as_ref()
            .and_then(|options| options.trigger_characters.clone())
            .unwrap_or_default();

        let signature_help_triggers = capabilities
            .signature_help_provider
            .as_ref()
            .and_then(|options| options.trigger_characters.clone())
            .unwrap_or_default();

        Self {
            encoding: OffsetEncoding::from_capability(capabilities.position_encoding.as_ref()),
            sync,
            completion: capabilities.completion_provider.is_some(),
            hover: capabilities.hover_provider.is_some(),
            definition: capabilities.definition_provider.is_some(),
            references: capabilities.references_provider.is_some(),
            document_symbols: capabilities.document_symbol_provider.is_some(),
            signature_help: capabilities.signature_help_provider.is_some(),
            rename: capabilities.rename_provider.is_some(),
            completion_triggers,
            signature_help_triggers,
            raw: capabilities,
        }
    }
}

fn sync_kind(kind: TextDocumentSyncKind) -> SyncKind {
    match kind {
        TextDocumentSyncKind::NONE => SyncKind::None,
        TextDocumentSyncKind::FULL => SyncKind::Full,
        TextDocumentSyncKind::INCREMENTAL => SyncKind::Incremental,
        // The enum is open (a bare u8 on the wire); an unknown kind gets full sync
        // rather than an assumption that could corrupt the server's copy.
        _ => SyncKind::Full,
    }
}

/// A connected language server.
pub struct Client {
    name: String,
    connection: Connection,
    process: Option<ServerProcess>,
    capabilities: Capabilities,
    documents: Vec<TrackedDocument>,
    /// Set once `shutdown` has been sent, so `Drop` does not try again.
    stopped: bool,
}

impl Client {
    /// Spawns the configured server and performs the handshake.
    ///
    /// If the handshake fails the child is killed rather than left running: a server
    /// that could not initialize is of no use to anyone, and leaking it would cost the
    /// user a stray process indexing their project forever.
    pub fn start(config: &ServerConfig) -> Result<Self> {
        let (mut process, pipes) = spawn(config)?;

        let connection = Connection::new(pipes.stdout, pipes.stdin, config.name.clone());
        match Self::handshake(config, connection) {
            Ok(mut client) => {
                client.process = Some(process);
                Ok(client)
            }
            Err(err) => {
                let _ = process.child.kill();
                let _ = process.child.wait();
                Err(err)
            }
        }
    }

    /// Performs the handshake over an existing connection.
    ///
    /// Public so tests can drive the whole lifecycle over in-memory pipes. This is the
    /// seam that keeps Intelephense out of the test suite entirely.
    pub fn connect(config: &ServerConfig, connection: Connection) -> Result<Self> {
        Self::handshake(config, connection)
    }

    /// initialize → (capabilities) → initialized.
    ///
    /// The ordering is not negotiable and the specification is strict about it: no
    /// request other than `initialize` may be sent before the response arrives, and no
    /// *other* request may be sent until `initialized` has gone out. Servers enforce
    /// this by erroring, and a client that gets it wrong fails intermittently.
    fn handshake(config: &ServerConfig, connection: Connection) -> Result<Self> {
        let params = InitializeParams {
            // `None` rather than our real pid: the field exists so a server can exit
            // when its parent dies, and we manage the child's lifetime explicitly.
            process_id: None,
            capabilities: client_capabilities(),
            initialization_options: config.initialization_options.clone(),
            workspace_folders: Some(vec![lsp_types::WorkspaceFolder {
                uri: path_to_uri(&config.root)?,
                name: config
                    .root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| config.name.clone()),
            }]),
            ..Default::default()
        };

        let outcome = connection.request(
            lsp_types::request::Initialize::METHOD,
            serde_json::to_value(params)?,
            INITIALIZE_TIMEOUT,
        )?;

        let result: InitializeResult = match outcome {
            RequestOutcome::Ok(value) => serde_json::from_value(value)?,
            RequestOutcome::Failed(error) => {
                bail!("the {} language server refused to initialize: {error}", config.name)
            }
            RequestOutcome::Cancelled => {
                bail!("initializing the {} language server was cancelled", config.name)
            }
        };

        let capabilities = Capabilities::from_server(result.capabilities);

        // Only now may other traffic flow.
        connection.notify(lsp_types::notification::Initialized::METHOD, json!({}))?;

        tracing::info!(
            server = %config.name,
            encoding = ?capabilities.encoding,
            sync = ?capabilities.sync,
            "language server ready"
        );

        Ok(Self {
            name: config.name.clone(),
            connection,
            process: None,
            capabilities,
            documents: Vec::new(),
            stopped: false,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    pub fn encoding(&self) -> OffsetEncoding {
        self.capabilities.encoding
    }

    /// Whether the server is still usable. A `false` here is the signal to degrade to
    /// syntax-only editing rather than to show an error on every keystroke.
    pub fn is_alive(&self) -> bool {
        !self.connection.is_closed()
    }

    pub fn failure(&self) -> Option<String> {
        self.connection.failure()
    }

    // --- document sync -----------------------------------------------------------

    /// Tells the server about a newly opened file.
    pub fn did_open(&mut self, uri: Uri, language_id: &str, text: &str) -> Result<()> {
        let document = TrackedDocument::new(uri.clone(), language_id, text);
        let params = DidOpenTextDocumentParams { text_document: document.item() };

        self.documents.retain(|d| d.uri != uri);
        self.documents.push(document);

        self.connection.notify(
            lsp_types::notification::DidOpenTextDocument::METHOD,
            serde_json::to_value(params)?,
        )
    }

    /// Forwards one edit.
    ///
    /// Takes an `elle_text::Edit` directly: it already carries the byte range and the
    /// replacement text, which is exactly a `TextDocumentContentChangeEvent` in the
    /// other unit system. Re-deriving a diff here would be work that the editor has
    /// already done, and a second chance to get it wrong.
    pub fn did_change(&mut self, uri: &Uri, edit: &Edit) -> Result<()> {
        let (sync, encoding) = (self.capabilities.sync, self.capabilities.encoding);

        let Some(document) = self.documents.iter_mut().find(|d| &d.uri == uri) else {
            // Not an error: an edit to a file the server was never told about is
            // ordinary when a language has no server.
            tracing::debug!(uri = uri.as_str(), "ignoring a change to an untracked document");
            return Ok(());
        };

        let changes = document.apply(edit, sync, encoding);
        if changes.is_empty() {
            return Ok(());
        }

        let params = DidChangeTextDocumentParams {
            text_document: document.versioned_identifier(),
            content_changes: changes,
        };

        self.connection.notify(
            lsp_types::notification::DidChangeTextDocument::METHOD,
            serde_json::to_value(params)?,
        )
    }

    /// Replaces the server's copy of a document with `text`.
    ///
    /// The resync path, for a caller that has the new text but not the edit that produced
    /// it. [`Client::did_change`] is the better call whenever an `Edit` is in hand — it
    /// sends only what changed — but a caller reading the buffer after the fact does not
    /// have one, and inventing a diff to look incremental would be a second chance to
    /// diverge from the server's copy for no bandwidth that matters at PHP file sizes.
    ///
    /// Ignores the server's declared [`SyncKind`] deliberately: a full replacement is
    /// valid for a server that asked for incremental changes, where the reverse is not.
    pub fn did_change_full(&mut self, uri: &Uri, text: &str) -> Result<()> {
        let Some(document) = self.documents.iter_mut().find(|d| &d.uri == uri) else {
            tracing::debug!(uri = uri.as_str(), "ignoring a resync of an untracked document");
            return Ok(());
        };

        let changes = document.reset(text);
        let params = DidChangeTextDocumentParams {
            text_document: document.versioned_identifier(),
            content_changes: changes,
        };

        self.connection.notify(
            lsp_types::notification::DidChangeTextDocument::METHOD,
            serde_json::to_value(params)?,
        )
    }

    pub fn did_close(&mut self, uri: &Uri) -> Result<()> {
        let Some(position) = self.documents.iter().position(|d| &d.uri == uri) else {
            return Ok(());
        };
        let document = self.documents.remove(position);

        let params = DidCloseTextDocumentParams { text_document: document.identifier() };
        self.connection.notify(
            lsp_types::notification::DidCloseTextDocument::METHOD,
            serde_json::to_value(params)?,
        )
    }

    pub fn document(&self, uri: &Uri) -> Option<&TrackedDocument> {
        self.documents.iter().find(|d| &d.uri == uri)
    }

    /// Converts a byte offset into the position to send for `uri`.
    pub fn position(&self, uri: &Uri, offset: usize) -> Option<Position> {
        self.document(uri).map(|d| d.position(offset, self.capabilities.encoding))
    }

    /// Converts a position from the server back into a byte offset.
    pub fn offset(&self, uri: &Uri, position: Position) -> Option<usize> {
        self.document(uri).map(|d| d.offset(position, self.capabilities.encoding))
    }

    // --- requests ------------------------------------------------------------------

    /// Sends a request without waiting, returning its id so it can be cancelled.
    ///
    /// This is the completion-while-typing path: issue, then cancel the previous one.
    pub fn send_request(&self, method: &str, params: Value) -> Result<RequestId> {
        self.connection.send(method, params)
    }

    /// Waits for a request issued by [`Client::send_request`], decoding its result.
    pub fn await_response<T: DeserializeOwned>(
        &self,
        id: &RequestId,
        timeout: Duration,
    ) -> Result<Option<T>> {
        decode(self.connection.wait(id, timeout)?, &self.name)
    }

    /// Takes the answer to a request if it has arrived, without blocking.
    ///
    /// For a caller on a thread it must not block — the UI thread — this is the counterpart
    /// to [`Client::await_response`]: poll, yield, poll again. See [`Connection::poll`] for
    /// why a zero-timeout `wait` cannot serve.
    ///
    /// The two layers of `Option` are not the same question and collapsing them would lose
    /// the one that matters: the outer is *has the server answered*, the inner is *did it
    /// have anything to say*. A caller polling in a loop must keep waiting on the first and
    /// stop on the second — `Some(None)` is "no definition found", which is a final answer.
    pub fn poll_response<T: DeserializeOwned>(&self, id: &RequestId) -> Result<Option<Option<T>>> {
        match self.connection.poll(id)? {
            Some(outcome) => decode(outcome, &self.name).map(Some),
            None => Ok(None),
        }
    }

    /// Cancels a pending request (`$/cancelRequest`).
    pub fn cancel(&self, id: &RequestId) {
        self.connection.cancel(id);
    }

    fn request<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<Option<T>> {
        decode(self.connection.request(method, params, REQUEST_TIMEOUT)?, &self.name)
    }

    pub fn completion(&self, uri: &Uri, offset: usize) -> Result<Option<CompletionResponse>> {
        self.request(
            lsp_types::request::Completion::METHOD,
            self.text_document_position(uri, offset)?,
        )
    }

    /// Issues a completion request without waiting, for the cancel-on-keystroke path.
    pub fn request_completion(&self, uri: &Uri, offset: usize) -> Result<RequestId> {
        self.send_request(
            lsp_types::request::Completion::METHOD,
            self.text_document_position(uri, offset)?,
        )
    }

    pub fn hover(&self, uri: &Uri, offset: usize) -> Result<Option<Hover>> {
        self.request(
            lsp_types::request::HoverRequest::METHOD,
            self.text_document_position(uri, offset)?,
        )
    }

    /// Issues a hover request without waiting.
    ///
    /// Hover fires on cursor movement, so it has the same superseding problem
    /// completion does: the answer to where the pointer *was* is not worth showing.
    pub fn request_hover(&self, uri: &Uri, offset: usize) -> Result<RequestId> {
        self.send_request(
            lsp_types::request::HoverRequest::METHOD,
            self.text_document_position(uri, offset)?,
        )
    }

    pub fn definition(&self, uri: &Uri, offset: usize) -> Result<Option<GotoDefinitionResponse>> {
        self.request(
            lsp_types::request::GotoDefinition::METHOD,
            self.text_document_position(uri, offset)?,
        )
    }

    /// Issues a go-to-definition request without waiting.
    pub fn request_definition(&self, uri: &Uri, offset: usize) -> Result<RequestId> {
        self.send_request(
            lsp_types::request::GotoDefinition::METHOD,
            self.text_document_position(uri, offset)?,
        )
    }

    pub fn references(
        &self,
        uri: &Uri,
        offset: usize,
        include_declaration: bool,
    ) -> Result<Option<Vec<Location>>> {
        self.request(
            lsp_types::request::References::METHOD,
            self.reference_params(uri, offset, include_declaration)?,
        )
    }

    /// Issues a find-references request without waiting.
    ///
    /// The one most worth cancelling: a whole-project search is the slowest thing a
    /// server does, and abandoning it is how the user closes the panel without waiting.
    pub fn request_references(
        &self,
        uri: &Uri,
        offset: usize,
        include_declaration: bool,
    ) -> Result<RequestId> {
        self.send_request(
            lsp_types::request::References::METHOD,
            self.reference_params(uri, offset, include_declaration)?,
        )
    }

    pub fn document_symbols(&self, uri: &Uri) -> Result<Option<DocumentSymbolResponse>> {
        self.request(
            lsp_types::request::DocumentSymbolRequest::METHOD,
            self.document_symbol_params(uri)?,
        )
    }

    /// Issues a document-symbol request without waiting.
    pub fn request_document_symbols(&self, uri: &Uri) -> Result<RequestId> {
        self.send_request(
            lsp_types::request::DocumentSymbolRequest::METHOD,
            self.document_symbol_params(uri)?,
        )
    }

    pub fn signature_help(&self, uri: &Uri, offset: usize) -> Result<Option<SignatureHelp>> {
        self.request(
            lsp_types::request::SignatureHelpRequest::METHOD,
            self.text_document_position(uri, offset)?,
        )
    }

    /// Issues a signature-help request without waiting.
    pub fn request_signature_help(&self, uri: &Uri, offset: usize) -> Result<RequestId> {
        self.send_request(
            lsp_types::request::SignatureHelpRequest::METHOD,
            self.text_document_position(uri, offset)?,
        )
    }

    /// Issues a code-action request for a byte range without waiting (#19).
    ///
    /// The diagnostics ride along in the context because servers decide what to offer
    /// *from* them — an empty context is a quieter server, so the caller passes whatever
    /// it holds for the range. The reply mixes actions (with edits) and bare commands;
    /// what to do with a command-only entry is the caller's decision.
    pub fn request_code_actions(
        &self,
        uri: &Uri,
        range: std::ops::Range<usize>,
        diagnostics: &[lsp_types::Diagnostic],
    ) -> Result<RequestId> {
        let Some(document) = self.document(uri) else {
            bail!("{} is not open on the {} language server", uri.as_str(), self.name);
        };
        let encoding = self.capabilities.encoding;
        self.send_request(
            lsp_types::request::CodeActionRequest::METHOD,
            json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": document.position(range.start, encoding),
                    "end": document.position(range.end, encoding),
                },
                "context": { "diagnostics": diagnostics },
            }),
        )
    }

    /// Issues a rename request without waiting (#19).
    ///
    /// The reply is a `WorkspaceEdit` that may touch files this editor has never opened;
    /// deciding what to do with those is the caller's problem, and the one rule worth
    /// stating here is the caller's too: a rename applied to *some* of its files is
    /// corruption, not partial progress.
    pub fn request_rename(&self, uri: &Uri, offset: usize, new_name: &str) -> Result<RequestId> {
        let mut params = self.text_document_position(uri, offset)?;
        params["newName"] = json!(new_name);
        self.send_request(lsp_types::request::Rename::METHOD, params)
    }

    /// Issues a workspace-wide symbol search without waiting (#19).
    ///
    /// No open-document guard, deliberately: the question is about the workspace, not a
    /// file, and demanding one would break the search in a window with no tabs yet. The
    /// server is the matcher — the query goes through as typed and the reply is already
    /// filtered, which is why the palette must not filter it a second time.
    pub fn request_workspace_symbols(&self, query: &str) -> Result<RequestId> {
        self.send_request(
            lsp_types::request::WorkspaceSymbolRequest::METHOD,
            json!({ "query": query }),
        )
    }

    /// Issues a whole-document formatting request without waiting (#19).
    ///
    /// `tab_size`/`insert_spaces` are the two options every server honours; the rest of
    /// `FormattingOptions` is server-specific tuning nothing here needs yet. The reply
    /// is `Option<Vec<TextEdit>>` — `None` means the server chose not to format, which
    /// is not an error and must not be reported as one (RISKS #4).
    pub fn request_formatting(
        &self,
        uri: &Uri,
        tab_size: u32,
        insert_spaces: bool,
    ) -> Result<RequestId> {
        if self.document(uri).is_none() {
            bail!("{} is not open on the {} language server", uri.as_str(), self.name);
        }
        self.send_request(
            lsp_types::request::Formatting::METHOD,
            json!({
                "textDocument": { "uri": uri },
                "options": { "tabSize": tab_size, "insertSpaces": insert_spaces },
            }),
        )
    }

    fn reference_params(
        &self,
        uri: &Uri,
        offset: usize,
        include_declaration: bool,
    ) -> Result<Value> {
        let mut params = self.text_document_position(uri, offset)?;
        params["context"] = json!({ "includeDeclaration": include_declaration });
        Ok(params)
    }

    /// Rejects a document the server was never told about, exactly as
    /// [`Client::text_document_position`] does — a symbol request carries no position,
    /// but asking about an unopened file is the same caller mistake either way.
    fn document_symbol_params(&self, uri: &Uri) -> Result<Value> {
        if self.document(uri).is_none() {
            bail!("{} is not open on the {} language server", uri.as_str(), self.name);
        }
        Ok(json!({ "textDocument": { "uri": uri } }))
    }

    fn text_document_position(&self, uri: &Uri, offset: usize) -> Result<Value> {
        let Some(document) = self.document(uri) else {
            bail!("{} is not open on the {} language server", uri.as_str(), self.name);
        };
        Ok(json!({
            "textDocument": { "uri": uri },
            "position": document.position(offset, self.capabilities.encoding),
        }))
    }

    // --- server-initiated traffic ----------------------------------------------------

    /// Takes everything the server has pushed since the last call.
    ///
    /// Pull rather than callback: a callback would mean this crate owning a threading
    /// model, and the app already has one (gpui's). The app drains this from a
    /// background task and applies the results on the main thread.
    pub fn drain_events(&self) -> Vec<ServerEvent> {
        self.connection.drain_inbox().into_iter().filter_map(|m| self.to_event(m)).collect()
    }

    /// Blocks until the server pushes something, or the timeout elapses.
    pub fn wait_for_events(&self, timeout: Duration) -> Vec<ServerEvent> {
        self.connection
            .wait_for_inbox(timeout)
            .into_iter()
            .filter_map(|m| self.to_event(m))
            .collect()
    }

    fn to_event(&self, message: ServerMessage) -> Option<ServerEvent> {
        match message {
            ServerMessage::Notification { method, params }
                if method == lsp_types::notification::PublishDiagnostics::METHOD =>
            {
                match serde_json::from_value::<PublishDiagnosticsParams>(params) {
                    Ok(params) => Some(ServerEvent::Diagnostics {
                        uri: params.uri,
                        version: params.version,
                        diagnostics: params.diagnostics,
                    }),
                    Err(err) => {
                        tracing::warn!(%err, "ignoring malformed diagnostics");
                        None
                    }
                }
            }
            ServerMessage::Notification { method, params } => {
                Some(ServerEvent::Notification { method, params })
            }
            ServerMessage::Request { id, method, params } => {
                // Answer the handful of requests that would otherwise hang the server,
                // and refuse the rest politely. A server left waiting on a reply can
                // stall its own request queue, which looks to the user like a hang.
                self.answer_server_request(&id, &method);
                Some(ServerEvent::Request { id, method, params })
            }
        }
    }

    /// Replies to a server-initiated request.
    ///
    /// ponytail: every request is currently refused with `MethodNotFound` except the
    /// ones with a trivially correct empty answer. That is honest — we genuinely do not
    /// implement dynamic registration or workspace configuration yet — and a refusal
    /// keeps the server moving. Implement `client/registerCapability` and
    /// `workspace/configuration` when a backend needs them.
    fn answer_server_request(&self, id: &RequestId, method: &str) {
        let response = match method {
            // Accepting registration without acting on it would be a lie; but these two
            // are the ones servers send unprompted at startup and treat a failure as
            // fatal, so they get an empty success.
            "client/registerCapability" | "client/unregisterCapability" => {
                Response::success(id.clone(), Value::Null)
            }
            "workspace/configuration" => Response::success(id.clone(), json!([])),
            "window/workDoneProgress/create" => Response::success(id.clone(), Value::Null),
            _ => Response::error(
                id.clone(),
                error_codes::METHOD_NOT_FOUND,
                format!("{method} is not implemented by this client"),
            ),
        };

        if let Err(err) = self.connection.respond(response) {
            tracing::debug!(%err, method, "could not reply to a server request");
        }
    }

    // --- lifecycle -------------------------------------------------------------------

    /// shutdown → exit, then waits for the process to end.
    ///
    /// The ordering matters: `shutdown` is a *request* the server answers, and only
    /// then may `exit` be sent. Sending `exit` first makes a conforming server exit
    /// with a non-zero status.
    ///
    /// Never returns an error for a server that is already gone — stopping something
    /// that has already stopped is a success, not a fault.
    pub fn stop(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;

        if self.connection.is_closed() {
            self.kill();
            return Ok(());
        }

        // A hung server must not hold the shutdown open; the kill below is the backstop.
        match self.connection.request(
            lsp_types::request::Shutdown::METHOD,
            Value::Null,
            SHUTDOWN_TIMEOUT,
        ) {
            Ok(_) => {}
            Err(err) => {
                tracing::debug!(%err, server = %self.name, "shutdown request failed; exiting anyway")
            }
        }

        let _ = self.connection.notify(lsp_types::notification::Exit::METHOD, Value::Null);
        // Most servers exit on stdin EOF; closing it covers those that ignore `exit`.
        self.connection.close_writer();
        self.connection.join_reader(SHUTDOWN_TIMEOUT);
        self.kill();
        Ok(())
    }

    /// Last resort for a server that ignored `exit`.
    ///
    /// A leaked language server is a runaway CPU and memory cost on the user's machine
    /// long after the editor closed, so politeness has a deadline.
    fn kill(&mut self) {
        let Some(process) = self.process.as_mut() else { return };
        match process.child.try_wait() {
            Ok(Some(_)) => {}
            _ => {
                let _ = process.child.kill();
                let _ = process.child.wait();
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // §24 again: dropping a client must never panic and must never leak a process.
        let _ = self.stop();
    }
}

/// Turns an outcome into an optional value.
///
/// `None` covers both "the server has nothing to say" (a `null` result, which is how
/// hover reports nothing) and "cancelled", because to a caller those are the same
/// thing: no answer to show.
fn decode<T: DeserializeOwned>(outcome: RequestOutcome, server: &str) -> Result<Option<T>> {
    match outcome {
        RequestOutcome::Ok(Value::Null) => Ok(None),
        RequestOutcome::Ok(value) => Ok(Some(serde_json::from_value(value)?)),
        RequestOutcome::Cancelled => Ok(None),
        RequestOutcome::Failed(error) if error.code == error_codes::METHOD_NOT_FOUND => {
            // A server that does not implement a feature is not broken. Degrading
            // quietly is what lets a lighter backend be a drop-in replacement for a
            // heavier one.
            tracing::debug!(server, %error, "the server does not implement this request");
            Ok(None)
        }
        RequestOutcome::Failed(error) => bail!("the {server} language server failed: {error}"),
    }
}

/// What this client tells servers it supports.
///
/// Kept honest: advertising a capability we do not implement makes servers send
/// traffic we then ignore, and the resulting misbehaviour is attributed to the server.
fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        general: Some(GeneralClientCapabilities {
            // Ordered by preference. UTF-8 first because it needs no conversion at all
            // on our side; UTF-16 last but always present, since it is the only
            // encoding every server is required to support.
            position_encodings: Some(vec![
                PositionEncodingKind::UTF8,
                PositionEncodingKind::UTF32,
                PositionEncodingKind::UTF16,
            ]),
            ..Default::default()
        }),
        text_document: Some(TextDocumentClientCapabilities {
            completion: Some(CompletionClientCapabilities::default()),
            hover: Some(HoverClientCapabilities::default()),
            signature_help: Some(SignatureHelpClientCapabilities::default()),
            document_symbol: Some(DocumentSymbolClientCapabilities::default()),
            ..Default::default()
        }),
        window: Some(WindowClientCapabilities::default()),
        ..Default::default()
    }
}

/// Something the server pushed at us.
#[derive(Debug)]
pub enum ServerEvent {
    Diagnostics {
        uri: Uri,
        version: Option<i32>,
        diagnostics: Vec<Diagnostic>,
    },
    /// Any other notification, passed through rather than dropped so a caller can act
    /// on `window/logMessage` and friends without this crate growing a case per method.
    Notification {
        method: String,
        params: Value,
    },
    /// A server-initiated request. Already answered; surfaced for visibility.
    Request {
        id: RequestId,
        method: String,
        params: Value,
    },
}
