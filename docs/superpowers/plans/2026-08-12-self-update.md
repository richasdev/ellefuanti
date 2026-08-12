# Self-Update Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In-app update: check GitHub Releases, one-click download+install, "Restart to update".

**Architecture:** New `crates/app/src/update.rs` holds every pure decision (version compare, release parsing, state machine, shell-step planning) unit-tested headless; `workspace_view.rs` gets one state field, one background check task, one status-bar cell. All network/disk work shells out to macOS-bundled tools (`curl`, `hdiutil`, `xattr`) via `smol::process` on the background executor — zero new dependencies.

**Tech Stack:** Rust, gpui 0.2.2, smol, serde_json (present), macOS CLI tools.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-12-self-update-design.md`.
- Repo for the API: `richasdev/ellefuanti` (the actual remote; the Cargo.toml `repository` field says `uemuradev/` and is stale — do not use it).
- `cargo test` green, clippy clean, commit per task on `feat/self-update`.

---

### Task 1: `update.rs` — pure logic, test-first

**Files:**

- Create: `crates/app/src/update.rs`; register `mod update;` in `main.rs` next to the other mods.

**Interfaces (produces):**

- `pub struct Version(pub u32, pub u32, pub u32)` with `parse(&str) -> Option<Version>` accepting `"0.3.0"` and `"v0.3.0"`, deriving `PartialOrd/Ord/Eq`.
- `pub struct Available { pub version: Version, pub tag: String, pub dmg_url: Option<String>, pub html_url: String }`
- `pub fn parse_latest_release(json: &str) -> Option<Available>` — reads `tag_name`, `html_url`, `assets[].name`/`browser_download_url`, picking the asset whose name ends with `-macos.dmg`.
- `pub fn newer_than_current(a: &Available, current: &str) -> bool`
- `pub enum UpdateState { Idle, Available(Available), Downloading, ReadyToRestart }` with `pub fn status_label(&self) -> Option<String>` (`Available` → `"Update vX.Y.Z ↓"`, `Downloading` → `"Updating…"`, `ReadyToRestart` → `"Restart to update"`, `Idle` → `None`).

- [ ] Tests first (same file, `#[cfg(test)]`): version parse (with/without `v`, garbage → None), ordering, release JSON with two assets picks the dmg, release JSON with no dmg still yields `html_url`, `newer_than_current` true/false/equal, labels per state.
- [ ] Implement minimal code; `cargo test -p ellefuanti update` → PASS. Commit `feat(update): version/release parsing and update state (pure)`.

### Task 2: check on startup + 6 h timer

**Files:** Modify `workspace_view.rs` — field `update: update::UpdateState` (+ `Idle` init), method `check_for_update`, called from `WorkspaceView::new`'s first-render hook (same place the window-activation observer registers) or `new` via `cx.spawn`.

- [ ] `check_for_update(cx)`: `cx.spawn` loop — run `curl -fsSL <api url>` via `smol::process::Command` on the background executor, `parse_latest_release`, if `newer_than_current(&a, env!("CARGO_PKG_VERSION"))` set `self.update = Available(a)` + `cx.notify()`; then `timer(6h)` and repeat. curl failure → stay `Idle`, retry next cycle (offline is not an error worth a toast).
- [ ] `#[cfg(test)] pub fn set_update_state_for_test` + `update_label_for_test` hooks. Commit `feat(update): periodic release check`.

### Task 3: status-bar cell + install + restart

**Files:** Modify `workspace_view.rs` status bar (`render` footer, next to the language cell) and add `start_update_install` / `restart_into_update`.

- [ ] Cell: rendered when `status_label()` is `Some`, id `"status-update"`, accent-colored, clickable. Click on `Available`: if `std::env::current_exe()` starts with `/Applications/ellefuanti.app` **and** `dmg_url` is `Some` → `start_update_install`; else `open <html_url>` in browser. Click on `ReadyToRestart` → `restart_into_update`.
- [ ] `start_update_install`: state `Downloading`; `cx.spawn` + `background_spawn` running the shell pipeline (curl to temp, `hdiutil attach -nobrowse -readonly -mountpoint`, `cp -R` the `.app` to `/Applications/ellefuanti.app.update`, `hdiutil detach`, swap, `xattr -dr com.apple.quarantine`). Each step's stderr goes into the error. Ok → `ReadyToRestart`; Err → toast + back to `Available`.
- [ ] `restart_into_update`: spawn detached `sh -c 'sleep 1; open -n /Applications/ellefuanti.app'`, then `cx.quit()`.
- [ ] Render test: seed `Available` via the test hook, assert the label; seed `ReadyToRestart`, assert. Commit `feat(update): status-bar cell, install pipeline, restart`.

### Final

- [ ] `cargo test --workspace` + clippy clean; CHANGELOG Unreleased entry; PR.
