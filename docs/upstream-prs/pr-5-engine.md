# PR 5 Blueprint — L4 `crates/engine` (+ `docs/code-chunks.md`)

- **Layer:** L4 (`wordcraft-engine`) + documentation (`docs/code-chunks.md`)
- **Depends on:** PR 1 (`crates/kernel`), PR 2 (`crates/doc`), PR 3 (`crates/formats`), PR 4 (`crates/layout`)
- **Suggested branch:** `feat/upstream-pr5-engine-code`
- **Commit title / PR title:** `feat(engine): add code chunk commands, transient output state, and Quarto I/O`
- **Fork provenance:** `wordcraft` PRs `#35`, `#42`, `#46`, `#48`, `#49`, `#117` (`#89`), `#118` (`#92`), `#119` (`#88`), `#120` (`#90`), `#121` (`#91`), `#122` (`#93`)

## Exact File Scope (20 files)

- `crates/engine/Cargo.toml`
- `crates/engine/src/chunks.rs`
- `crates/engine/src/cmd.rs`
- `crates/engine/src/cmd/code.rs`
- `crates/engine/src/cmd/file.rs`
- `crates/engine/src/cmd/insert.rs`
- `crates/engine/src/cmd/insert/objects.rs`
- `crates/engine/src/cmd/md_source.rs`
- `crates/engine/src/cmd/text/enter_tab.rs`
- `crates/engine/src/io.rs`
- `crates/engine/src/lib.rs`
- `crates/engine/src/session.rs`
- `crates/engine/src/session/documents.rs`
- `crates/engine/src/session/history.rs`
- `crates/engine/src/session/layout.rs` (replaces PR 4's `Default::default()` shim with `self.chunks.chunk_heights().clone()`, `heights_rev` cache invalidation, and `Session::set_chunk_heights`)
- `crates/engine/src/session/lifecycle.rs`
- `crates/engine/src/session/snapshot.rs`
- `crates/engine/src/tests.rs`
- `crates/engine/src/tests/code.rs`
- `docs/code-chunks.md`

*(Note: `crates/engine/src/cmd/page/fit_page.rs` already received its 1-line `Placed::Chunk` match arm in PR 4.)*

## Local Verification Commands

```bash
cargo fmt --all -- --check
cargo clippy -p wordcraft-engine --all-targets -- -D warnings
cargo test -p wordcraft-engine --lib -- tests::code::
cargo test -p wordcraft-engine --lib -- md_source
cargo test -p wordcraft-engine --test '*'
cargo xtask layers
cargo xtask wasm
```

---

## Ready-to-Use PR Description

```markdown
## Summary

Adds the Layer 4 engine orchestration for executable code chunks and Quarto/R Markdown documents (`crates/engine` + `docs/code-chunks.md`), connecting `crates/kernel`, `crates/doc`, `crates/formats`, and `crates/layout`.

- **Code chunk commands (`crates/engine/src/cmd/code.rs`, `cmd.rs`, `docs/code-chunks.md`):**
  - Registers `code.insert_chunk`, `code.update_chunk`, `code.delete_chunk`, `code.exit_below_chunk`, `code.run_chunk`, `code.run_all`, `code.restart_kernel`, `code.clear_output`, `code.insert_plot`, `code.insert_table`, and `code.insert_output`.
  - Refuses `code.run_chunk` / `code.run_all` under automation filesystem confinement (`file_access::current().is_some()`), refuses `run_all` when chunk options are missing, and preserves section breaks when inserting output into an empty paragraph.
- **Transient chunk state & layout integration (`crates/engine/src/chunks.rs`, `session.rs`, `session/*.rs`):**
  - Tracks per-document transient execution outputs (`Session::chunks`) with source-fingerprint invalidation, kernel generation counters, bounded total output memory per document, and per-document parking across window/document switches.
  - Wires `Session::set_chunk_heights` into `Session::layout` and layout cache revision keys.
- **Quarto/R Markdown I/O & Markdown Source (`crates/engine/src/io.rs`, `cmd/file.rs`, `cmd/md_source.rs`, `cmd/text/enter_tab.rs`):**
  - Preserves unedited `.qmd`/`.Rmd` source text on save (`FileText`), warns when saving an oversized Markdown/Quarto file truncates text, rejects copy saves targeting the active file, closes Markdown Source when Save As transfers ownership to a non-Markdown path, and records Enter-at-chunk-end paragraph insertions under Track Changes.

## Tests

- Added `crates/engine/src/tests/code.rs` and expanded `crates/engine/src/tests.rs` and `crates/engine/src/cmd/md_source.rs` covering all `code.*` commands, memory bounds, confinement guards, `.qmd`/`.Rmd` round-trip preservation, Save As ownership/copy-password isolation, section-break preservation, and tracked insertions.

## Verification

<!-- Fill in exact pass results after running locally on the final squashed branch -->
- `cargo fmt --all -- --check`:
- `cargo clippy -p wordcraft-engine --all-targets -- -D warnings`:
- `cargo test -p wordcraft-engine --lib -- tests::code::`:
- `cargo test -p wordcraft-engine --lib -- md_source`:
- `cargo test -p wordcraft-engine --test '*'`:
- `cargo xtask layers`:
- `cargo xtask wasm`:
```
