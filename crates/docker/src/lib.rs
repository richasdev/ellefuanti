//! What Docker looks like from this project, read-only (#25).
//!
//! **Not a Docker Desktop replacement**, by construction: this crate answers two
//! questions — "does this project use Docker?" (a handful of `is_file` calls) and
//! "which compose services are running?" (two `docker compose` invocations parsed as
//! plain lines, no JSON dependency). Everything with an effect — up, stop, logs, a
//! shell — is *typed into the terminal* by the app, the #146 artisan rule: nothing
//! runs that was not visibly on the prompt line.
//!
//! A broken daemon cannot break anything here: every failure is an `Err` with the
//! CLI's own words, the caller shows it as a line of text, and nothing retries.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The Docker-shaped files a project declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DockerProject {
    pub dockerfile: bool,
    /// The compose file, first match of the names `docker compose` itself accepts.
    pub compose: Option<PathBuf>,
}

/// Detects Docker usage — `None` when the project has neither file, so the caller can
/// say "not a Docker project" instead of showing an empty panel that looks broken.
pub fn detect(root: &Path) -> Option<DockerProject> {
    let dockerfile = root.join("Dockerfile").is_file();
    let compose = ["compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml"]
        .iter()
        .map(|name| root.join(name))
        .find(|path| path.is_file());
    (dockerfile || compose.is_some()).then_some(DockerProject { dockerfile, compose })
}

/// The compose services and whether each is running: two plain-text CLI calls
/// (`--services`, then `--services --status running`), merged by [`services_from`].
pub fn services(root: &Path) -> Result<Vec<(String, bool)>> {
    let all = compose_output(root, &["ps", "--services", "--all"])?;
    let running = compose_output(root, &["ps", "--services", "--status", "running"])?;
    Ok(services_from(&all, &running))
}

/// Pure merge of the two line lists — the testable half, since a test machine is not
/// guaranteed a daemon the way it is guaranteed git.
pub fn services_from(all: &str, running: &str) -> Vec<(String, bool)> {
    let running: std::collections::HashSet<&str> =
        running.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    all.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|name| (name.to_string(), running.contains(name)))
        .collect()
}

fn compose_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("docker")
        .arg("compose")
        .args(args)
        .current_dir(root)
        .output()
        .context("could not run docker — is it installed?")?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        // The daemon being down arrives here; its own words are the honest message.
        bail!("{}", if stderr.trim().is_empty() { stdout } else { stderr });
    }
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_needs_a_docker_shaped_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect(dir.path()), None, "no files, no claim");

        std::fs::write(dir.path().join("docker-compose.yml"), "services: {}\n").unwrap();
        let project = detect(dir.path()).expect("compose file found");
        assert!(!project.dockerfile);
        assert!(project.compose.unwrap().ends_with("docker-compose.yml"));

        std::fs::write(dir.path().join("compose.yaml"), "services: {}\n").unwrap();
        let project = detect(dir.path()).unwrap();
        assert!(
            project.compose.unwrap().ends_with("compose.yaml"),
            "the CLI's own precedence order: compose.yaml first"
        );
    }

    #[test]
    fn the_service_merge_marks_running_and_keeps_order() {
        let merged = services_from("app\ndb\nredis\n", "db\n");
        assert_eq!(
            merged,
            [("app".to_string(), false), ("db".to_string(), true), ("redis".to_string(), false)]
        );
        assert!(services_from("", "").is_empty());
    }
}
