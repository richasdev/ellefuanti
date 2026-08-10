# ADR-0009: JSON settings, global only, unknown keys preserved

**Status:** Accepted · 2026-08-10

## Context

Nothing the user chooses survives a restart. Three issues are blocked on the same missing
piece: the theme switcher works and forgets (#48), fonts are compile-time constants (#49),
and disk themes need somewhere to record which theme is selected (#58). #70 wants a
configurable scrollback depth. Each would otherwise invent its own storage.

Three questions had to be settled before any of them, because all three are painful to
change afterwards: which format, where the file lives, and whether per-project settings
exist.

## Decision

**JSON**, at `~/Library/Application Support/ellefuanti/settings.json`, **global only**, with
unknown keys preserved across a write and a `version` field from the first commit.

## Consequences

**Why JSON over TOML.** TOML is nicer to hand-edit and comments would be worth having. JSON
wins on one thing that outweighs that: #58 wants to import `.vscode/settings.json`, and with
JSON the importer is a key mapping over an already-parsed document rather than a second
parser and a translation layer. `serde_json` is also already a direct dependency
(`elle-lsp`), so this costs nothing in the build.

The real cost is comments — JSON has none, and a settings file a user cannot annotate is
worse than one they can. Accepted rather than solved: JSON5 or JSONC would fix it and would
mean a parser that is not `serde_json`, which gives back the reason for choosing JSON at
all. If the lack of comments turns out to hurt, the fix is documentation of the keys, not a
dialect.

**Why beside the index and not inside it.** #72 put the file index at
`.../ellefuanti/index/`. Both now go through one `elle_settings::support_dir` helper, so the
two cannot drift onto different roots. `settings.json` sits _beside_ `index/` rather than
inside it, because the index is derived data that may be deleted at any time (ADR-0008) and
someone clearing a cache must not lose their configuration with it.

No `XDG_CONFIG_HOME`. ellefuanti is macOS-only (§1), and honouring a variable macOS users
do not set is a branch that can only ever be wrong.

**Global only. There is no `.ellefuanti/settings.json`.** This is a decision, not an
omission. Per-project settings turn every read into a merge with precedence and every write
into a question about which file a key came from, and those rules have to be right for keys
that do not exist yet. Nothing currently configurable is per-project: a theme is a property
of the person, not the repository. When a genuinely per-project key arrives — a PHP binary
path, a test command — that is the issue in which to pay for the merge, and the read path is
the only thing that has to change because the accessors already go through one document.

**Settings are not a cache, which inverts ADR-0008.** The index deletes what it cannot
understand and rebuilds it. Nothing here may delete anything, because the input is a file a
human typed and there is no source to rebuild it from. Three properties follow:

- **The in-memory value is the parsed document**, a `serde_json::Map`, not a struct. There is
  no step at which an unrecognised key is dropped, because there is no step at which the
  document is projected onto a fixed shape. This is what makes a downgrade safe: an older
  build reads the theme, saves, and every key it has never heard of is still in the file.
- **One bad key costs one key.** A value of the wrong type falls back to that key's default
  and logs. Deserialising into a struct would fail the whole file over a single typo, which
  is the behaviour most editors have and most users hate.
- **A malformed file names the file and the position, and the app launches on defaults —
  read-only.** Never a fatal error, and never a rewrite. The read-only part is the
  non-obvious half: a file that fails to parse loads as _defaults_, so if the session then
  saved normally, the first theme toggle would write a two-key document over everything the
  user had configured. Losing a config file as a side effect of a keystroke, from a state
  the user is in because of one typo, is the worst outcome available here. So an unreadable
  file disables saving for the launch and says so.

**Version from the first commit**, same lesson as ADR-0008 and the opposite recovery. A
version this build does not recognise is a warning, not a discard: known keys are read,
unknown keys are preserved. Adding a key does not bump it — an absent key already resolves
to its default, which is exactly what a new key needs.

**Writes are temp-file-then-rename**, in the destination directory so the rename is
same-filesystem and therefore atomic. A crash mid-write leaves the previous complete file,
not a truncated one that fails to parse on the next launch.

**What this does not include.** No settings UI — a file plus documented keys is the
deliverable, and a GUI is a larger separate piece. No keybinding customisation. And exactly
one key, `theme`, is wired to a real consumer: the point of this change is a layer that
demonstrably works, not a schema of keys nobody reads yet.
