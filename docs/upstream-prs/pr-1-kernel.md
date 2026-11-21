# PR 1 Blueprint — L1 `crates/kernel`

- **Layer:** L1 (`wordcraft-kernel`)
- **Depends on:** None (can be submitted in parallel with PR 2)
- **Suggested branch:** `feat/upstream-pr1-kernel`
- **Commit title / PR title:** `feat(kernel): add sandboxed R and Python code chunk execution crate`
- **Fork provenance:** `wordcraft` PRs `#34`, `#37`, `#39`, `#45` (`#43`), `#103` (`#75`), `#104` (`#74`), `#105` (`#73`), `#108` (`#76`)

## Exact File Scope (8 files)

- `Cargo.toml` (register `crates/kernel` in workspace members + workspace dependency)
- `Cargo.lock`
- `xtask/src/layers.rs` (register `wordcraft-kernel` at L1)
- `crates/kernel/Cargo.toml`
- `crates/kernel/src/lib.rs`
- `crates/kernel/src/process_group.rs`
- `crates/kernel/src/process_tree.rs`
- `crates/kernel/src/tests.rs`

## Local Verification Commands

```bash
cargo fmt --all -- --check
cargo clippy -p wordcraft-kernel --all-targets -- -D warnings
cargo test -p wordcraft-kernel
cargo xtask layers
cargo xtask wasm
```

---

## Ready-to-Use PR Description

```markdown
## Summary

Introduces `crates/kernel` (`wordcraft-kernel`, Layer 1), an isolated, panic-free subprocess execution engine for running R (`Rscript`) and Python (`python3`) code chunks with strict timeout enforcement, bounded output capture, plot interception (PNG and SVG), and structured tabular data extraction (`wc_table()`).

- **Workspace & layering (`Cargo.toml`, `Cargo.lock`, `xtask/src/layers.rs`):**
  - Registers `wordcraft-kernel` as a Layer 1 workspace member with `#![forbid(unsafe_code)]` and panic-denying clippy lints (`unwrap_used`, `expect_used`, `panic`, `unimplemented`, `todo`, `unreachable`).
  - Compiles cleanly on `wasm32-unknown-unknown` (returning typed unsupported-target errors where subprocess execution is unavailable).
- **Isolated execution & subprocess safety (`crates/kernel/src/lib.rs`, `process_group.rs`, `process_tree.rs`):**
  - Executes scripts in exclusively created owner-only scratch workspaces (`0o700` on Unix) bound to RAII drop guards.
  - Enforces hard deadlines with overflow-safe arithmetic, bounded stream/file readers (`Read::take`), UTF-8 char-boundary truncation, and full process-group/tree teardown with bounded pipe draining and zombie checks.
  - Skips non-executable candidates in `which_binary` and avoids canonicalizing symlinked interpreter binaries so virtualenvs stay intact.
- **Plot & tabular capture (`crates/kernel/src/lib.rs`):**
  - Captures PNG and/or SVG plots for R (base graphics and `grid.newpage()` / `ggplot2`) and Python (`matplotlib`), validating document completeness to reject truncated artifacts.
  - Extracts structured tables (`TabularOutput`) with pre-serialization limits on table count, rows, columns, and cell length, preserving Python `from __future__ import` and `__main__` semantics.

## Tests

- Unit and integration tests in `crates/kernel/src/tests.rs` covering timeouts, process-tree reaping, bounded stream/plot reads, SVG/PNG capture and truncation rejection, tabular extraction limits, UTF-8 boundary slicing, and interpreter discovery (guarded with `is_language_available` when R/Python are absent).

## Verification

<!-- Fill in exact pass results after running locally on the final squashed branch -->
- `cargo fmt --all -- --check`:
- `cargo clippy -p wordcraft-kernel --all-targets -- -D warnings`:
- `cargo test -p wordcraft-kernel`:
- `cargo xtask layers`:
- `cargo xtask wasm`:
```
