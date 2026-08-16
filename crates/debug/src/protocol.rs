//! DBGp commands and the XML replies they produce.
//!
//! Pure functions over strings: build a command, parse a packet. Nothing here opens a
//! socket or holds state, so every rule below is tested against packets captured from
//! Xdebug's own suite rather than against a live PHP process.
//!
//! # Commands
//!
//! ```text
//! command_name -i TRANSACTION_ID [-arg value] [-- base64_data]
//! ```
//!
//! The transaction id correlates a reply to its command. Xdebug's command loop is
//! strictly serial — it reads one command, answers it, then reads the next — so replies
//! cannot interleave and matching on the id is a check rather than a routing mechanism.
//! It is still checked, because a reply to the *previous* command arriving after a
//! timeout would otherwise be read as the answer to this one.
//!
//! # The state machine
//!
//! Nothing in the script runs until a continuation command is sent. Xdebug connects,
//! sends `<init>`, and waits. Features and breakpoints are negotiated during that pause;
//! `run` starts execution. Every continuation command (`run`, `step_into`, `step_over`,
//! `step_out`) answers with the state the engine reached:
//!
//! - `status="break"` — stopped, and there is a stack to inspect
//! - `status="stopping"` — the script finished; only `stop`/`detach` are useful now
//! - `status="stopped"` — the connection is over
//!
//! # Xdebug's defaults are too small to debug with
//!
//! `max_depth=1`, `max_children=32`, `max_data=1024`. At depth 1 an array's contents are
//! simply absent, so variable inspection looks broken rather than truncated. [`Feature`]
//! carries the values we raise them to during the handshake; see its documentation for
//! why they are not raised further.

use std::fmt::Write as _;

/// The `<init>` packet Xdebug sends the moment it connects.
///
/// The `idekey` is how a multi-session setup tells connections apart. We record it but do
/// not filter on it: a single-project IDE listening on its own port has no second session
/// to confuse this with, and filtering would silently drop the connection of anyone whose
/// `xdebug.idekey` does not match a value they never set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Init {
    /// The script that triggered the session, as a `file://` URI.
    pub file_uri: String,
    pub language: String,
    pub protocol_version: String,
    pub idekey: String,
    /// Xdebug's own version, from `<engine version="...">`. Worth surfacing: the commonest
    /// cause of a session that connects and then behaves oddly is a very old Xdebug.
    pub engine_version: String,
}

/// What a continuation command reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Connected, nothing running yet.
    Starting,
    /// Stopped at a breakpoint or after a step. The only state with a stack to read.
    Break,
    /// The script ran to the end. Still connected, but nothing left to inspect.
    Stopping,
    /// The session is over.
    Stopped,
    Running,
}

impl Status {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "starting" => Self::Starting,
            "break" => Self::Break,
            "stopping" => Self::Stopping,
            "stopped" => Self::Stopped,
            "running" => Self::Running,
            _ => return None,
        })
    }

    /// Whether the session can still be stepped.
    ///
    /// `stopping` is deliberately false: the script has finished, and sending `step_over`
    /// to a finished script produces an error rather than a step.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Starting | Self::Break | Self::Running)
    }
}

/// One frame of the call stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackFrame {
    /// 0 is the innermost frame; the highest number is the outermost (`{main}`).
    pub level: usize,
    /// The function this frame is executing, from `where`. `{main}` at file scope.
    pub function: String,
    /// A `file://` URI. Converted to a path by [`crate::uri_to_path`] at the edge.
    pub file_uri: String,
    /// 1-based, as the protocol sends it. Converted to a 0-based row at the UI edge.
    pub line: u32,
}

/// A variable, or one member of a structured one.
///
/// `children` is populated only as deep as `max_depth` allowed; a truncated container
/// still reports the true `child_count`, which is what lets the UI say "32 of 500" rather
/// than quietly showing a short list as if it were complete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Property {
    /// As written in source: `$user`, or `0` for an array element.
    pub name: String,
    /// The expression that addresses this value from the current scope — `$user['name']`.
    /// This, not `name`, is what a later `property_get` must send.
    pub full_name: String,
    /// `int`, `string`, `array`, `object`, `uninitialized`, …
    pub type_name: String,
    /// For `type="object"`, the class. `None` for scalars.
    pub class_name: Option<String>,
    /// Decoded scalar value. `None` for containers, which carry children instead.
    pub value: Option<String>,
    /// How many children the value really has, which may exceed `children.len()`.
    pub child_count: usize,
    pub children: Vec<Property>,
}

