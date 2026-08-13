//! A tiny XML reader, for DBGp packets only.
//!
//! # Why this is hand-written rather than a crate
//!
//! The binary budget. `roxmltree` was the obvious choice and was tried first: it is
//! already in `Cargo.lock`, reached through `gpui -> resvg -> usvg`, so declaring it
//! looked free by the same argument that justifies `regex` and `serde_json` elsewhere.
//!
//! **Measured, it was not.** The release binary went 18.91 MB -> 19.09 MB, past the 19 MB
//! gate. The reasoning was wrong in a way worth recording so nobody repeats it: a crate
//! being *in the dependency graph* does not mean its code is *linked into the binary*.
//! usvg only calls the part of `roxmltree` that SVG needs, and the linker had been
//! dropping the rest; calling the full document API from here brought it all in. The
//! `regex` precedent holds where the existing user already exercises the whole crate —
//! it does not generalise to "anything in the lock file is free".
//!
//! So the parser is here instead, and it is small because DBGp's XML is small. This is
//! **not** a general XML parser and must not be used as one. It handles exactly what
//! Xdebug emits:
//!
//! - elements, possibly self-closing, possibly namespaced (`<xdebug:message/>`)
//! - double-quoted attributes with the five predefined entities
//! - `<![CDATA[...]]>` text, which is how every value arrives
//! - an `<?xml ...?>` declaration, skipped
//!
//! It does **not** handle DTDs, processing instructions beyond the declaration, comments,
//! namespace resolution (the prefix is stripped, which is all the caller wants), or
//! character references beyond the five named ones. Anything it does not understand is a
//! parse error rather than a guess, because a silently mis-parsed debugger packet is
//! worse than a reported one.

use anyhow::{Result, bail};

/// One parsed element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// The local name: the part after any `:` prefix. `<xdebug:message>` is `message`,
    /// which is what every caller matches on.
    pub name: String,
    pub attributes: Vec<(String, String)>,
    /// Text content directly inside this element, CDATA and entities already decoded.
    pub text: Option<String>,
    pub children: Vec<Node>,
}

impl Node {
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|child| child.name == name)
    }

    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }
}

/// Parses one packet into its root element.
pub fn parse(xml: &str) -> Result<Node> {
    let bytes = xml.as_bytes();
    let mut position = 0;

    skip_prologue(bytes, &mut position);
    let node = parse_element(bytes, &mut position)?;
    Ok(node)
}

/// Skips whitespace, the `<?xml ... ?>` declaration and any comments before the root.
fn skip_prologue(bytes: &[u8], position: &mut usize) {
    loop {
        skip_whitespace(bytes, position);
        if bytes[*position..].starts_with(b"<?")
            && let Some(end) = find(bytes, *position, b"?>")
        {
            *position = end + 2;
            continue;
        }
        if bytes[*position..].starts_with(b"<!--")
            && let Some(end) = find(bytes, *position, b"-->")
        {
            *position = end + 3;
            continue;
        }
        return;
    }
}

fn skip_whitespace(bytes: &[u8], position: &mut usize) {
    while *position < bytes.len() && bytes[*position].is_ascii_whitespace() {
        *position += 1;
    }
}

