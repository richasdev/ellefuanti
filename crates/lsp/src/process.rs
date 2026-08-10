//! Spawning a language server as a subprocess.
//!
//! Isolated in its own module so the rest of the crate talks to pipes rather than to a
//! `Child`. That is what lets the tests drive the whole client over in-memory pipes
//! with no server installed — the mock is not a special case in the client, it is the
//! ordinary path with different streams.

use std::path::Path;
use std::process::{Child, Command, Stdio};

use anyhow::{Context as _, Result, bail};

use crate::config::ServerConfig;

/// A spawned server process, with its pipes handed out separately.
///
/// The pipes are returned *beside* the child rather than stored in it, because the
/// connection takes ownership of the streams while the client keeps the handle in order
/// to kill a server that ignores `exit`. Bundling all three in one struct would force
/// the caller to leave two fields permanently invalid after moving the pipes out.
pub struct ServerProcess {
    pub child: Child,
}

/// The streams a connection reads from and writes to.
pub struct ServerPipes {
    pub stdin: std::process::ChildStdin,
    pub stdout: std::process::ChildStdout,
}

/// Spawns the configured server.
///
/// Errors rather than panics when the binary is missing (§24): a server that was never
/// installed is a common, entirely recoverable situation — the user gets a message and
/// keeps editing with syntax highlighting only.
pub fn spawn(config: &ServerConfig) -> Result<(ServerProcess, ServerPipes)> {
    if !config.root.is_dir() {
        bail!("project root {} is not a directory", config.root.display());
    }

    let mut command = Command::new(&config.command);
    command
        .args(&config.args)
        .current_dir(&config.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Inherited would interleave the server's chatter into our own terminal; piping
        // and dropping it keeps the log readable. Null rather than piped because
        // nothing reads it, and an unread pipe eventually blocks the writer.
        //
        // ponytail: capture stderr into the log once there is a UI to show server
        // diagnostics in. Until then it is noise with nowhere to go.
        .stderr(Stdio::null());

    for (key, value) in &config.env {
        command.env(key, value);
    }

    let mut child = command.spawn().with_context(|| {
        format!("could not start the {} language server ({})", config.name, config.command)
    })?;

    // `take` rather than `unwrap` on the option each time: both were piped above, so
    // their absence would be a bug in this function, not a runtime condition.
    let stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");

    Ok((ServerProcess { child }, ServerPipes { stdin, stdout }))
}

/// Converts a filesystem path into a `file:` URI.
///
/// Hand-rolled because `lsp-types` 0.97's `Uri` is a plain parser with no
/// path-to-URI constructor, and pulling in the `url` crate for one function is not
/// worth the dependency.
///
/// ponytail: percent-encoding here covers the characters that actually appear in
/// source trees. If a path ever needs the full RFC 3986 rules (non-UTF-8 bytes on
/// Linux, say), swap this for the `percent-encoding` crate rather than extending the
/// match arm by arm.
pub fn path_to_uri(path: &Path) -> Result<lsp_types::Uri> {
    let path = path.to_str().with_context(|| format!("{} is not valid UTF-8", path.display()))?;

    let mut encoded = String::with_capacity(path.len() + 8);
    for byte in path.bytes() {
        match byte {
            // Unreserved per RFC 3986, plus the separators a path legitimately contains.
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                encoded.push(byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }

    // Three slashes: an empty authority, then the absolute path.
    let uri = if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    };

    uri.parse().with_context(|| format!("could not build a file URI for {path}"))
}

/// Converts a `file:` URI back into a filesystem path.
///
/// The inverse of [`path_to_uri`], and needed the moment the server starts *answering* with
/// locations rather than only being told about them — a go-to-definition response names the
/// file it found as a URI, and it has to become a path before anything can be opened.
///
/// Rejects any other scheme. A server may legitimately answer with `jdt:` or `untitled:`,
/// and treating those as paths would produce a file the user does not have; refusing is the
/// honest answer, and the caller shows nothing rather than opening the wrong thing.
pub fn uri_to_path(uri: &lsp_types::Uri) -> Result<std::path::PathBuf> {
    let scheme = uri.scheme().map(|s| s.as_str()).unwrap_or_default();
    if !scheme.eq_ignore_ascii_case("file") {
        anyhow::bail!("{} is not a file: URI", uri.as_str());
    }

    // `as_estr().decode()` reverses the percent-encoding `path_to_uri` applies. Lossy
    // because a path that is not valid UTF-8 cannot round-trip through a `str` anyway —
    // the same limitation `path_to_uri` states.
    let path = uri.path().as_estr().decode().into_string_lossy().to_string();
    if path.is_empty() {
        anyhow::bail!("{} has no path", uri.as_str());
    }
    Ok(std::path::PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The path a URI actually denotes, after percent-decoding.
    fn decoded_path(uri: &lsp_types::Uri) -> String {
        uri.path().as_estr().decode().into_string_lossy().to_string()
    }

    /// `spawn`'s success value holds live OS pipe handles, which have no useful `Debug`
    /// representation; deriving one purely so `unwrap_err` compiles would be tooling
    /// leaking into the design. Matching the error out keeps the types honest.
    fn spawn_error(config: &ServerConfig) -> String {
        match spawn(config) {
            Ok(_) => panic!("expected spawning {} to fail", config.command),
            Err(err) => err.to_string(),
        }
    }

    #[test]
    fn a_missing_binary_is_an_error_not_a_panic() {
        // §24: a server that was never installed must leave the editor working.
        let config = ServerConfig::new("ghost", "definitely-not-a-real-binary-xyzzy", "/tmp");
        let err = spawn_error(&config);
        assert!(err.contains("could not start"), "{err}");
    }

    #[test]
    fn a_missing_root_is_an_error() {
        let config = ServerConfig::new("x", "echo", "/no/such/directory/anywhere");
        assert!(spawn_error(&config).contains("not a directory"));
    }

    #[test]
    fn builds_file_uris() {
        let uri = path_to_uri(&PathBuf::from("/srv/app/routes/web.php")).unwrap();
        assert_eq!(uri.as_str(), "file:///srv/app/routes/web.php");
        assert_eq!(decoded_path(&uri), "/srv/app/routes/web.php");
    }

    #[test]
    fn percent_encodes_spaces_and_accents() {
        // Both appear in real project paths, and an unencoded space produces a URI the
        // server silently fails to match against its own document store.
        let uri = path_to_uri(&PathBuf::from("/srv/minha app/ação.php")).unwrap();
        assert_eq!(uri.as_str(), "file:///srv/minha%20app/a%C3%A7%C3%A3o.php");
        // The encoding must be reversible, or navigation lands on the wrong file.
        assert_eq!(decoded_path(&uri), "/srv/minha app/ação.php");
    }

    #[test]
    fn round_trips_paths_that_stress_the_encoder() {
        for path in [
            "/srv/app/Http/Controllers/UsuárioController.php",
            "/srv/app/a+b/c#d/e?f.php",
            "/srv/app/emoji 😀/x.php",
            "/srv/app/percent%.php",
        ] {
            let uri = path_to_uri(&PathBuf::from(path)).unwrap();
            assert_eq!(decoded_path(&uri), path, "failed to round-trip {path}");
        }
    }

    #[test]
    fn uris_convert_back_into_paths() {
        // The direction navigation needs: a server answers with a URI and it has to name a
        // file we can open. Anything the encoder can produce must survive the return trip,
        // or a definition in a directory with a space in it lands nowhere.
        for path in [
            "/srv/app/routes/web.php",
            "/srv/minha app/ação.php",
            "/srv/app/emoji 😀/x.php",
            "/srv/app/percent%.php",
        ] {
            let uri = path_to_uri(&PathBuf::from(path)).unwrap();
            assert_eq!(uri_to_path(&uri).unwrap(), PathBuf::from(path), "round trip {path}");
        }
    }

    #[test]
    fn a_non_file_uri_is_refused_rather_than_guessed_at() {
        // Servers legitimately answer with locations inside archives or virtual documents.
        // Treating `jdt:/...` as a path would open a file the user does not have, which is
        // worse than showing nothing: it looks like navigation went to the wrong place.
        let uri: lsp_types::Uri = "jdt://contents/rt.jar/java.lang/String.class".parse().unwrap();
        assert!(uri_to_path(&uri).is_err());

        let uri: lsp_types::Uri = "untitled:Untitled-1".parse().unwrap();
        assert!(uri_to_path(&uri).is_err());
    }
}