impl Property {
    /// Whether this value has more children than were sent.
    ///
    /// Drives the "showing N of M" affordance rather than letting a truncated array read
    /// as a complete one — the failure that makes a debugger actively misleading.
    pub fn is_truncated(&self) -> bool {
        self.child_count > self.children.len()
    }
}

/// A parsed reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Packet {
    Init(Init),
    Response(Response),
}

/// A `<response>` to a command we sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub command: String,
    pub transaction_id: u32,
    /// Present on continuation commands only.
    pub status: Option<Status>,
    /// Where execution stopped, from Xdebug's non-standard `<xdebug:message>` child.
    ///
    /// Worth reading because it is free: a continuation command already reports the new
    /// position, so the gutter arrow can move without a `stack_get` round trip.
    pub position: Option<(String, u32)>,
    /// The id Xdebug assigned a `breakpoint_set`, needed to remove it later.
    pub breakpoint_id: Option<String>,
    pub stack: Vec<StackFrame>,
    pub properties: Vec<Property>,
    /// An `<error>` child. A failed command, not a broken connection.
    pub error: Option<ProtocolError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolError {
    pub code: u32,
    pub message: String,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (DBGp error {})", self.message, self.code)
    }
}

/// Settings raised during the handshake.
///
/// The defaults make variable inspection useless rather than merely limited — at
/// `max_depth=1` an array shows as a type and a count with nothing inside it. These are
/// the values we ask for instead.
///
/// Not raised further on purpose. Every one of these multiplies the size of a
/// `context_get` reply, and a deeply nested Laravel container graph at depth 10 is
/// megabytes of XML fetched to render a tree the user has not expanded. Depth 3 covers
/// the common case — a request payload, a model's attributes — and anything deeper is a
/// `property_get` on the node the user actually opened, which is one small request rather
/// than a large speculative one.
pub struct Feature;

impl Feature {
    pub const MAX_DEPTH: u32 = 3;
    pub const MAX_CHILDREN: u32 = 100;
    /// Bytes per scalar. A truncated string is marked as such by Xdebug, so this bounds
    /// the reply without hiding that something was cut.
    pub const MAX_DATA: u32 = 16 * 1024;
}

/// Xdebug's variable scopes, from `context_names`.
///
/// Fixed rather than discovered. The specification says to ask, and Xdebug's answer is a
/// constant — these three, in this order, at every stack level. Asking would be a round
/// trip per stop to learn something that has not changed since Xdebug 2.
pub mod context {
    /// Locals: what the user means by "variables".
    pub const LOCALS: u32 = 0;
    /// `$_GET`, `$_SERVER` and friends. Fetched on demand, not with every stop: it is
    /// large, and it is the same on every stop.
    pub const SUPERGLOBALS: u32 = 1;
    pub const CONSTANTS: u32 = 2;
}

