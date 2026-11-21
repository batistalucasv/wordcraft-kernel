# PR 3 Blueprint — L2 `crates/formats`

- **Layer:** L2 (`wordcraft-formats`)
- **Depends on:** PR 2 (`crates/doc`) — can be submitted in parallel with PR 4
- **Suggested branch:** `feat/upstream-pr3-quarto-formats`
- **Commit title / PR title:** `feat(formats): add Quarto (.qmd) and R Markdown (.Rmd) parser and writer`
- **Fork provenance:** `wordcraft` PRs `#35`, `#111` (`#84`), `#112` (`#85`), `#113` (`#86`), `#114` (`#83`), `#115` (`#82`), `#116` (`#87`)

## Exact File Scope (3 files)

- `crates/formats/src/quarto.rs`
- `crates/formats/src/comments.rs`
- `crates/formats/src/lib.rs`

## Local Verification Commands

```bash
cargo fmt --all -- --check
cargo clippy -p wordcraft-formats --all-targets -- -D warnings
cargo test -p wordcraft-formats
cargo xtask layers
cargo xtask wasm
```

---

## Ready-to-Use PR Description

```markdown
## Summary

Adds bidirectional Quarto (`.qmd`) and R Markdown (`.Rmd`) import/export support in `crates/formats` (Layer 2), building on the Layer 1 code chunk model from PR 2.

- **Quarto & R Markdown codec (`crates/formats/src/quarto.rs`, `lib.rs`, `comments.rs`):**
  - Parses YAML frontmatter, prose Markdown, and executable `{r}` / `{python}` fenced code chunks with both Quarto `#|` hash-pipe options (including multiline YAML block scalars and captions) and knitr header options.
  - Preserves CommonMark indentation/tab-stop rules, nested fences, list-item containment for replaced fences, leading blank lines before literal `#|` comments, and backtick info-string validation.
  - Avoids consuming `#|` lines rejected by `ChunkOptions::set` and explicitly reports unrepresentable table-cell code chunks on Quarto export rather than silently dropping them.

## Tests

- Comprehensive unit tests in `crates/formats/src/quarto.rs` covering `.qmd`/`.Rmd` round-trips, frontmatter handling, knitr & `#|` options, multiline captions, rejected option lines, list-item fences, table-cell export reporting, and hostile/edge-case fence syntax.

## Verification

<!-- Fill in exact pass results after running locally on the final squashed branch -->
- `cargo fmt --all -- --check`:
- `cargo clippy -p wordcraft-formats --all-targets -- -D warnings`:
- `cargo test -p wordcraft-formats`:
- `cargo xtask layers`:
- `cargo xtask wasm`:
```
