//! Prints status and the first file's diff for a folder, so the crate can be pointed at a
//! real repository — and at a folder that is not one — without launching the editor.
//!
//! `cargo run -p elle-git --example probe -- <path>`

fn main() {
    let Some(root) = std::env::args().nth(1) else {
        eprintln!("usage: probe <path>");
        std::process::exit(2);
    };
    let root = std::path::PathBuf::from(root);
    let never = || false;

    match elle_git::status(&root, &never) {
        None => println!("NOT A REPO: {}", root.display()),
        Some(status) => {
            println!(
                "branch={:?} cancelled={} files={}",
                status.branch,
                status.cancelled,
                status.files.len()
            );
            for file in status.files.iter().take(10) {
                println!("  {} {} staged={}", file.status.marker(), file.relative, file.staged);
            }
            if let Some(first) = status.files.first() {
                match elle_git::diff_file(&root, &first.path) {
                    Some(diff) => {
                        let (added, removed) = diff.counts();
                        println!(
                            "  diff {} +{added} -{removed} hunks={} binary={}",
                            diff.relative,
                            diff.hunks.len(),
                            diff.binary
                        );
                    }
                    None => println!("  no diff for {}", first.relative),
                }
            }
        }
    }
}
