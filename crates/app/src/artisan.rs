//! Artisan through the command palette (#23): list the project's own commands, and type
//! the chosen one into the terminal — never execute it out of sight.
//!
//! The honesty rule shapes both halves. The list comes from `php artisan list --raw` run
//! against *this* project, so a package-registered command appears and a command this
//! Laravel version does not have does not — a curated built-in list would be a claim
//! about someone else's project. And confirming a row only *types* `php artisan <name> `
//! into the shell, no newline: the user sees the real command, completes its arguments,
//! and presses Enter themselves. Nothing runs that was not visibly on the prompt line.

use std::path::Path;

/// One palette row: `(name, description)` as artisan itself reports them.
///
/// `--raw` prints `name` and description separated by a run of spaces, one per line,
/// no ANSI, no sections. A line with no description is a name alone.
pub fn parse_raw_list(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with(char::is_whitespace) {
                return None;
            }
            match line.split_once("  ") {
                Some((name, description)) => {
                    Some((name.to_string(), description.trim().to_string()))
                }
                None => Some((line.to_string(), String::new())),
            }
        })
        .collect()
}

/// The line typed into the shell for a chosen command — trailing space, **no newline**.
/// The missing newline is the design: arguments are the user's to add, and Enter is the
/// user's to press.
pub fn command_line(name: &str) -> String {
    format!("php artisan {name} ")
}

/// Runs `php artisan list --raw` at `root` and parses it, or `None` when this is not a
/// Laravel project (no `artisan` file), php is not findable, or artisan itself failed.
///
/// Blocking — the caller wraps it in `cx.background_spawn`. A failure is silence rather
/// than a message because the palette is already open showing an empty list; the states
/// are indistinguishable to the user and both mean "artisan did not answer".
pub fn list(root: &Path) -> Option<Vec<(String, String)>> {
    if !root.join("artisan").is_file() {
        return None;
    }
    let php = crate::lsp_session::resolve_binary("php", &crate::lsp_session::search_dirs())?;
    let output = std::process::Command::new(php)
        .args(["artisan", "list", "--raw", "--no-ansi"])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_raw_list(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_raw_list_parses_to_name_and_description() {
        let output = "about                Display basic information about your application\n\
                      clear-compiled       Remove the compiled class file\n\
                      make:model           Create a new Eloquent model class\n";
        let commands = parse_raw_list(output);
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0].0, "about");
        assert_eq!(commands[0].1, "Display basic information about your application");
        assert_eq!(commands[2].0, "make:model");
    }

    #[test]
    fn a_bare_name_and_blank_lines_survive() {
        let commands = parse_raw_list("db\n\n   indented continuation is not a command\n");
        assert_eq!(commands, [("db".to_string(), String::new())]);
    }

    #[test]
    fn the_typed_line_ends_with_a_space_and_no_newline() {
        // The missing newline is the whole design: nothing executes that the user did
        // not visibly finish and press Enter on.
        assert_eq!(command_line("make:model"), "php artisan make:model ");
        assert!(!command_line("migrate").contains('\n'));
    }
}
