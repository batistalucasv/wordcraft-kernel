# PR 4 Blueprint — L2 `crates/layout`

- **Layer:** L2 (`wordcraft-layout`) + minimal 1-line workspace match/struct-literal compile shims
- **Depends on:** PR 2 (`crates/doc`) — can be submitted in parallel with PR 3
- **Suggested branch:** `feat/upstream-pr4-layout-chunks`
- **Commit title / PR title:** `feat(layout): reserve Placed::Chunk blocks for interactive code chunks`
- **Fork provenance:** `wordcraft` PR `#49` (layout portion, cleanly decoupled from L6 UI)

## Exact File Scope (13 files)

Core L2 layout changes (11 files):
- `crates/layout/src/cache.rs`
- `crates/layout/src/display/page.rs`
- `crates/layout/src/lib.rs`
- `crates/layout/src/page.rs`
- `crates/layout/src/pass.rs`
- `crates/layout/src/placed.rs`
- `crates/layout/src/table.rs`
- `crates/layout/src/table_flow.rs`
- `crates/layout/src/tests.rs`
- `crates/layout/src/textbox.rs`
- `crates/layout/src/vertical.rs`

Minimal exhaustive-match / struct-literal compile shims so the workspace compiles cleanly (3 files, 1 line each — note `session/layout.rs` gets full wiring in PR 5):
- `crates/engine/src/cmd/page/fit_page.rs` (`Placed::Chunk { .. } => Some(&StoryRef::Body),`)
- `crates/engine/src/session/layout.rs` (`chunk_heights: Default::default(),` in `LayoutOptions` literal)
- `crates/ui-egui/src/canvas/pages.rs` (`| Placed::Chunk { .. }` arm in `page_content_key`)

## Local Verification Commands

```bash
cargo fmt --all -- --check
cargo clippy -p wordcraft-layout -p wordcraft-engine -p wordcraft-ui-egui --all-targets -- -D warnings
cargo test -p wordcraft-layout
cargo xtask layers
cargo xtask wasm
```

---

## Ready-to-Use PR Description

```markdown
## Summary

Adds `Placed::Chunk` and `LayoutOptions::chunk_heights` in `crates/layout` (Layer 2) so interactive UI hosts can reserve measured page-space rectangles for mounted top-level body code chunks while headless, PDF, and Outline-view layouts continue laying out chunk paragraphs normally.

- **Layout reservation (`crates/layout/src/placed.rs`, `lib.rs`, `pass.rs`, `cache.rs`):**
  - Introduces `Placed::Chunk { id, rect, first, last }` and `LayoutOptions::chunk_heights: BTreeMap<i64, u32>` (default empty for headless/PDF rendering).
  - Reserves chunk heights during normal Print/Web layout (honoring `page_break_before`, column balancing, and page breaks), while keeping Outline view and table-cell chunks on ordinary paragraph layout.
- **Page metrics, geometry & navigation (`page.rs`, `display/page.rs`, `table.rs`, `table_flow.rs`, `textbox.rs`, `vertical.rs`):**
  - Accounts for `Placed::Chunk` vertical spans, translation/rotation, table-flow collision checks, and body content bounds.
- **Exhaustive workspace match shims (`crates/engine/src/cmd/page/fit_page.rs`, `crates/engine/src/session/layout.rs`, `crates/ui-egui/src/canvas/pages.rs`):**
  - Updates exhaustive `Placed` matches and `LayoutOptions` construction with minimal one-line arms so the full workspace compiles cleanly before the engine and UI PRs land.

## Tests

- Added layout tests in `crates/layout/src/tests.rs` verifying headless default layout (no `Placed::Chunk`), mounted height reservation and reflow of following paragraphs, page breaks for tall chunks, and exclusion of table-cell chunks from top-level `Placed::Chunk`.

## Verification

<!-- Fill in exact pass results after running locally on the final squashed branch -->
- `cargo fmt --all -- --check`:
- `cargo clippy -p wordcraft-layout -p wordcraft-engine -p wordcraft-ui-egui --all-targets -- -D warnings`:
- `cargo test -p wordcraft-layout`:
- `cargo xtask layers`:
- `cargo xtask wasm`:
```
