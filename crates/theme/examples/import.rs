//! Imports a VS Code theme and prints it in the native format.
//!
//! How the files in `assets/themes/` were produced, kept in the tree so they can be
//! regenerated rather than hand-maintained:
//!
//! ```sh
//! cargo run -p elle-theme --example import -- \
//!     ~/.vscode/extensions/github.github-vscode-theme-6.3.5/themes/dark-default.json \
//!     github-dark 'github.github-vscode-theme v6.3.5, MIT' > assets/themes/github-dark.json
//! ```
//!
//! An example rather than a subcommand of the app: importing is a thing done once when a
//! theme is added, not a feature of the editor, and a binary nobody ships is cheaper than a
//! UI nobody uses. When #28's plugin system wants themes at runtime it calls
//! `elle_theme::import` directly — the same function this wraps.

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(path), Some(name)) = (args.next(), args.next()) else {
        eprintln!("usage: import <vscode-theme.json> <name> [origin]");
        std::process::exit(2);
    };
    let origin = args.next();

    match elle_theme::import(std::path::Path::new(&path), &name) {
        Ok(mut theme) => {
            theme.origin = origin;
            print!("{}", theme.to_json());
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
