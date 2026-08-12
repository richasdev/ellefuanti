# UX six-pack: drag & drop, DB panel collapse, active-file indicator, tab reveal, no size limit, tree auto-refresh

Date: 2026-08-12
Status: approved

## Goal

Six independent UX improvements to the workspace shell. No new subsystems except a
filesystem watcher.

## Features

### 1. No file-size limit

Delete the `MAX_FILE_BYTES` (64 MB) check in `crates/workspace/src/fs.rs`. Any file
opens. The user explicitly accepted the freeze/OOM risk of giant files. The separate
512 KB search limit in `crates/app/src/editor/project_search.rs` stays — it guards
search throughput, not file opening.

### 2. Database panel expand-all / collapse-all

The explorer header has an expand-all / collapse-all button pair
(`workspace_view.rs`, `render_tree_header_buttons`). Add the same pair to the
database panel header, driving the existing expanded-tables set
(`workspace_view.rs` schema state, #65): expand all = every table shows columns,
collapse all = clean list of table names.

### 3. Active-file indicator (reveal in tree)

The explorer row whose path equals the active tab's path renders with an "active"
highlight distinct from hover/selection. On tab activation (click, palette, tab
switch), ancestors of the path auto-expand and the row scrolls into view.

### 4. Tab scroll-into-view

Whenever a tab becomes active (including via explorer click), the tab-bar strip
scrolls horizontally so that tab is fully visible.

### 5. Tree auto-refresh (FS watcher)

Add the `notify` crate (FSEvents backend on macOS). Watch the workspace root
recursively; debounce events ~300 ms; on fire, call the existing
`FileTree::refresh()` (already preserves expansion state and hidden-file rules,
already tested) and re-render. Covers creations/deletions/renames from Finder,
terminals, and the app itself. Watcher errors are non-fatal: log and continue with
manual refresh behavior.

### 6. Drag & drop (GPUI native, three scopes)

- **Finder → app**: `on_drop::<ExternalPaths>` on the workspace root element.
  Dropped file → open as tab. Dropped directory → open as new workspace root.
- **Move within tree**: `on_drag` on explorer rows; directory rows (and root
  background) are drop targets. Drop performs `std::fs::rename`, then tree refresh.
  Guards: drop on self or own descendant = no-op; name collision at destination =
  error toast, never overwrite; cross-device rename error surfaces as toast.
- **Tab reorder**: `on_drag` on tabs; dropping on another tab reorders the
  open-tabs vec at that index. Active-tab identity follows the moved tab.

## Non-goals

- No copy-on-drag (only move), no multi-select drag.
- No streaming/lazy loading for giant files (limit removed as-is).
- No watcher-driven git-status refresh (tree only).

## Testing

Rust unit tests where logic is testable headless: fs limit removal, rename guards
(self/descendant/collision), tab reorder indices, debounce coalescing. Render/UI
behavior verified in `render_tests.rs` patterns where they exist.

## Implementation order

1 → 2 → 3 → 4 → 5 → 6 (smallest to largest risk).
