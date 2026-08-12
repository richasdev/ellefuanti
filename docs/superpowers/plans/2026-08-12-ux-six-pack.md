# UX Six-Pack Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Six approved UX features: no file-size limit, DB panel expand/collapse-all, active-file highlight in tree, tab scroll-into-view, tree auto-refresh via FS watcher, drag & drop (Finder→app, tree move, tab reorder).

**Architecture:** All UI work lives in `crates/app/src/workspace_view.rs` (the workspace shell); file logic in `crates/workspace/src/{fs.rs,file_tree.rs}`. The watcher is the only new dependency (`notify`). Drag & drop uses gpui 0.2.2's native `on_drag`/`on_drop`/`ExternalPaths` (verified present in `~/.cargo/registry/.../gpui-0.2.2/src/elements/div.rs:462,499` and `interactive.rs:497`).

**Tech Stack:** Rust, gpui 0.2.2, smol, notify (new).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-12-ux-six-pack-design.md`.
- Follow the codebase's comment style: comments explain _why_, reference issue numbers where relevant.
- `cargo clippy` must stay clean; `cargo test` green after every task.
- Never overwrite files on drag-move; errors surface via `self.status` toast, never panic.
- Commit after each task on branch `feat/ux-six-pack`.

---

### Task 1: Remove the 64 MB file-size limit

**Files:**

- Modify: `crates/workspace/src/fs.rs:18-42` (delete `MAX_FILE_BYTES` and its check)

**Interfaces:**

- Produces: `read_file` no longer errors on size. `MAX_FILE_BYTES` symbol deleted — grep the repo for other references first (`crates/app/src/editor/project_search.rs` has its _own_ separate 512 KB const; leave it).

- [ ] **Step 1: Grep for users of the const**

Run: `grep -rn "workspace.*MAX_FILE_BYTES\|fs::MAX_FILE_BYTES" crates/` — fix any import that breaks. Also check `crates/workspace/src/fs.rs` tests and `render_tests.rs` for a "limit"/"too large" test that asserts the refusal; delete that assertion/test.

- [ ] **Step 2: Delete the check**

In `fs.rs` remove the `MAX_FILE_BYTES` const (lines 18-23) and the `if meta.len() > MAX_FILE_BYTES { bail!(...) }` block (lines 35-42). Keep the binary-file NUL check and the empty-file guard (`bytes[..bytes.len().min(8192)]` — note: `fs::read` of a 0-byte file gives an empty slice; the existing indexing is already safe).

- [ ] **Step 3: Test + commit**

Run: `cargo test -p elle-workspace` → PASS. `cargo clippy -p elle-workspace` → clean.
Commit: `feat(fs): remove the 64MB open limit — any file opens (owner request)`

---

### Task 2: DB panel expand-all / collapse-all

**Files:**

- Modify: `crates/app/src/workspace_view.rs` — header `.when(...)` at ~7703, new methods near `expand_all_tree` (~1194), new render fn near `render_explorer_header_buttons` (~7793)

**Interfaces:**

- Consumes: `db_schema: Option<Result<Vec<elle_db::TableInfo>, String>>` (field ~609), `db_expanded: HashSet<String>` (~616), `db_expanded_for_test` (~1381).
- Produces: `fn expand_all_db(&mut self, cx)`, `fn collapse_all_db(&mut self, cx)`.

- [ ] **Step 1: Methods**

```rust
/// The database header's "expand all": every table shows its columns (#65's
/// bulk counterpart — mirrors the explorer's pair).
fn expand_all_db(&mut self, cx: &mut Context<Self>) {
    if let Some(Ok(tables)) = self.db_schema.as_ref() {
        self.db_expanded = tables.iter().map(|t| t.name.clone()).collect();
        cx.notify();
    }
}

fn collapse_all_db(&mut self, cx: &mut Context<Self>) {
    self.db_expanded.clear();
    cx.notify();
}
```

(Check `elle_db::TableInfo`'s field name for the table name — `grep -n "pub struct TableInfo" crates/db/src/` — and use whatever `render_db_panel` at ~7505 uses to build `name`.)

- [ ] **Step 2: Header buttons**

Add `render_db_header_buttons` cloned from `render_explorer_header_buttons` (~7793) minus the reveal button, calling the two new methods. In the header at ~7703, add below the explorer `.when(...)`:

```rust
.when(
    self.sidebar == Sidebar::Database
        && matches!(self.db_schema, Some(Ok(ref t)) if !t.is_empty()),
    |el| el.child(self.render_db_header_buttons(theme, cx)),
)
```

- [ ] **Step 3: Test**

Add a render test (follow the existing `db_expanded_for_test` usages in `render_tests.rs` for setup): after loading a schema with two tables, call the expand-all handler via a `#[cfg(test)] pub fn expand_all_db_for_test` wrapper; assert `db_expanded_for_test("users")` true for both; collapse-all → false.