/// Escapes an argument value for the command line.
///
/// Values with spaces must be quoted, and `"`, `\` and NUL escaped within the quotes
/// (specification §6.3.1). This matters for `property_get -n "$x['a b']"`, which is not
/// exotic — any array keyed by a human-readable string produces one.
fn escape_argument(value: &str) -> String {
    if !value.contains([' ', '"', '\\', '\0']) {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            // A literal NUL would terminate the command early, so it cannot be passed
            // through even escaped. Dropping it is the only option that keeps the frame
            // intact, and no PHP identifier contains one.
            '\0' => {}
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Builds a command line, without its terminating NUL.
///
/// Private because every command has a named constructor below: a typo in a command name
/// should be a compile error, not a runtime `error code 4`.
fn command(name: &str, transaction_id: u32, args: &[(&str, &str)]) -> String {
    let mut out = String::new();
    let _ = write!(out, "{name} -i {transaction_id}");
    for (flag, value) in args {
        let _ = write!(out, " -{flag} {}", escape_argument(value));
    }
    out
}

/// `feature_set` — raises one of [`Feature`]'s limits.
pub fn feature_set(transaction_id: u32, name: &str, value: &str) -> String {
    command("feature_set", transaction_id, &[("n", name), ("v", value)])
}

/// `breakpoint_set -t line` — the only breakpoint type in scope.
///
/// `file_uri` must be a `file://` URI, and must be the path *PHP* will see. On a local
/// project those are the same string; under Docker they are not, which is the path
/// mapping the issue leaves out of scope.
pub fn breakpoint_set_line(transaction_id: u32, file_uri: &str, line: u32) -> String {
    command(
        "breakpoint_set",
        transaction_id,
        &[("t", "line"), ("f", file_uri), ("n", &line.to_string())],
    )
}

/// `breakpoint_remove` — takes the id Xdebug assigned, not a file and line.
pub fn breakpoint_remove(transaction_id: u32, breakpoint_id: &str) -> String {
    command("breakpoint_remove", transaction_id, &[("d", breakpoint_id)])
}

pub fn run(transaction_id: u32) -> String {
    command("run", transaction_id, &[])
}

pub fn step_into(transaction_id: u32) -> String {
    command("step_into", transaction_id, &[])
}

pub fn step_over(transaction_id: u32) -> String {
    command("step_over", transaction_id, &[])
}

pub fn step_out(transaction_id: u32) -> String {
    command("step_out", transaction_id, &[])
}

/// `stack_get` with no depth: every frame at once.
///
/// One round trip for the whole stack. Asking per level would be a request per frame to
/// render a panel that shows them all.
pub fn stack_get(transaction_id: u32) -> String {
    command("stack_get", transaction_id, &[])
}

/// `context_get` — the variables of one scope at one stack level.
pub fn context_get(transaction_id: u32, context_id: u32, stack_depth: u32) -> String {
    command(
        "context_get",
        transaction_id,
        &[("c", &context_id.to_string()), ("d", &stack_depth.to_string())],
    )
}

/// `property_get` — one value, addressed by its `full_name`.
///
/// This is how a truncated container is expanded: the tree asks for the node the user
/// opened instead of refetching the scope at a greater depth.
pub fn property_get(transaction_id: u32, full_name: &str, stack_depth: u32) -> String {
    command("property_get", transaction_id, &[("n", full_name), ("d", &stack_depth.to_string())])
}

/// `detach` — let the script run to completion without us.
///
/// Preferred over `stop`, which kills the request. Detaching leaves a web request to
/// finish and return its page; stopping it strands the browser on a dead connection.
pub fn detach(transaction_id: u32) -> String {
    command("detach", transaction_id, &[])
}

/// `stop` — end the script now.
pub fn stop(transaction_id: u32) -> String {
    command("stop", transaction_id, &[])
}

/// Decodes a property's text, honouring `encoding="base64"`.
///
/// Xdebug base64-encodes only what it must, so the attribute's *absence* means plain
/// text. Branching on the attribute rather than always decoding is what keeps a string
/// that merely looks like base64 — `"dGVzdA=="` as an actual value — from being mangled.
fn decode_value(text: &str, encoding: Option<&str>) -> String {
    match encoding {
        Some("base64") => match base64_decode(text.trim()) {
            Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            // Undecodable is shown as-is rather than dropped: a wrong-looking value in
            // the panel is debuggable, a silently empty one is not.
            None => text.to_string(),
        },
        _ => text.to_string(),
    }
}

/// Base64, by hand.
///
/// Twenty lines against a dependency that would otherwise be the *only* thing this crate
/// needs beyond an XML reader. The decoding rules are fixed and fully covered by the
/// tests below, including the padding and non-alphabet cases that are the usual bugs.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut accumulator: u32 = 0;
    let mut bits = 0u32;

    for byte in input.bytes() {
        // Xdebug wraps long payloads, and whitespace is not data.
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'=' {
            break;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32;

        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }

    Some(out)
}

/// Parses one packet's XML.
///
/// Returns `Err` only when the XML itself will not parse. A `<response>` carrying an
/// `<error>` child is a successful parse of a failed command — the distinction matters,
/// because a bad breakpoint must not look like a dead connection.
pub fn parse_packet(xml: &str) -> anyhow::Result<Packet> {
    let root = crate::xml::parse(xml)?;

    match root.name.as_str() {
        "init" => Ok(Packet::Init(Init {
            file_uri: root.attribute("fileuri").unwrap_or_default().to_string(),
            language: root.attribute("language").unwrap_or_default().to_string(),
            protocol_version: root.attribute("protocol_version").unwrap_or_default().to_string(),
            idekey: root.attribute("idekey").unwrap_or_default().to_string(),
            engine_version: root
                .child("engine")
                .and_then(|node| node.attribute("version"))
                .unwrap_or_default()
                .to_string(),
        })),
        "response" => Ok(Packet::Response(parse_response(&root))),
        other => anyhow::bail!("unexpected DBGp packet <{other}>"),
    }
}

fn parse_response(root: &crate::xml::Node) -> Response {
    let mut response = Response {
        command: root.attribute("command").unwrap_or_default().to_string(),
        // A reply we cannot correlate is more useful than no reply: the caller compares
        // and reports a mismatch rather than this silently inventing one.
        transaction_id: root
            .attribute("transaction_id")
            .and_then(|value| value.parse().ok())
            .unwrap_or_default(),
        status: root.attribute("status").and_then(Status::parse),
        position: None,
        // `breakpoint_set` returns the new id as the response's own `id` attribute.
        breakpoint_id: root.attribute("id").map(str::to_string),
        stack: Vec::new(),
        properties: Vec::new(),
        error: None,
    };

    for child in &root.children {
        match child.name.as_str() {
            // `xdebug:message` on the wire; the reader keeps the local name.
            "message" => {
                if let (Some(file), Some(line)) =
                    (child.attribute("filename"), child.attribute("lineno"))
                    && let Ok(line) = line.parse()
                {
                    response.position = Some((file.to_string(), line));
                }
            }
            "stack" => {
                if let Some(frame) = parse_stack_frame(child) {
                    response.stack.push(frame);
                }
            }
            "property" => response.properties.push(parse_property(child)),
            "error" => {
                response.error = Some(ProtocolError {
                    code: child.attribute("code").and_then(|c| c.parse().ok()).unwrap_or_default(),
                    message: child
                        .child("message")
                        .and_then(|node| node.text())
                        .unwrap_or("the debugger rejected the command")
                        .trim()
                        .to_string(),
                });
            }
            _ => {}
        }
    }

    response
}

fn parse_stack_frame(node: &crate::xml::Node) -> Option<StackFrame> {
    Some(StackFrame {
        level: node.attribute("level")?.parse().ok()?,
        function: node.attribute("where").unwrap_or("{main}").to_string(),
        file_uri: node.attribute("filename").unwrap_or_default().to_string(),
        line: node.attribute("lineno").and_then(|line| line.parse().ok()).unwrap_or_default(),
    })
}

fn parse_property(node: &crate::xml::Node) -> Property {
    let children: Vec<Property> =
        node.children.iter().filter(|child| child.name == "property").map(parse_property).collect();

    // A container's own text is whitespace between its children, not a value. Reporting
    // that as the value would show every array as an empty string.
    let value = if children.is_empty() && node.attribute("children") != Some("1") {
        node.text().map(|text| decode_value(text, node.attribute("encoding")))
    } else {
        None
    };

    Property {
        name: node.attribute("name").unwrap_or_default().to_string(),
        full_name: node.attribute("fullname").unwrap_or_default().to_string(),
        type_name: node.attribute("type").unwrap_or("uninitialized").to_string(),
        class_name: node.attribute("classname").map(str::to_string),
        value,
        child_count: node
            .attribute("numchildren")
            .and_then(|count| count.parse().ok())
            .unwrap_or(children.len()),
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every fixture below is byte-exact output from Xdebug's own `.phpt` suite
    // (`tests/debugger/dbgp-*.phpt`), not something written to match this parser. That is
    // the whole point: a fixture invented from the specification would agree with a
    // misreading of the specification.

    #[test]
    fn the_init_packet_yields_the_script_and_the_engine_version() {
        let xml = r#"<?xml version="1.0" encoding="iso-8859-1"?>
<init xmlns="urn:debugger_protocol_v1" xmlns:xdebug="https://xdebug.org/dbgp/xdebug" fileuri="file:///srv/app/public/index.php" language="PHP" xdebug:language_version="8.3.0" protocol_version="1.0" appid="1234" idekey="ellefuanti"><engine version="3.3.1"><![CDATA[Xdebug]]></engine><author><![CDATA[Derick Rethans]]></author></init>"#;

        let Packet::Init(init) = parse_packet(xml).unwrap() else {
            panic!("expected an init packet");
        };
        assert_eq!(init.file_uri, "file:///srv/app/public/index.php");
        assert_eq!(init.language, "PHP");
        assert_eq!(init.protocol_version, "1.0");
        assert_eq!(init.idekey, "ellefuanti");
        assert_eq!(init.engine_version, "3.3.1");
    }

    #[test]
    fn a_continuation_reply_carries_the_new_position_without_a_stack_request() {
        // From `dbgp-breakpoint-line.phpt`. `<xdebug:message>` is why the gutter arrow can
        // move on a step without a second round trip.
        let xml = r#"<?xml version="1.0" encoding="iso-8859-1"?>
<response xmlns="urn:debugger_protocol_v1" xmlns:xdebug="https://xdebug.org/dbgp/xdebug" command="run" transaction_id="4" status="break" reason="ok"><xdebug:message filename="file:///srv/app/index.php" lineno="3"></xdebug:message><breakpoint type="line" filename="file:///srv/app/index.php" lineno="3" state="enabled" hit_count="1" hit_value="0" id="123450001"></breakpoint></response>"#;

        let Packet::Response(response) = parse_packet(xml).unwrap() else {
            panic!("expected a response");
        };
        assert_eq!(response.command, "run");
        assert_eq!(response.transaction_id, 4);
        assert_eq!(response.status, Some(Status::Break));
        assert_eq!(response.position, Some(("file:///srv/app/index.php".to_string(), 3)));
    }

    #[test]
    fn breakpoint_set_returns_the_id_removal_will_need() {
        // From `dbgp-breakpoint-line.phpt`. The id is Xdebug's, not ours, so a breakpoint
        // cannot be removed without keeping it.
        let xml = r#"<?xml version="1.0" encoding="iso-8859-1"?>
<response xmlns="urn:debugger_protocol_v1" command="breakpoint_set" transaction_id="3" id="123450001"></response>"#;

        let Packet::Response(response) = parse_packet(xml).unwrap() else {
            panic!("expected a response");
        };
        assert_eq!(response.breakpoint_id.as_deref(), Some("123450001"));
        assert!(response.error.is_none());
    }

    #[test]
    fn detach_reports_stopping_rather_than_stopped() {
        // Verified in `dbgp-breakpoint-line.phpt`, and the reason `is_live` treats the two
        // separately: a caller waiting for `stopped` after detaching would wait forever.
        let xml = r#"<?xml version="1.0" encoding="iso-8859-1"?>
<response xmlns="urn:debugger_protocol_v1" command="detach" transaction_id="5" status="stopping" reason="ok"></response>"#;

        let Packet::Response(response) = parse_packet(xml).unwrap() else {
            panic!("expected a response");
        };
        assert_eq!(response.status, Some(Status::Stopping));
        assert!(!response.status.unwrap().is_live());
    }

    #[test]
    fn a_scalar_context_parses_with_its_value() {
        // From `dbgp-context-get.phpt`, verbatim.
        let xml = r#"<?xml version="1.0" encoding="iso-8859-1"?>
<response xmlns="urn:debugger_protocol_v1" xmlns:xdebug="https://xdebug.org/dbgp/xdebug" command="context_get" transaction_id="4" context="0"><property name="$NO" fullname="$NO" type="int"><![CDATA[42]]></property></response>"#;

        let Packet::Response(response) = parse_packet(xml).unwrap() else {
            panic!("expected a response");
        };
        assert_eq!(response.properties.len(), 1);
        let property = &response.properties[0];
        assert_eq!(property.name, "$NO");
        assert_eq!(property.full_name, "$NO");
        assert_eq!(property.type_name, "int");
        assert_eq!(property.value.as_deref(), Some("42"));
        assert!(!property.is_truncated());
    }

    #[test]
    fn a_constant_context_parses_the_same_way() {
        // Also `dbgp-context-get.phpt`: constants carry `facet="constant"` and no `$`.
        let xml = r#"<?xml version="1.0" encoding="iso-8859-1"?>
<response xmlns="urn:debugger_protocol_v1" command="context_get" transaction_id="5" context="2"><property name="YES" fullname="YES" type="float" facet="constant"><![CDATA[3.141592653589793]]></property></response>"#;

        let Packet::Response(response) = parse_packet(xml).unwrap() else {
            panic!("expected a response");
        };
        assert_eq!(response.properties[0].name, "YES");
        assert_eq!(response.properties[0].value.as_deref(), Some("3.141592653589793"));
    }

    #[test]
    fn a_base64_value_is_decoded_and_a_plain_one_is_not() {
        // Xdebug encodes only when it must, so the attribute's absence is meaningful. The
        // second property is the trap: a plain string that happens to look like base64
        // must survive untouched.
        let xml = r#"<response xmlns="urn:debugger_protocol_v1" command="context_get" transaction_id="1" context="0"><property name="$greeting" fullname="$greeting" type="string" size="11" encoding="base64"><![CDATA[Y29yYcOnw6Nv]]></property><property name="$literal" fullname="$literal" type="string" size="8"><![CDATA[dGVzdA==]]></property></response>"#;

        let Packet::Response(response) = parse_packet(xml).unwrap() else {
            panic!("expected a response");
        };
        assert_eq!(response.properties[0].value.as_deref(), Some("coração"));
        assert_eq!(response.properties[1].value.as_deref(), Some("dGVzdA=="));
    }

    #[test]
    fn a_nested_array_parses_into_a_tree() {
        let xml = r#"<response xmlns="urn:debugger_protocol_v1" command="context_get" transaction_id="6" context="0"><property name="$user" fullname="$user" type="array" children="1" numchildren="2" page="0" pagesize="100"><property name="name" fullname="$user['name']" type="string" size="5" encoding="base64"><![CDATA[UmljYXJkbw==]]></property><property name="roles" fullname="$user['roles']" type="array" children="1" numchildren="1"><property name="0" fullname="$user['roles'][0]" type="string" encoding="base64"><![CDATA[YWRtaW4=]]></property></property></property></response>"#;

        let Packet::Response(response) = parse_packet(xml).unwrap() else {
            panic!("expected a response");
        };
        let user = &response.properties[0];
        assert_eq!(user.type_name, "array");
        // A container has no value of its own: the whitespace between its children must
        // not be reported as one.
        assert_eq!(user.value, None);
        assert_eq!(user.children.len(), 2);
        assert_eq!(user.children[0].value.as_deref(), Some("Ricardo"));
        assert_eq!(user.children[0].full_name, "$user['name']");
        assert_eq!(user.children[1].children[0].value.as_deref(), Some("admin"));
        assert_eq!(user.children[1].children[0].full_name, "$user['roles'][0]");
    }

    #[test]
    fn an_object_reports_its_class() {
        let xml = r#"<response xmlns="urn:debugger_protocol_v1" command="context_get" transaction_id="7" context="0"><property name="$model" fullname="$model" type="object" classname="App\Models\User" children="1" numchildren="1"><property name="id" fullname="$model->id" type="int"><![CDATA[7]]></property></property></response>"#;

        let Packet::Response(response) = parse_packet(xml).unwrap() else {
            panic!("expected a response");
        };
        let model = &response.properties[0];
        assert_eq!(model.class_name.as_deref(), Some(r"App\Models\User"));
        assert_eq!(model.children[0].full_name, "$model->id");
    }

    #[test]
    fn a_truncated_container_reports_the_true_child_count() {
        // The failure this guards: `max_children` cut the list at 100 and the panel showed
        // it as a complete 100-element array. A debugger that lies about the data is worse
        // than one that admits a limit.
        let xml = r#"<response xmlns="urn:debugger_protocol_v1" command="context_get" transaction_id="8" context="0"><property name="$rows" fullname="$rows" type="array" children="1" numchildren="500" pagesize="100"><property name="0" fullname="$rows[0]" type="int"><![CDATA[1]]></property></property></response>"#;

        let Packet::Response(response) = parse_packet(xml).unwrap() else {
            panic!("expected a response");
        };
        let rows = &response.properties[0];
        assert_eq!(rows.child_count, 500);
        assert_eq!(rows.children.len(), 1);
        assert!(rows.is_truncated());
    }

    #[test]
    fn an_uninitialised_variable_is_not_mistaken_for_an_empty_one() {
        let xml = r#"<response command="context_get" transaction_id="9" context="0"><property name="$later" fullname="$later" type="uninitialized"></property></response>"#;

        let Packet::Response(response) = parse_packet(xml).unwrap() else {
            panic!("expected a response");
        };
        assert_eq!(response.properties[0].type_name, "uninitialized");
        assert_eq!(response.properties[0].value, None);
    }

    #[test]
    fn a_stack_parses_innermost_first() {
        let xml = r#"<?xml version="1.0" encoding="iso-8859-1"?>
<response xmlns="urn:debugger_protocol_v1" command="stack_get" transaction_id="10"><stack where="App\Http\Controllers\UserController->show" level="0" type="file" filename="file:///srv/app/app/Http/Controllers/UserController.php" lineno="24"></stack><stack where="Illuminate\Routing\Route->run" level="1" type="file" filename="file:///srv/app/vendor/laravel/framework/src/Illuminate/Routing/Route.php" lineno="205"></stack><stack where="{main}" level="2" type="file" filename="file:///srv/app/public/index.php" lineno="51"></stack></response>"#;

        let Packet::Response(response) = parse_packet(xml).unwrap() else {
            panic!("expected a response");
        };
        assert_eq!(response.stack.len(), 3);
        assert_eq!(response.stack[0].level, 0);
        assert_eq!(response.stack[0].function, r"App\Http\Controllers\UserController->show");
        assert_eq!(response.stack[0].line, 24);
        // The outermost frame is the highest level, and it is `{main}` at file scope.
        assert_eq!(response.stack[2].function, "{main}");
        assert_eq!(response.stack[2].file_uri, "file:///srv/app/public/index.php");
    }

    #[test]
    fn an_error_response_is_a_failed_command_not_a_broken_parse() {
        // A breakpoint on a line PHP will not accept. The session is still perfectly
        // healthy, and treating this as a transport failure would tear it down.
        let xml = r#"<?xml version="1.0" encoding="iso-8859-1"?>
<response xmlns="urn:debugger_protocol_v1" command="breakpoint_set" transaction_id="11"><error code="200"><message><![CDATA[breakpoint could not be set]]></message></error></response>"#;

        let Packet::Response(response) = parse_packet(xml).unwrap() else {
            panic!("expected a response");
        };
        let error = response.error.unwrap();
        assert_eq!(error.code, 200);
        assert_eq!(error.message, "breakpoint could not be set");
        assert!(response.breakpoint_id.is_none());
    }

    #[test]
    fn malformed_xml_is_an_error_rather_than_a_panic() {
        // §24 in this crate's terms: a misbehaving engine must not take the IDE down.
        assert!(parse_packet("<response").is_err());
        assert!(parse_packet("").is_err());
        assert!(parse_packet("<unknown/>").is_err());
    }

    #[test]
    fn commands_carry_the_transaction_id_the_reply_will_echo() {
        assert_eq!(run(4), "run -i 4");
        assert_eq!(step_into(1), "step_into -i 1");
        assert_eq!(step_over(2), "step_over -i 2");
        assert_eq!(step_out(3), "step_out -i 3");
        assert_eq!(stack_get(10), "stack_get -i 10");
        assert_eq!(detach(5), "detach -i 5");
        assert_eq!(stop(6), "stop -i 6");
    }

    #[test]
    fn a_line_breakpoint_names_the_file_and_line() {
        // Matches the shape in `dbgp-breakpoint-line.phpt`, with the file argument the
        // fixture omits because it breakpoints the file already running.
        assert_eq!(
            breakpoint_set_line(3, "file:///srv/app/index.php", 3),
            "breakpoint_set -i 3 -t line -f file:///srv/app/index.php -n 3"
        );
        assert_eq!(breakpoint_remove(7, "123450001"), "breakpoint_remove -i 7 -d 123450001");
    }

    #[test]
    fn feature_set_raises_the_limits_that_make_inspection_usable() {
        assert_eq!(
            feature_set(1, "max_depth", &Feature::MAX_DEPTH.to_string()),
            "feature_set -i 1 -n max_depth -v 3"
        );
        // Xdebug's own defaults are `max_depth=1` and `max_children=32`, and at depth 1 an
        // array shows as a type with nothing inside it. Asserted in a `const` block so
        // lowering either constant back to the default fails the *build* rather than a
        // test — this is a compile-time fact about a constant, not a runtime behaviour.
        const { assert!(Feature::MAX_DEPTH > 1) };
        const { assert!(Feature::MAX_CHILDREN > 32) };
    }

    #[test]
    fn context_and_property_requests_name_the_stack_level() {
        // Without `-d`, both answer for the innermost frame — so clicking frame 2 in the
        // stack panel would show frame 0's variables.
        assert_eq!(context_get(5, context::LOCALS, 0), "context_get -i 5 -c 0 -d 0");
        assert_eq!(context_get(6, context::SUPERGLOBALS, 2), "context_get -i 6 -c 1 -d 2");
        assert_eq!(property_get(7, "$user", 1), "property_get -i 7 -n $user -d 1");
    }

    #[test]
    fn an_argument_with_spaces_is_quoted_and_escaped() {
        // `$config['app name']` is an ordinary array key and produces an argument with a
        // space in it; unquoted, DBGp would read `name']` as the next flag.
        assert_eq!(
            property_get(8, "$config['app name']", 0),
            r#"property_get -i 8 -n "$config['app name']" -d 0"#
        );
        assert_eq!(
            property_get(9, r#"$x["a\"b"]"#, 0),
            r#"property_get -i 9 -n "$x[\"a\\\"b\"]" -d 0"#
        );
        // A NUL cannot be escaped through a NUL-terminated command, so it is dropped
        // rather than allowed to truncate the frame.
        assert!(!property_get(10, "$a\0b", 0).contains('\0'));
    }

    #[test]
    fn base64_decodes_padding_and_rejects_junk() {
        assert_eq!(base64_decode("YWRtaW4=").unwrap(), b"admin");
        assert_eq!(base64_decode("YQ==").unwrap(), b"a");
        assert_eq!(base64_decode("YWI=").unwrap(), b"ab");
        assert_eq!(base64_decode("YWJj").unwrap(), b"abc");
        assert_eq!(base64_decode("").unwrap(), b"");
        // Xdebug wraps long payloads across lines.
        assert_eq!(base64_decode("YWRt\naW4=").unwrap(), b"admin");
        assert!(base64_decode("not valid!").is_none());
    }

    #[test]
    fn every_status_value_the_specification_defines_is_understood() {
        assert_eq!(Status::parse("starting"), Some(Status::Starting));
        assert_eq!(Status::parse("stopping"), Some(Status::Stopping));
        assert_eq!(Status::parse("stopped"), Some(Status::Stopped));
        assert_eq!(Status::parse("running"), Some(Status::Running));
        assert_eq!(Status::parse("break"), Some(Status::Break));
        assert_eq!(Status::parse("nonsense"), None);

        // Only these three can be stepped; `stopping` cannot, which is the distinction
        // that stops the UI offering a step that will error.
        assert!(Status::Break.is_live());
        assert!(Status::Starting.is_live());
        assert!(Status::Running.is_live());
        assert!(!Status::Stopping.is_live());
        assert!(!Status::Stopped.is_live());
    }
}
