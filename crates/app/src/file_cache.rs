//! Persisting the quick-open file list, so reopening a project does not re-walk it.
//!
//! The index is a cache, never a source of truth (ADR-0008). Everything here is written so
//! that a missing, corrupt, or version-mismatched database costs a live walk and nothing
//! else: every fallible step collapses to "walk it", and `elle_index::Index::open` already
//! throws away anything it cannot read. Deleting the database file while the editor runs is
//! a supported thing to do.
//!
//! Blocking on purpose — `elle-index` is executor-agnostic (ADR-0007) and the caller wraps
//! this in `cx.background_spawn`.
//!
//! **Why this is worth its cost.** Measured warm-vs-cold on three real projects:
//! crm-livewire-v3 (279 files) 3.56ms → 0.96ms, this repo (128 files) 3.71ms → 0.83ms,
//! filminho (135 files) 3.17ms → 0.91ms. Between 3.5x and 4.5x.
//!
//! The write costs more than the walk it follows (4.7-7.5ms), which sounds like a losing
//! trade and is not: it runs *after* the palette is already filled from that walk, so no
//! one is waiting on it, and it is paid once per change rather than once per open.
//!
//! The verify step is a `stat` per known file and directory, which is O(entries) just like
//! the walk. It wins because `stat` on a path you already have is much cheaper than
//! discovering that path — no directory reads, no `.gitignore` matching. The margin narrows
//! as projects grow: on a synthetic 16k-file tree an earlier probe measured 43ms walking
//! against 28ms warm, only 1.5x. Realistic Laravel projects are hundreds of files, which is
//! where the 3.5-4.5x applies; something genuinely huge degrades toward break-even rather
//! than breaking.

use std::path::{Path, PathBuf};

use elle_index::{Index, Opened};
use elle_workspace::{CancelFlag, IndexedFile, index_files};

/// Where a project's index lives.
///
/// `~/Library/Application Support/ellefuanti/index/<hash>.sqlite`. Outside the project,
/// deliberately: an editor that drops a SQLite file into someone's git repo is a bug, and
/// the index is derived data that has no business being committed or diffed.
///
/// The file name is a hash of the absolute root path rather than the project's name,
/// because two checkouts of the same repo are different projects with the same name and
/// must not share a cache. Collisions would serve one project's file list for another; a
/// 64-bit hash makes that unlikely enough, and the stat-verify pass below would reject
/// nearly every row anyway if it ever happened.
///
/// The directory itself comes from `elle_settings::support_dir` since #60, so the index
/// and settings.json cannot drift onto two different roots. The *location* is still not
/// configurable and there is no key for it: nobody has asked to move their cache, and a
/// setting that exists to be left alone is a setting that goes untested.
pub fn index_path(root: &Path) -> Option<PathBuf> {
    Some(
        elle_settings::support_dir()?
            .join("index")
            .join(format!("{:016x}.sqlite", path_hash(root))),
    )
}

/// Stable 64-bit hash of the root path.
///
/// Hand-rolled FNV-1a rather than `DefaultHasher`: `std`'s hasher is explicitly not
/// guaranteed stable across releases, and a hash that changes under a compiler upgrade
/// silently orphans every cache on disk. Not cryptographic and does not need to be — the
/// only thing it protects against is two projects colliding.
fn path_hash(root: &Path) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

/// mtime and size, the pair that decides whether a cached row is still good.
///
/// A file we cannot stat reports `None` and is treated as gone. mtime is nanoseconds since
/// the epoch; anything before 1970 or unreadable collapses to `None` rather than a
/// sentinel, so "no information" cannot be mistaken for "unchanged".
fn stamp(path: &Path) -> Option<(i64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((directory_mtime(&meta)?, meta.len() as i64))
}

/// mtime in nanoseconds from already-read metadata, so the directory check does not stat
/// twice. Named for its caller; there is nothing directory-specific about it.
fn directory_mtime(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos().try_into().ok()
}

/// Marks a `files` row as describing a directory rather than a file.
///
/// A real file's size is never negative, so this is unambiguous without adding a column —
/// and adding one would mean a schema change in `elle-index`, which this wiring has no
/// business making for a detail only quick open cares about.
///
/// Directories are cached because they are the *only* way to notice a file that was added
/// since the walk. A new file changes no existing file's stamp, so a cache of files alone
/// verifies clean while quick open silently lacks the new file — caught by
/// `a_new_file_is_picked_up_rather_than_hidden_by_the_cache`. Creating or deleting an entry
/// bumps its parent directory's mtime on macOS and Linux, so stamping directories closes
/// the hole for the cost of a few dozen extra rows.
const DIRECTORY_MARKER: i64 = -1;