- [ ] **Step 4: Run + commit**

`cargo test -p elle-app db_` → PASS. Commit: `feat(db): expand-all/collapse-all buttons on the database panel header`

---

### Task 3: Active-file highlight in the tree

**Files:**

- Modify: `crates/app/src/workspace_view.rs:7861-7935` (`render_tree_rows`)

**Interfaces:**

- Consumes: `self.tabs.get(self.active_tab).and_then(|t| t.path.clone())` (the pattern at ~1209).

- [ ] **Step 1: Highlight**

In `render_tree_rows` before the `uniform_list` closure, capture:

```rust
let active_path = self.tabs.get(self.active_tab).and_then(|tab| tab.path.clone());
let active_bg = theme.pressed; // persistent row tint; check theme.rs for a dedicated selection color first
```

Inside the row builder, after `.active(...)` add:

```rust
.when(active_path.as_deref() == Some(path.as_path()), |el| {
    el.bg(active_bg).text_color(text)
})
```

`active_path` must be cloned into the closure (it is `move`).

- [ ] **Step 2: Verify + commit**

`cargo test -p elle-app` → PASS (render tests catch regressions). Manual: open two files, switch tabs, watch the tree row tint follow. Commit: `feat(tree): highlight the active file's row (owner request)`

---

### Task 4: Tab scroll-into-view

**Files:**

