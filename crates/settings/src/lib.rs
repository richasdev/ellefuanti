//! User settings: one JSON file, read at startup, written atomically.
//!
//! Blocking on purpose, like every other infrastructure crate here — this one does not
//! know which executor runs it and the caller wraps it in `cx.background_spawn`
//! (ADR-0007). Plain Rust, no gpui (ADR-0004): the settings file stores a theme *name*,
//! and mapping that name to a `ThemeVariant` is the app crate's job.
//!
//! **Settings are not a cache.** `elle-index` may delete a database it does not
//! understand and rebuild it (ADR-0008); nothing here may delete anything. The file is
//! something a human typed, so the recovery path for every failure is "use defaults for
//! this launch and leave the file alone", never "rewrite it into a shape we like".
//!
//! Three properties the rest of the design falls out of:
//!
//! - **Every key has a default.** A missing file and an empty file are both first-run,
//!   and first-run must be the experience someone actually wants.
//! - **Unknown keys survive a write.** The in-memory value *is* the parsed document, and
//!   setters mutate it in place, so a key written by a newer build is still there after an
//!   older build saves. Deserialising into a struct would silently drop it, which is how a
//!   downgrade eats someone's configuration.
//! - **One bad key costs one key.** A value of the wrong type falls back to that key's
//!   default and logs; it does not discard the other twenty keys around it.
//!
//! Only [`Settings::theme`] exists so far. It is deliberately one real consumer rather
//! than a speculative schema: fonts (#49), scrollback (#70) and window geometry each add
//! their accessor pair when the issue that needs them lands.

mod file;
mod paths;

pub use file::{Load, Settings, SettingsError};
pub use paths::{settings_path, support_dir};

/// Bumped when a *readable* older file would be understood wrongly by this build — a key
/// whose meaning changed, not a key that was added.
///
/// From the first commit, same reasoning as `elle-index`'s `SCHEMA_VERSION` (ADR-0008),
/// and with the opposite recovery: the index deletes what it cannot read, and settings may
/// not. So a version this build does not recognise means "read what you understand, warn,
/// and preserve the rest on write" — see [`Load`].
///
/// Adding a key does not bump this. A key absent from an old file already resolves to its
/// default, which is exactly the behaviour a new key needs.
pub const SETTINGS_VERSION: u64 = 1;