/// Every directory containing a listed file, including all intermediate ones and the root.
///
/// The root is `""`, which `root.join("")` resolves back to the root itself. Intermediate
/// directories matter as much as leaf ones: a new file at `app/Http/` is invisible in
/// `app/Http/Controllers/`'s mtime, so stamping only the directories that directly hold
/// files would miss it.
///
/// Deduplicated and sorted, so the write order is stable and a project with 300 files in
/// one directory stores one row for it rather than 300.
fn directories_of(files: &[IndexedFile]) -> Vec<String> {
    let mut directories: Vec<String> = files
        .iter()
        .flat_map(|file| {
            // Each prefix of the relative path up to (not including) the file name.
            file.relative
                .match_indices('/')
                .map(|(index, _)| file.relative[..index].to_string())
                .chain(std::iter::once(String::new()))
        })
        .collect();
    directories.sort_unstable();
    directories.dedup();
    directories
}

/// How the file list was obtained. The caller uses this to decide whether to write back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Served from the index, every row stat-verified.
    Cache,
    /// Walked live. Either there was no usable cache or the cache disagreed with disk.
    Walk,
}

/// The quick-open file list for `root`, from the cache when it is provably current and from
/// a live walk otherwise.
///
/// Cancellation is honoured throughout: the walk takes the flag, and the verify loop checks
/// it too, because stat-ing thousands of paths for a palette the user already dismissed is
/// the same waste the flag exists to prevent.
pub fn load(root: &Path, cancel: &CancelFlag) -> (Vec<IndexedFile>, Source) {
    if let Some(files) = read_cached(root, cancel) {
        return (files, Source::Cache);
    }
    (index_files(root, cancel), Source::Walk)
}

/// The cached list, or `None` if there is no usable cache or it disagrees with disk.
///
/// "Disagrees" is deliberately all-or-nothing: one stale row and the whole list is
/// discarded for a walk. Serving a partially-repaired list would mean deciding what to do
/// about files that appeared on disk but are in no row — which a per-row repair cannot see,
/// because you only find new files by walking. Since a mismatch usually means the project
/// changed on disk (a `git checkout`, a `composer install`), the walk is the honest answer
/// and the one that also finds the additions.
fn read_cached(root: &Path, cancel: &CancelFlag) -> Option<Vec<IndexedFile>> {
    let path = index_path(root)?;
    // A cache that does not exist yet must not create one here: writing happens after a
    // successful walk, and `Index::open` would otherwise leave an empty database behind
    // for every project ever opened.
    if !path.exists() {
        return None;
    }

    let (index, opened) = Index::open(&path).ok()?;
    // Rebuilt means the file was unusable and has just been replaced with an empty schema.
    // There is nothing to serve, and reporting empty would be reporting a project with no
    // files in it.
    if opened != Opened::Reused {
        return None;
    }

    let mut stmt = index.connection().prepare("SELECT path, mtime_ns, size FROM files").ok()?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
        })
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()?;

    // An empty cache is not a project with no files; it is a cache that was never written.
    if rows.is_empty() {
        return None;
    }

    let mut files = Vec::with_capacity(rows.len());
    for (relative, mtime, size) in rows {
        if cancel.is_cancelled() {
            return None;
        }
        let absolute = root.join(&relative);

        if size == DIRECTORY_MARKER {
            // A directory whose mtime moved has gained or lost an entry. That entry may be
            // a file no row describes, so the only correct answer is to walk.
            let meta = std::fs::metadata(&absolute).ok()?;
            if !meta.is_dir() {
                return None;
            }
            if directory_mtime(&meta)? != mtime {
                return None;
            }
            // Directories are scaffolding for the freshness check, not quick-open results.
            continue;
        }

        // Any disagreement — changed, deleted, or unreadable — invalidates the whole list.
        if stamp(&absolute)? != (mtime, size) {
            return None;
        }
        let name_offset = relative.rfind('/').map_or(0, |i| i + 1);
        files.push(IndexedFile { path: absolute, relative, name_offset });
    }

    // Every row was a directory: a cache that can prove nothing about files is no cache.
    if files.is_empty() {
        return None;
    }
    Some(files)
}