- Modify: `crates/app/src/workspace_view.rs` — new field near `tree_scroll` (~498), init ~703, tab strip `render_tab_bar` (~8056), every `active_tab =` assignment site (~1924, ~2020, ~2063, ~2101, ~2377, tab click ~8120, plus any ⌘⇧[ / next-prev-tab handlers — grep `active_tab =`)

**Interfaces:**

- Produces: `tab_scroll: gpui::ScrollHandle` field; helper `fn scroll_active_tab_into_view(&self)`.

- [ ] **Step 1: Field + tracking**

```rust
/// Scrolls the tab strip so the active tab is visible — activating a tab from
/// the tree or palette with 20 tabs open otherwise selects it off-screen.
tab_scroll: gpui::ScrollHandle,
```

Init `tab_scroll: gpui::ScrollHandle::new(),`. In `render_tab_bar` after `.id("tab-strip")` add `.track_scroll(&self.tab_scroll)`. (`ScrollHandle::scroll_to_item(ix)` exists — gpui-0.2.2 `div.rs:3141` — and works on a tracked `overflow_x_scroll` container's flex children.)

- [ ] **Step 2: Helper + call sites**

```rust
fn scroll_active_tab_into_view(&self) {
    self.tab_scroll.scroll_to_item(self.active_tab);
}
```

Grep `active_tab = ` and call the helper after each assignment (including new-tab pushes and `active_after_close`). In the tab-click closure (~8118) call it inside the same `entity.update`.

- [ ] **Step 3: Verify + commit**

`cargo test -p elle-app` → PASS. Manual: open 15+ files, click a file in the tree whose tab is off-strip — strip scrolls to it. Commit: `feat(tabs): scroll the active tab into view on activation`

---

### Task 5: Tree auto-refresh (FS watcher)

**Files:**

- Modify: `crates/app/Cargo.toml` + root `Cargo.toml` workspace deps (add `notify = "8"`)
- Modify: `crates/app/src/workspace_view.rs` — new field, start-watcher fn, called where a folder becomes the root (find the point where `self.tree = Some(...)` happens on open — grep `self.tree = Some`; there is also `open_folder_for_test` ~1630)

**Interfaces:**

- Consumes: `self.refresh_tree(cx)` (~5474) — already re-reads preserving expansion.
- Produces: `fn start_tree_watcher(&mut self, root: PathBuf, cx: &mut Context<Self>)`, field `tree_watcher: Option<notify::RecommendedWatcher>` + held `gpui::Task`.

- [ ] **Step 1: Wire the watcher**

```rust
/// Watches the workspace root so the tree follows Finder/terminal/mkdir without a
/// manual refresh (owner request). The watcher thread pushes into a channel; a
/// foreground task debounces 300 ms and calls the existing refresh. `.git` churn
/// is filtered or every `git status` would repaint the tree.
tree_watcher: Option<(notify::RecommendedWatcher, gpui::Task<()>)>,
```

```rust
fn start_tree_watcher(&mut self, root: std::path::PathBuf, cx: &mut Context<Self>) {
    use notify::Watcher as _;
    let (tx, rx) = smol::channel::unbounded::<()>();
    let git_dir = root.join(".git");
    let mut watcher = match notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        if event.paths.iter().all(|p| p.starts_with(&git_dir)) {
            return;
        }
        let _ = tx.try_send(());
    }) {
        Ok(w) => w,
        Err(err) => {
            log::warn!("tree watcher unavailable: {err}");
            return; // non-fatal: manual refresh still works
        }
    };
    if let Err(err) = watcher.watch(&root, notify::RecursiveMode::Recursive) {
        log::warn!("tree watcher failed on {}: {err}", root.display());
        return;
    }
    let task = cx.spawn(async move |this, cx| {
        while rx.recv().await.is_ok() {
            smol::Timer::after(std::time::Duration::from_millis(300)).await;
            while rx.try_recv().is_ok() {} // coalesce the burst
            if this.update(cx, |this, cx| this.refresh_tree(cx)).is_err() {
                break;
            }
        }
    });
    self.tree_watcher = Some((watcher, task));
}
```

(Match the crate's actual logging facility — grep `log::` in `main.rs`; if absent, drop to `eprintln!` or the `self.status` pattern. Match `cx.spawn` closure signature to neighbors at ~775.)

- [ ] **Step 2: Start it wherever the root is set**

Grep `self.tree = Some(`; call `self.start_tree_watcher(root.clone(), cx)` at each folder-open site (including `open_folder_for_test` so tests can exercise it). Replacing the root drops the old watcher (`Option` overwrite), which un-watches.

- [ ] **Step 3: Test**

In `render_tests.rs` style: open a temp-dir folder, `std::fs::create_dir(root.join("nova"))`, pump the executor past the debounce (`cx.executor().advance_clock` / `run_until_parked` — follow the async patterns already in the file), assert the tree contains `nova`. If the harness can't drive the real watcher deterministically, test the debounce task path by sending on the channel directly and keep the watcher itself as manual verification.

- [ ] **Step 4: Run + commit**

`cargo test -p elle-app` → PASS. Manual: `mkdir` in Finder and in-app; tree updates alone. Commit: `feat(tree): auto-refresh via FS watcher (notify) — Finder and in-app changes appear alone`

---

### Task 6: Drag & drop — Finder→app, tree move, tab reorder

**Files:**

- Modify: `crates/app/src/workspace_view.rs` — root container in `render` (grep `fn render` impl for `WorkspaceView`), `render_tree_rows` (~7906), `render_tab_bar` (~8089)
- Test: pure helpers unit-tested in the same file's `#[cfg(test)] mod`

**Interfaces:**

- Produces: `struct DraggedTreeEntry { path: PathBuf, is_dir: bool }`, `struct DraggedTab { index: usize }`, `struct DragLabel(SharedString)` (Render impl for the preview), pure `fn reorder_tabs_active(from: usize, to: usize, active: usize) -> usize` (returns new active), pure `fn move_entry(source: &Path, dest_dir: &Path) -> anyhow::Result<PathBuf>` in `crates/workspace/src/fs.rs`.

- [ ] **Step 1: Pure helpers + failing tests first**

In `crates/workspace/src/fs.rs`:

```rust
/// Moves a file or directory into `dest_dir` by rename. Refuses the moves that
/// destroy data or loop: onto itself, into its own subtree, onto an existing name.
pub fn move_entry(source: &Path, dest_dir: &Path) -> Result<PathBuf> {
    let name = source
        .file_name()
        .with_context(|| format!("{} has no file name", source.display()))?;
    if dest_dir == source {
        bail!("cannot move a folder into itself");
    }
    if dest_dir.starts_with(source) {
        bail!("cannot move a folder into its own subtree");
    }
    let dest = dest_dir.join(name);
    if dest == source {
        return Ok(dest); // dropped where it already lives — honest no-op
    }
    if dest.exists() {
        bail!("{} already exists", dest.display());
    }
    fs::rename(source, &dest)
        .with_context(|| format!("moving {} to {}", source.display(), dest.display()))?;
    Ok(dest)
}
```

Tests (tempdir, same style as `file_tree.rs` tests): move file into subdir → moved; move dir into own child → err; collision → err + source untouched; drop into current parent → Ok no-op.

In `workspace_view.rs` (or a small helper mod):

```rust
/// Where the active index lands after moving a tab from `from` to `to`.
fn reorder_tabs_active(from: usize, to: usize, active: usize) -> usize {
    if active == from {
        to
    } else if from < active && to >= active {
        active - 1
    } else if from > active && to <= active {
        active + 1
    } else {
        active
    }
}
```

Tests: moving the active tab follows it; moving across the active shifts it by one; unrelated move leaves it.

- [ ] **Step 2: Finder → app**

On the outermost workspace container in `render` (the div wrapping everything):

```rust
.on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| {
    for path in paths.paths() {
        if path.is_dir() {
            this.set_root_folder(path.clone(), window, cx); // whatever open_folder's post-dialog path is named
        } else {
            this.open_path(path.clone(), window, cx);
        }
    }
}))
```

(Check `ExternalPaths`' accessor — `interactive.rs:497` wraps a `SmallVec<[PathBuf; 2]>`; grep for `pub fn paths`. Check whether `on_drop` here is the `InteractiveElement` method needing an `.id(...)` on that div. Reuse the exact folder-open routine `open_folder` (~765) calls after its dialog.)

- [ ] **Step 3: Tree move**

Row builder (~7906): give every row `.on_drag(DraggedTreeEntry { path: path.clone(), is_dir }, |entry, _, _, cx| cx.new(|_| DragLabel(entry.path.file_name()...into())))`. Directory rows and the root background additionally:

```rust
.drag_over::<DraggedTreeEntry>(|el, _, _, _| el.bg(hover))
.on_drop({ let entity = entity.clone(); let dest = path.clone(); move |dragged: &DraggedTreeEntry, _window, cx| {
    entity.update(cx, |this, cx| this.drop_tree_entry(dragged.path.clone(), dest.clone(), cx));
}})
```

`drop_tree_entry` calls `elle_workspace::fs::move_entry(&source, &dest_dir)`; on `Err` set `self.status`; on `Ok(new_path)` rewrite any open tab whose `path` starts with `source` to the new prefix (so dirty buffers keep saving to the right place), then `self.refresh_tree(cx)`. Root background drop target = move to root. `DragLabel` is a minimal `Render` impl: one padded, themed div showing the file name (copy styling from the tooltip).

- [ ] **Step 4: Tab reorder**

Each tab (~8089): `.on_drag(DraggedTab { index }, |tab, _, _, cx| cx.new(|_| DragLabel(title.clone().into())))` and `.on_drop(move |dragged: &DraggedTab, _window, cx| { entity.update(cx, |this, cx| { let tab = this.tabs.remove(dragged.index); this.tabs.insert(index, tab); this.active_tab = reorder_tabs_active(dragged.index, index, this.active_tab); this.scroll_active_tab_into_view(); cx.notify(); }) })` — guard `dragged.index != index` and clamp: the drop-target `index` was captured before the remove, so when `dragged.index < index` insert at `index` (post-remove it is the slot _after_ the hovered tab, which is the natural "drop to the right" feel); verify against the unit tests.

- [ ] **Step 5: Run everything + commit**

`cargo test --workspace` → PASS, `cargo clippy --workspace` → clean. Manual sweep: drag file from Finder (opens), drag folder from Finder (workspace switches), drag file onto folder in tree (moves, tab survives, tree refreshes via Task 5 watcher too), drag tab across strip (reorders, active follows).
Commit: `feat(dnd): drag & drop — Finder opens, tree moves files, tabs reorder`

---

## Final verification

- [ ] `cargo test --workspace` green, `cargo clippy --workspace` clean, `cargo build --release` builds.
- [ ] Manual pass of all six features in the running app (`cargo run`).
- [ ] Update CHANGELOG.md under Unreleased with the six lines.
