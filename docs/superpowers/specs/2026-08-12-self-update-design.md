# Self-update: check, download, install, "Restart to update"

Date: 2026-08-12
Status: approved (owner chose the full flow over notify-only)

## Goal

A user running the installed app learns a new version exists, updates with one click,
and restarts with another — the VS Code shape. No manual dmg download.

## Flow

1. **Check** — on startup and every 6 h: fetch
   `https://api.github.com/repos/richasdev/ellefuanti/releases/latest` via `curl -fsSL`
   (ships with macOS; no HTTP crate). Parse with the already-present `serde_json`:
   `tag_name` → semver, assets → the one named `*-macos.dmg`. Newer than
   `CARGO_PKG_VERSION` → state `Available`.
2. **Offer** — a status-bar cell appears: `Update v0.3.0 ↓`. Clicking it:
   - If the running executable is inside `/Applications/ellefuanti.app`: start the
     install (state `Downloading`).
   - Otherwise (a `cargo run`, a translocated copy): open the release page in the
     browser — replacing an app we are not running from would update the wrong thing.
   - No dmg asset on the release → same browser fallback.
3. **Install** (background, blocking work off the main thread):
   `curl` the dmg to a temp dir → `hdiutil attach -nobrowse -readonly` → copy the
   `.app` to `/Applications/ellefuanti.app.update` → detach → swap
   (`rm -rf` old, `mv` new into place; macOS keeps the running binary's inode alive) →
   `xattr -dr com.apple.quarantine` on the new app (the unsigned-build "damaged" fix,
   applied by the app the user already trusts). Success → state `ReadyToRestart`.
   Any step failing → status-bar error toast, state back to `Available`.
4. **Restart** — the cell now reads `Restart to update`. Clicking spawns a detached
   `sh -c 'sleep 1; open -n /Applications/ellefuanti.app'` and quits the app.

## States

`Idle → Available{version, dmg_url} → Downloading → ReadyToRestart`, with `Available`
as the failure fallback. One enum on the workspace; the status-bar cell renders from it.

## Non-goals

- No delta updates, no signature verification beyond HTTPS + the GitHub API (the build
  itself is unsigned — quarantine clearing is already the documented install story).
- No Sparkle/framework dependency.
- No Windows/Linux paths (the app is macOS-only today).

## Testing

Pure parts unit-tested headless: semver parse/compare, release-JSON parsing and dmg
asset selection, state→label mapping, newer-than logic. The curl/hdiutil/swap pipeline
and the restart are manual verification — they need a real GitHub release and a real
`/Applications` install.