/// Writes `files` to the project's index, replacing whatever was there.
///
/// Called after a walk, once the palette already has its results — the user is not waiting
/// on this. Failure is silent by design: an unwritable state directory costs a walk next
/// time and nothing more, and there is no action a user could take on being told about it.
///
/// Skipped entirely when the walk was cancelled: a partial list persisted as if complete is
/// exactly the "stale entry served as fresh" failure the stamp check exists to prevent —
/// every row in it would verify clean, and quick open would be missing files until
/// something happened to invalidate it.
pub fn store(root: &Path, files: &[IndexedFile], cancel: &CancelFlag) {
    if cancel.is_cancelled() {
        return;
    }
    let Some(path) = index_path(root) else { return };
    let Ok((index, _)) = Index::open(&path) else { return };

    let write = || -> anyhow::Result<()> {
        let conn = index.connection();
        // One transaction for the lot. Per-row commits would fsync per file and turn a
        // 2ms write into seconds; this is the batching question ADR-0008 left open, and
        // the answer for a list this shape is "per pass".
        let tx = conn.unchecked_transaction()?;
        // The cache is a snapshot of one walk, so stale rows go rather than accumulate.
        // Without this, a file deleted on disk would linger in quick open until a stamp
        // check happened to notice — and it never would, because a deleted file has no row
        // to disagree with.
        tx.execute("DELETE FROM files", [])?;
        for file in files {
            // A file that vanished between the walk and now is simply not cached. Storing
            // a guessed stamp would make it verify clean forever.
            let Some((mtime, size)) = stamp(&file.path) else { continue };
            elle_index::insert_file(&tx, &file.relative, mtime, size)?;
        }

        // Every directory that contains something we listed, so an added file is noticed.
        // The root is included explicitly: a file dropped at the top level has no parent
        // among the relative paths.
        for directory in directories_of(files) {
            let Some(mtime) = std::fs::metadata(root.join(&directory))
                .ok()
                .filter(|meta| meta.is_dir())
                .and_then(|meta| directory_mtime(&meta))
            else {
                continue;
            };
            elle_index::insert_file(&tx, &directory, mtime, DIRECTORY_MARKER)?;
        }
        tx.commit()?;
        Ok(())
    };

    if let Err(err) = write() {
        tracing::debug!(error = %err, "could not persist the quick-open file list");
    }
}

