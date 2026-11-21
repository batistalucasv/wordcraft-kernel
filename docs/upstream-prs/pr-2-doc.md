# PR 2 Blueprint — L1 `crates/doc` (+ `crates/docx` tests)

- **Layer:** L1 (`wordcraft-doc`) + L2 round-trip integration tests (`crates/docx`)
- **Depends on:** None (can be submitted in parallel with PR 1)
- **Suggested branch:** `feat/upstream-pr2-doc-chunks`
- **Commit title / PR title:** `feat(doc): model executable code chunks over block content controls`
- **Fork provenance:** `wordcraft` PRs `#35`, `#46`, `#49`, `#102` (`#77`), `#106` (`#78`), `#107` (`#79`), `#109` (`#80`), `#110` (`#81`)

## Exact File Scope (5 files)

- `crates/doc/src/chunk.rs`
- `crates/doc/src/control.rs`
- `crates/doc/src/document.rs`
- `crates/doc/src/lib.rs`
- `crates/docx/tests/it/content_controls.rs`

## Local Verification Commands

```bash
cargo fmt --all -- --check
cargo clippy -p wordcraft-doc -p wordcraft-docx --all-targets -- -D warnings
cargo test -p wordcraft-doc
cargo test -p wordcraft-docx --test it content_controls
cargo xtask layers
cargo xtask wasm
```

---

## Ready-to-Use PR Description

```markdown
## Summary

Models executable code chunks in `crates/doc` (Layer 1) on top of standard OOXML block content controls (`w:sdtBlock` with `wordcraft.chunk.<id>` tags), enabling clean `.docx` round-trip fidelity in Microsoft Word while carrying language and chunk options (`eval`, `echo`, `label`, figure dimensions, etc.).

- **Chunk data model & operations (`crates/doc/src/chunk.rs`, `control.rs`, `document.rs`, `lib.rs`):**
  - Defines `CodeChunk`, `ChunkOptions`, `ChunkRef`, and `ChunkError` with strict validation (rejecting embedded `U+FFFC`, enforcing bounds, and preserving non-chunk document structure).
  - Carries serialized chunk options in the content control tag so copy/paste inside Word preserves `eval: false` and other options, while detecting missing/truncated options safely.
  - Adds `Document::update_chunk` and chunk inspection/mutation helpers scoped strictly to block content controls with control-ID collision safety and style preservation.
- **DOCX round-trip coverage (`crates/docx/tests/it/content_controls.rs`):**
  - Verifies `.docx` save/load preservation of code chunk content controls, tags, options, and multi-paragraph chunk bodies.

## Tests

- Unit tests in `crates/doc/src/chunk.rs` covering chunk creation, option parsing/serialization, tag overflow detection, `update_chunk`, `U+FFFC` rejection, and block-only SDT scoping.
- Integration tests in `crates/docx/tests/it/content_controls.rs` verifying `.docx` round-trip fidelity.

## Verification

<!-- Fill in exact pass results after running locally on the final squashed branch -->
- `cargo fmt --all -- --check`:
- `cargo clippy -p wordcraft-doc -p wordcraft-docx --all-targets -- -D warnings`:
- `cargo test -p wordcraft-doc`:
- `cargo test -p wordcraft-docx --test it content_controls`:
- `cargo xtask layers`:
- `cargo xtask wasm`:
```