/// Finds `needle` at or after `from`, returning its start.
fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= bytes.len() {
        return None;
    }
    bytes[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

fn parse_element(bytes: &[u8], position: &mut usize) -> Result<Node> {
    skip_whitespace(bytes, position);
    if bytes.get(*position) != Some(&b'<') {
        bail!("expected an element");
    }
    *position += 1;

    let name = read_name(bytes, position)?;
    let mut attributes = Vec::new();

    loop {
        skip_whitespace(bytes, position);
        match bytes.get(*position) {
            // `<name/>`: no children, no text.
            Some(b'/') => {
                if bytes.get(*position + 1) != Some(&b'>') {
                    bail!("malformed self-closing tag");
                }
                *position += 2;
                return Ok(Node { name, attributes, text: None, children: Vec::new() });
            }
            Some(b'>') => {
                *position += 1;
                break;
            }
            Some(_) => {
                let (key, value) = read_attribute(bytes, position)?;
                attributes.push((key, value));
            }
            None => bail!("the document ended inside a tag"),
        }
    }

    // Content: text, CDATA and child elements, until the closing tag.
    let mut children = Vec::new();
    let mut text = String::new();

    loop {
        if *position >= bytes.len() {
            bail!("the document ended before </{name}>");
        }

        if bytes[*position..].starts_with(b"<![CDATA[") {
            *position += 9;
            let Some(end) = find(bytes, *position, b"]]>") else {
                bail!("unterminated CDATA section");
            };
            // CDATA is literal by definition: no entity decoding inside it.
            text.push_str(&String::from_utf8_lossy(&bytes[*position..end]));
            *position = end + 3;
            continue;
        }

        if bytes[*position..].starts_with(b"<!--") {
            let Some(end) = find(bytes, *position, b"-->") else {
                bail!("unterminated comment");
            };
            *position = end + 3;
            continue;
        }

        if bytes[*position..].starts_with(b"</") {
            *position += 2;
            let closing = read_name(bytes, position)?;
            if closing != name {
                bail!("</{closing}> closes an element opened as <{name}>");
            }
            skip_whitespace(bytes, position);
            if bytes.get(*position) != Some(&b'>') {
                bail!("malformed closing tag for <{name}>");
            }
            *position += 1;
            break;
        }

        if bytes[*position] == b'<' {
            children.push(parse_element(bytes, position)?);
            continue;
        }

        // Plain text up to the next tag.
        let start = *position;
        while *position < bytes.len() && bytes[*position] != b'<' {
            *position += 1;
        }
        text.push_str(&decode_entities(&String::from_utf8_lossy(&bytes[start..*position])));
    }

    // Whitespace between child elements is layout, not a value — reporting it as one
    // would give every container an empty string where it has no value at all. An element
    // with no text at all is the same story, so both collapse to the same test: text that
    // is only whitespace *and* has siblings to be whitespace between, or no text at all.
    let text = if text.is_empty() || (text.trim().is_empty() && !children.is_empty()) {
        None
    } else {
        Some(text)
    };

    Ok(Node { name, attributes, text, children })
}

/// Reads a tag or attribute name, dropping any namespace prefix.
fn read_name(bytes: &[u8], position: &mut usize) -> Result<String> {
    let start = *position;
    while *position < bytes.len() {
        let byte = bytes[*position];
        if byte.is_ascii_whitespace() || byte == b'>' || byte == b'/' || byte == b'=' {
            break;
        }
        *position += 1;
    }
    if start == *position {
        bail!("expected a name");
    }
    let full = String::from_utf8_lossy(&bytes[start..*position]).into_owned();
    // `xdebug:message` -> `message`. The caller matches on local names, and resolving
    // namespaces properly would be work in service of a distinction DBGp never makes.
    Ok(match full.split_once(':') {
        Some((_, local)) => local.to_string(),
        None => full,
    })
}

fn read_attribute(bytes: &[u8], position: &mut usize) -> Result<(String, String)> {
    let key = read_name(bytes, position)?;
    skip_whitespace(bytes, position);
    if bytes.get(*position) != Some(&b'=') {
        bail!("attribute {key} has no value");
    }
    *position += 1;
    skip_whitespace(bytes, position);

    let quote = match bytes.get(*position) {
        Some(&b'"') => b'"',
        Some(&b'\'') => b'\'',
        _ => bail!("attribute {key}'s value is not quoted"),
    };
    *position += 1;

    let start = *position;
    while *position < bytes.len() && bytes[*position] != quote {
        *position += 1;
    }
    if *position >= bytes.len() {
        bail!("attribute {key}'s value is unterminated");
    }
    let value = decode_entities(&String::from_utf8_lossy(&bytes[start..*position]));
    *position += 1;
    Ok((key, value))
}

/// Decodes the five predefined entities, and numeric references.
///
/// A class name like `App\Models\User` arrives plain, but `where` attributes carry
/// `-&gt;` for `->` on every single method frame, so this is not an edge case.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        rest = &rest[index..];

        let Some(end) = rest.find(';') else {
            // A bare `&` that is not an entity. Xdebug should escape it, but keeping it is
            // better than dropping the remainder of a variable's value.
            out.push_str(rest);
            return out;
        };

        let entity = &rest[1..end];
        match entity {
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "amp" => out.push('&'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {
                let decoded = entity
                    .strip_prefix("#x")
                    .or_else(|| entity.strip_prefix("#X"))
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .or_else(|| entity.strip_prefix('#').and_then(|n| n.parse().ok()))
                    .and_then(char::from_u32);
                match decoded {
                    Some(character) => out.push(character),
                    // An entity we do not know is passed through as written rather than
                    // silently deleted from a value the user is reading.
                    None => out.push_str(&rest[..=end]),
                }
            }
        }
        rest = &rest[end + 1..];
    }

    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_element_with_attributes() {
        let node = parse(r#"<response command="run" transaction_id="4"/>"#).unwrap();
        assert_eq!(node.name, "response");
        assert_eq!(node.attribute("command"), Some("run"));
        assert_eq!(node.attribute("transaction_id"), Some("4"));
        assert_eq!(node.attribute("absent"), None);
        assert!(node.children.is_empty());
    }

    #[test]
    fn skips_the_xml_declaration_xdebug_always_sends() {
        // Every packet opens with one, and it declares `iso-8859-1`.
        let node =
            parse("<?xml version=\"1.0\" encoding=\"iso-8859-1\"?>\n<response id=\"7\"/>").unwrap();
        assert_eq!(node.name, "response");
        assert_eq!(node.attribute("id"), Some("7"));
    }

    #[test]
    fn strips_the_namespace_prefix_from_names() {
        // `xdebug:message` is how the current position arrives on every step, and the
        // caller matches on `message`.
        let node = parse(r#"<response><xdebug:message lineno="3"/></response>"#).unwrap();
        assert_eq!(node.children[0].name, "message");
        assert_eq!(node.children[0].attribute("lineno"), Some("3"));
    }

    #[test]
    fn namespace_declarations_are_ordinary_attributes() {
        let node = parse(
            r#"<init xmlns="urn:debugger_protocol_v1" xmlns:xdebug="https://xdebug.org/dbgp/xdebug" language="PHP"/>"#,
        )
        .unwrap();
        assert_eq!(node.attribute("language"), Some("PHP"));
    }

    #[test]
    fn reads_cdata_which_is_how_every_value_arrives() {
        let node = parse(r#"<property name="$n"><![CDATA[42]]></property>"#).unwrap();
        assert_eq!(node.text(), Some("42"));
    }

    #[test]
    fn cdata_is_literal_and_entities_inside_it_are_not_decoded() {
        // A PHP string holding `<b>&amp;</b>` must come back exactly as it is.
        let node = parse(r#"<property><![CDATA[<b>&amp;</b>]]></property>"#).unwrap();
        assert_eq!(node.text(), Some("<b>&amp;</b>"));
    }

    #[test]
    fn decodes_entities_in_attributes() {
        // `where="Class->method"` arrives escaped on every method frame in every stack.
        let node = parse(r#"<stack where="App\Models\User-&gt;name" level="0"/>"#).unwrap();
        assert_eq!(node.attribute("where"), Some(r"App\Models\User->name"));
    }

    #[test]
    fn decodes_all_five_predefined_entities_and_numeric_references() {
        assert_eq!(decode_entities("a&lt;b&gt;c&amp;d&quot;e&apos;f"), "a<b>c&d\"e'f");
        assert_eq!(decode_entities("&#65;&#x42;"), "AB");
        // Text with no `&` at all takes the fast path unchanged.
        assert_eq!(decode_entities("plain"), "plain");
        // An unknown entity is passed through rather than deleted from a user's value.
        assert_eq!(decode_entities("&nosuch;"), "&nosuch;");
        assert_eq!(decode_entities("a & b"), "a & b");
    }

    #[test]
    fn nests_children_to_the_depth_xdebug_sends() {
        let node = parse(
            r#"<response><property name="$user" children="1"><property name="roles"><property name="0"><![CDATA[admin]]></property></property></property></response>"#,
        )
        .unwrap();
        let user = &node.children[0];
        assert_eq!(user.attribute("name"), Some("$user"));
        let roles = &user.children[0];
        assert_eq!(roles.attribute("name"), Some("roles"));
        assert_eq!(roles.children[0].text(), Some("admin"));
    }

    #[test]
    fn a_container_has_no_text_of_its_own() {
        // The whitespace between child elements is layout. Reporting it as a value is what
        // would make every array render as an empty string.
        let node = parse("<response>\n  <stack level=\"0\"/>\n  <stack level=\"1\"/>\n</response>")
            .unwrap();
        assert_eq!(node.text(), None);
        assert_eq!(node.children.len(), 2);
    }

    #[test]
    fn an_empty_element_has_no_text() {
        assert_eq!(parse("<property></property>").unwrap().text(), None);
        assert_eq!(parse("<property/>").unwrap().text(), None);
    }

    #[test]
    fn finds_a_named_child() {
        let node = parse(r#"<response><error code="200"><message>no</message></error></response>"#)
            .unwrap();
        let error = node.child("error").unwrap();
        assert_eq!(error.attribute("code"), Some("200"));
        assert_eq!(error.child("message").unwrap().text(), Some("no"));
        assert!(node.child("absent").is_none());
    }

    #[test]
    fn single_quoted_attributes_are_accepted() {
        let node = parse("<response command='run'/>").unwrap();
        assert_eq!(node.attribute("command"), Some("run"));
    }

    #[test]
    fn multibyte_text_survives() {
        // Percent of the point of this product: a PHP string holding Portuguese.
        let node = parse("<property><![CDATA[coração]]></property>").unwrap();
        assert_eq!(node.text(), Some("coração"));
    }

    #[test]
    fn malformed_input_is_an_error_rather_than_a_panic() {
        // §24 in this crate's terms. Every one of these is a way a corrupted stream or a
        // misbehaving engine could arrive, and none may take the IDE down.
        assert!(parse("").is_err());
        assert!(parse("not xml").is_err());
        assert!(parse("<unclosed>").is_err());
        assert!(parse("<a></b>").is_err());
        assert!(parse("<a").is_err());
        assert!(parse("<a attr>").is_err());
        assert!(parse("<a attr=unquoted>").is_err());
        assert!(parse(r#"<a attr="unterminated>"#).is_err());
        assert!(parse("<a><![CDATA[unterminated</a>").is_err());
        assert!(parse("<>").is_err());
        assert!(parse("<a/").is_err());
    }

    #[test]
    fn a_real_xdebug_packet_parses_whole() {
        // Byte-exact from Xdebug's own suite, declaration included.
        let xml = "<?xml version=\"1.0\" encoding=\"iso-8859-1\"?>\n\
            <response xmlns=\"urn:debugger_protocol_v1\" xmlns:xdebug=\"https://xdebug.org/dbgp/xdebug\" \
            command=\"run\" transaction_id=\"4\" status=\"break\" reason=\"ok\">\
            <xdebug:message filename=\"file:///srv/app/index.php\" lineno=\"3\"></xdebug:message>\
            <breakpoint type=\"line\" filename=\"file:///srv/app/index.php\" lineno=\"3\" \
            state=\"enabled\" hit_count=\"1\" hit_value=\"0\" id=\"123450001\"></breakpoint></response>";

        let node = parse(xml).unwrap();
        assert_eq!(node.name, "response");
        assert_eq!(node.attribute("status"), Some("break"));
        let message = node.child("message").unwrap();
        assert_eq!(message.attribute("lineno"), Some("3"));
        assert_eq!(node.child("breakpoint").unwrap().attribute("id"), Some("123450001"));
    }
}