/// `HOME` is process-global, so any test that *sets* it — or resolves an
/// `index_path` on a background task while another test might be setting it — must
/// hold this lock for the duration. It lives outside `mod tests` because the render
/// tests (#22's popup tests) read paths derived from `HOME` across awaits and race
/// the `with_home` tests below without it: popup items arrived empty only under the
/// full parallel suite, which is exactly the class of bug #43 was.
#[cfg(test)]
pub(crate) static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Points HOME at a temp dir so tests never touch the real state directory, and returns
    /// both it and a project root.
    fn fixture() -> (tempfile::TempDir, tempfile::TempDir) {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("app/Models")).unwrap();
        fs::write(project.path().join("app/Models/User.php"), "<?php").unwrap();
        fs::write(project.path().join("artisan"), "#!/usr/bin/env php").unwrap();
        (home, project)
    }

    fn with_home<T>(home: &Path, body: impl FnOnce() -> T) -> T {
        let _guard = super::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os("HOME");
        // SAFETY: guarded by HOME_LOCK, and restored before the guard drops.
        unsafe { std::env::set_var("HOME", home) };
        let out = body();
        match previous {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        out
    }

    fn relatives(files: &[IndexedFile]) -> Vec<String> {
        let mut names: Vec<String> = files.iter().map(|f| f.relative.clone()).collect();
        names.sort();
        names
    }

    #[test]
    fn the_first_open_walks_and_the_second_is_served_from_cache() {
        let (home, project) = fixture();
        with_home(home.path(), || {
            let cancel = CancelFlag::new();

            let (cold, source) = load(project.path(), &cancel);
            assert_eq!(source, Source::Walk, "nothing is cached yet");
            store(project.path(), &cold, &cancel);

            let (warm, source) = load(project.path(), &cancel);
            assert_eq!(source, Source::Cache);
            assert_eq!(relatives(&warm), relatives(&cold), "the cache must agree with the walk");
        });
    }

    /// The whole staleness contract in one test: an edited file must not be served from a
    /// row that describes the old one.
    #[test]
    fn a_changed_file_invalidates_the_cache() {
        let (home, project) = fixture();
        with_home(home.path(), || {
            let cancel = CancelFlag::new();
            let (cold, _) = load(project.path(), &cancel);
            store(project.path(), &cold, &cancel);

            // Same path, different size — the stamp must catch it. mtime alone would too,
            // but filesystems with coarse mtime granularity are exactly why size is stored.
            fs::write(project.path().join("artisan"), "#!/usr/bin/env php\n// changed").unwrap();

            let (_, source) = load(project.path(), &cancel);
            assert_eq!(source, Source::Walk, "a changed file must force a re-walk");
        });
    }

    #[test]
    fn a_deleted_file_invalidates_the_cache() {
        let (home, project) = fixture();
        with_home(home.path(), || {
            let cancel = CancelFlag::new();
            let (cold, _) = load(project.path(), &cancel);
            store(project.path(), &cold, &cancel);

            fs::remove_file(project.path().join("artisan")).unwrap();

            let (files, source) = load(project.path(), &cancel);
            assert_eq!(source, Source::Walk);
            assert!(
                !relatives(&files).contains(&"artisan".to_string()),
                "quick open must not offer a file that is gone"
            );
        });
    }

    /// A new file is the case a per-row stamp check cannot see, which is why a mismatch
    /// discards the whole list rather than repairing it.
    #[test]
    fn a_new_file_is_picked_up_rather_than_hidden_by_the_cache() {
        let (home, project) = fixture();
        with_home(home.path(), || {
            let cancel = CancelFlag::new();
            let (cold, _) = load(project.path(), &cancel);
            store(project.path(), &cold, &cancel);

            fs::write(project.path().join("app/Models/Post.php"), "<?php").unwrap();

            let (files, _) = load(project.path(), &cancel);
            assert!(
                relatives(&files).contains(&"app/Models/Post.php".to_string()),
                "a file added since the walk must appear"
            );
        });
    }

    /// The intermediate-directory case. `app/` holds no files itself, so if only
    /// directories that directly contain files were stamped, a file added at `app/` would
    /// be invisible — no file row changes, and `app/Models/`'s mtime does not move either.
    #[test]
    fn a_new_file_in_an_intermediate_directory_is_picked_up() {
        let (home, project) = fixture();
        with_home(home.path(), || {
            let cancel = CancelFlag::new();
            let (cold, _) = load(project.path(), &cancel);
            store(project.path(), &cold, &cancel);

            fs::write(project.path().join("app/Kernel.php"), "<?php").unwrap();

            let (files, source) = load(project.path(), &cancel);
            assert_eq!(source, Source::Walk, "the parent directory's mtime moved");
            assert!(relatives(&files).contains(&"app/Kernel.php".to_string()));
        });
    }

    /// Directory rows are freshness scaffolding, not results — a directory must never show
    /// up as something the user can open.
    #[test]
    fn directories_are_not_offered_as_quick_open_results() {
        let (home, project) = fixture();
        with_home(home.path(), || {
            let cancel = CancelFlag::new();
            let (cold, _) = load(project.path(), &cancel);
            store(project.path(), &cold, &cancel);

            let (warm, source) = load(project.path(), &cancel);
            assert_eq!(source, Source::Cache);
            assert_eq!(relatives(&warm), relatives(&cold));
            assert!(
                !relatives(&warm).iter().any(|r| r == "app" || r == "app/Models" || r.is_empty()),
                "a directory is not a file: {:?}",
                relatives(&warm)
            );
        });
    }

    /// §24 and ADR-0008: deleting the database at any time must leave the editor working.
    #[test]
    fn deleting_the_database_falls_back_to_a_walk() {
        let (home, project) = fixture();
        with_home(home.path(), || {
            let cancel = CancelFlag::new();
            let (cold, _) = load(project.path(), &cancel);
            store(project.path(), &cold, &cancel);

            fs::remove_file(index_path(project.path()).unwrap()).unwrap();

            let (files, source) = load(project.path(), &cancel);
            assert_eq!(source, Source::Walk);
            assert_eq!(relatives(&files), relatives(&cold), "the walk is the source of truth");
        });
    }

    #[test]
    fn a_corrupt_database_falls_back_to_a_walk() {
        let (home, project) = fixture();
        with_home(home.path(), || {
            let cancel = CancelFlag::new();
            let (cold, _) = load(project.path(), &cancel);
            store(project.path(), &cold, &cancel);

            let db = index_path(project.path()).unwrap();
            fs::write(&db, "this is not a SQLite database").unwrap();

            let (files, source) = load(project.path(), &cancel);
            assert_eq!(source, Source::Walk, "a corrupt cache must not fail the open");
            assert_eq!(relatives(&files), relatives(&cold));
        });
    }

    /// The version-mismatch path. `Index::open` rebuilds, which leaves an empty database —
    /// and empty must read as "no cache", not as "a project with no files".
    #[test]
    fn a_version_mismatch_falls_back_to_a_walk() {
        let (home, project) = fixture();
        with_home(home.path(), || {
            let cancel = CancelFlag::new();
            let (cold, _) = load(project.path(), &cancel);
            store(project.path(), &cold, &cancel);

            let db = index_path(project.path()).unwrap();
            let (index, _) = Index::open(&db).unwrap();
            index
                .connection()
                .execute("UPDATE schema_version SET version = version + 1", [])
                .unwrap();
            drop(index);

            let (files, source) = load(project.path(), &cancel);
            assert_eq!(source, Source::Walk);
            assert_eq!(relatives(&files), relatives(&cold));
        });
    }

    /// A cancelled walk returns a partial list. Persisting it would produce a cache whose
    /// every row verifies clean while files are missing from quick open — stale served as
    /// fresh, with nothing to ever invalidate it.
    #[test]
    fn a_cancelled_walk_is_never_persisted() {
        let (home, project) = fixture();
        with_home(home.path(), || {
            let cancel = CancelFlag::new();
            let (cold, _) = load(project.path(), &cancel);

            cancel.cancel();
            store(project.path(), &cold, &cancel);

            let fresh = CancelFlag::new();
            let (_, source) = load(project.path(), &fresh);
            assert_eq!(source, Source::Walk, "nothing should have been written");
        });
    }

    #[test]
    fn two_projects_do_not_share_a_cache() {
        let home = tempfile::tempdir().unwrap();
        let one = tempfile::tempdir().unwrap();
        let two = tempfile::tempdir().unwrap();
        fs::write(one.path().join("one.php"), "<?php").unwrap();
        fs::write(two.path().join("two.php"), "<?php").unwrap();

        with_home(home.path(), || {
            let cancel = CancelFlag::new();
            let (first, _) = load(one.path(), &cancel);
            store(one.path(), &first, &cancel);
            let (second, _) = load(two.path(), &cancel);
            store(two.path(), &second, &cancel);

            assert_ne!(index_path(one.path()), index_path(two.path()));
            let (warm, source) = load(two.path(), &cancel);
            assert_eq!(source, Source::Cache);
            assert_eq!(relatives(&warm), vec!["two.php".to_string()]);
        });
    }

    /// The cache lives in the state directory, never in the project. An editor that writes
    /// into a git repo is a bug, so this is asserted rather than left to review.
    #[test]
    fn nothing_is_written_into_the_project_directory() {
        let (home, project) = fixture();
        with_home(home.path(), || {
            let cancel = CancelFlag::new();
            let (cold, _) = load(project.path(), &cancel);
            store(project.path(), &cold, &cancel);

            let (files, _) = load(project.path(), &cancel);
            assert_eq!(relatives(&files), relatives(&cold), "no new files in the project");

            let db = index_path(project.path()).unwrap();
            assert!(db.exists(), "the cache should exist somewhere");
            assert!(
                !db.starts_with(project.path()),
                "the index must not live inside the project: {}",
                db.display()
            );
        });
    }

    /// Cancellation must stop the verify loop too, not just the walk.
    #[test]
    fn a_cancelled_load_does_not_serve_the_cache() {
        let (home, project) = fixture();
        with_home(home.path(), || {
            let cancel = CancelFlag::new();
            let (cold, _) = load(project.path(), &cancel);
            store(project.path(), &cold, &cancel);

            cancel.cancel();
            let (_, source) = load(project.path(), &cancel);
            assert_eq!(source, Source::Walk, "a cancelled verify must not report a cache hit");
        });
    }

    /// The hash has to be stable across runs or every cache orphans itself.
    #[test]
    fn the_same_root_hashes_the_same_way_every_time() {
        let root = Path::new("/Users/someone/projects/laravel-app");
        assert_eq!(path_hash(root), path_hash(root));
        assert_ne!(path_hash(root), path_hash(Path::new("/Users/someone/projects/other-app")));
    }
}
