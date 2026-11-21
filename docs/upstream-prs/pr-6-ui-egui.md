# PR 6 Blueprint — L6 `crates/ui-egui` (+ `ATTRIBUTION.md`)

- **Layer:** L6 (`wordcraft-ui-egui`) + icon attribution (`ATTRIBUTION.md`)
- **Depends on:** PR 1 (`crates/kernel`), PR 2 (`crates/doc`), PR 4 (`crates/layout`), PR 5 (`crates/engine`)
- **Suggested branch:** `feat/upstream-pr6-ui-code-chunks`
- **Commit title / PR title:** `feat(ui): mount interactive code chunks in document canvas, ribbon, and menus`
- **Fork provenance:** `wordcraft` PRs `#34`, `#35`, `#39`, `#47` (`#44`), `#49`, `#123` (`#98`), `#124` (`#96`), `#125` (`#94`), `#126` (`#97`), `#127` (`#95`), `#128` (`#99`), `#129` (`#72`), `#131` (`#100`)

## Exact File Scope (29 files)

- `ATTRIBUTION.md`
- `crates/ui-egui/Cargo.toml`
- `crates/ui-egui/icons/codeChunk.svg`
- `crates/ui-egui/examples/code_chunk_shot.rs`
- `crates/ui-egui/src/code_chunk.rs`
- `crates/ui-egui/src/code_chunk/tests.rs`
- `crates/ui-egui/src/canvas.rs`
- `crates/ui-egui/src/canvas/caret.rs`
- `crates/ui-egui/src/canvas/code_chunks.rs`
- `crates/ui-egui/src/canvas/context_menu.rs`
- `crates/ui-egui/src/canvas/input.rs`
- `crates/ui-egui/src/canvas/tests.rs`
- `crates/ui-egui/src/canvas/tests/markup.rs`
- `crates/ui-egui/src/app/commands.rs`
- `crates/ui-egui/src/app/files.rs`
- `crates/ui-egui/src/app/frame.rs`
- `crates/ui-egui/src/documents.rs`
- `crates/ui-egui/src/file_dialogs.rs`
- `crates/ui-egui/src/file_dialogs/tests.rs`
- `crates/ui-egui/src/icons.rs`
- `crates/ui-egui/src/keys/canvas.rs`
- `crates/ui-egui/src/lib.rs`
- `crates/ui-egui/src/print_page.rs`
- `crates/ui-egui/src/ribbon/insert.rs`
- `crates/ui-egui/src/save_types.rs`
- `crates/ui-egui/src/tests.rs`
- `crates/ui-egui/src/updates_ui.rs`
- `crates/ui-egui/src/windows.rs`
- `crates/ui-egui/src/windows/tests.rs`

*(Note: `crates/ui-egui/src/canvas/pages.rs` already received its 1-line `Placed::Chunk` match arm in PR 4. Zero files in `apps/` or L2 crates are modified in PR 6.)*

## Local Verification Commands

```bash
cargo fmt --all -- --check
cargo clippy -p wordcraft -p wordcraft-ui-egui --all-targets -- -D warnings
cargo test -p wordcraft -p wordcraft-ui-egui --lib -- code_chunk::tests::
cargo test -p wordcraft -p wordcraft-ui-egui --lib -- canvas::tests::
cargo xtask assets
cargo xtask layers
cargo xtask wasm
```

---

## Ready-to-Use PR Description

```markdown
## Summary

Adds the interactive Layer 6 egui code chunk widget, canvas mounting, ribbon controls, context menu, asynchronous worker pool, and `.qmd`/`.Rmd` file-dialog integration in `crates/ui-egui`.

- **Interactive `CodeChunk` widget (`crates/ui-egui/src/code_chunk.rs`, `examples/code_chunk_shot.rs`):**
  - Rounded code container with language badge (`R` / `Python`), inline label editor, Teaching Mode toggle (`Hide code`), monospace syntax-highlighted editor with line numbers, and collapsible output drawer (`Console`, zoomable `Plot` with multi-plot selector, and previewed `Table` via `egui_extras::TableBuilder`).
  - Borrows encoded plot bytes across repaints (cloning only on `Insert into document`), caches failed PNG decodes, enforces `MAX_CODE_CHARS` and `MAX_OUTPUT_BYTES`, uses monotonic elapsed timing, and explains desktop-only execution in `wasm32` builds.
- **Canvas mounting, focus & caret integration (`crates/ui-egui/src/canvas/code_chunks.rs`, `canvas.rs`, `canvas/caret.rs`, `canvas/input.rs`, `canvas/context_menu.rs`, `keys/canvas.rs`):**
  - Synchronizes mounted widgets with `Placed::Chunk` reservations, debounced `code.update_chunk` flushes, Undo/Redo checkpoint handling, offscreen/unplaced focus reconciliation, Outline-view caret visibility, multi-column trailing-chunk click handling, and arrow-key navigation across chunk boundaries.
  - Runs `code.run_chunk` and `code.run_all` asynchronously with a bounded worker pool (`MAX_CONCURRENT_CHUNK_RUNS`), slot accounting for invalidated workers, `stopOnError` batch cancellation, single-live-run-per-chunk enforcement across multi-window handoff, and accurate `started`/`queued` reporting.
- **Ribbon, file dialogs, multi-window & browser dirty tracking (`ribbon/insert.rs`, `icons.rs`, `icons/codeChunk.svg`, `ATTRIBUTION.md`, `app/*.rs`, `documents.rs`, `windows.rs`, `file_dialogs.rs`, `save_types.rs`):**
  - Adds the Insert-tab Code Chunk menu, right-click context menu, `.qmd`/`.Rmd` open/save filters, cross-window draft reconciliation, and parked-draft dirty notifications.

## Tests

- Unit and `egui_kittest` tests in `crates/ui-egui/src/code_chunk/tests.rs` (29 tests) covering syntax highlighting, output truncation, plot decoding/borrowing/insertion, multi-plot & table tabs, Teaching Mode, browser messaging, and PATH-independent Run-button tests.
- Canvas, window, and dialog regression tests in `crates/ui-egui/src/canvas/tests.rs`, `windows/tests.rs`, `file_dialogs/tests.rs`, and `tests.rs` covering initial `.qmd` caret rendering, Outline view, multi-column trailing clicks, unplaced-page arrow focus, refused-draft Undo/Redo, multi-window handoff, invalidated worker slots, and launch-error propagation.

## Verification

<!-- Fill in exact pass results after running locally on the final squashed branch -->
- `cargo fmt --all -- --check`:
- `cargo clippy -p wordcraft -p wordcraft-ui-egui --all-targets -- -D warnings`:
- `cargo test -p wordcraft -p wordcraft-ui-egui --lib -- code_chunk::tests::`:
- `cargo test -p wordcraft -p wordcraft-ui-egui --lib -- canvas::tests::`:
- `cargo xtask assets`:
- `cargo xtask layers`:
- `cargo xtask wasm`:
```
