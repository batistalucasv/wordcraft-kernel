# WordCraft Kernel

Safe, isolated execution runner for **R** and **Python** code chunks, designed for the **WordCraft** word processor ecosystem and interactive literate computing.

Developed specifically for research in **Ecology**, statistical data science, and **interactive programming education**, enabling live code chunks (similar to Quarto / R Markdown / Jupyter) embedded directly inside native word processor documents and terminal workflows.

---

## Features

- **100% Native Rust:** No unstable C-FFI, no unsafe code (`unsafe_code = "forbid"`), zero panics (`clippy::unwrap_used` denied in production code).
- **R & Python Parity:** Unified interface, consistent output capture, and standardized error handling across languages.
- **"Never Crash" Architecture:** Runs code in isolated child processes with configurable execution timeouts, terminating infinite loops and hung scripts automatically.
- **Dual Format Plot Capture:** Intercepts and extracts plots in both lossless vector (**SVG**) and high-resolution raster (**PNG**) formats:
  - **R:** Base graphics (`plot()`, `hist()`, `boxplot()`) and modern packages (`ggplot2`).
  - **Python:** `matplotlib`, `seaborn`, etc.
  - Configurable via `PlotFormat::Png`, `PlotFormat::Svg`, and `PlotFormat::Both`.
  - `with_prefer_svg(true)` upgrades the default PNG capture to SVG; an explicit `PlotFormat::Both` is preserved.
- **Clean Output Streams:** Strict separation of standard output (`stdout`), standard error (`stderr`), exit codes, and elapsed execution time.
- **Persistent Sessions:** A `SessionManager` keeps one background R or Python daemon per `session_id`, so datasets loaded by one chunk stay in memory for the next chunk. Sessions can be restarted, closed, and expire automatically when idle. Note: structured tabular data capture (`ExecutionResult::tables`) is supported in one-shot `execute()`; persistent sessions return `tables` as empty.
- **Template Library:** Ready-to-run, commented ecology and statistics snippets for R and Python, described in `templates`.
- **Standalone CLI (`wc-kernel`):** Command-line binary for direct terminal execution, inspection, piped scripts, and JSON integration with external tools.

---

## Command-Line Interface (`wc-kernel`)

The crate provides a standalone CLI binary, `wc-kernel`, for terminal execution and automation.

### Installation / Build

```bash
cargo install --path .
```

This installs both `wc-kernel` and its `wc-kernel-supervisor` helper. To build without installing, use `cargo build --release --bins` and run `./target/release/wc-kernel` instead.

### CLI Usage

```text
wc-kernel — Command-line runner for R and Python code chunks

USAGE:
    wc-kernel [OPTIONS] [FILE]

OPTIONS:
    -l, --lang <LANG>          Target language: 'r' or 'py'/'python'.
                               Optional if inferrable from script file extension (.R, .py).
    -e, --eval <CODE>          Inline code snippet to execute. May be specified multiple times.
    -f, --file <PATH>          Path to script file to execute (or '-' for stdin).
    --stdin                    Read script code from standard input (stdin).
    -t, --timeout <MS>         Execution timeout in milliseconds [default: 30000].
    --json                     Output full ExecutionResult structured as JSON.
    --out-plot <PATH>          Save captured plot image (PNG or SVG) to specified file path.
    --plot-format <FMT>        Plot capture format: 'png', 'svg', or 'both' [default: png].
    --no-plot                  Disable automatic plot interception.
    -C, --cwd, --working-dir <DIR>
                               Working directory for script execution
                               [default: current directory].
    -h, --help                 Display help information and exit.
    -V, --version              Display version information and exit.
```

Scripts run in the directory where you invoked `wc-kernel` unless `-C` is given, so relative reads and writes resolve there. When running a Python file with `--file`, its directory is also added to the module search path, so sibling imports keep working.

Captured output is capped at 10 MiB per stream. Bytes beyond the cap are discarded and `output_truncated` is set to `true`; if the script is still running when the supervisor detects the overrun, it is stopped. A script that finishes first may still return `success: true` with truncated output.

### CLI Examples

**Evaluate code inline:**
```bash
wc-kernel --lang r -e 'print(1+1)'
wc-kernel --lang py -e 'print("Hello from Python")'
```

**Output structured JSON for automation:**
```bash
wc-kernel --lang r -e 'print(1+1)' --json
```
Output:
```json
{
  "stdout": "[1] 2\n",
  "stderr": "",
  "exit_code": 0,
  "success": true,
  "timed_out": false,
  "output_truncated": false,
  "plot_png": null,
  "plot_svg": null,
  "duration_ms": 42
}
```

**Execute script files (language inferred from file extension):**
```bash
wc-kernel --file analysis.R
wc-kernel script.py --json
```

**Pipe code via standard input:**
```bash
cat script.R | wc-kernel --lang r
echo 'print(40 + 2)' | wc-kernel --lang py
```

**Save captured plot figures (PNG or SVG):**
```bash
wc-kernel --lang r -e 'plot(1:10, 1:10, main="Test")' --out-plot figure.png
wc-kernel --lang r -e 'plot(1:10, 1:10, main="Vector")' --out-plot figure.svg
```

---

## Using as a Rust Library

### Adding as a Dependency

In your `Cargo.toml`:

```toml
[dependencies]
wordcraft-kernel = { git = "https://github.com/batistalucasv/wordcraft-kernel" }
```

The `wc-kernel-supervisor` executable must be available beside the host executable or on `PATH`. If it is stored elsewhere, set it per request with `ExecutionRequest::with_supervisor_path`. Linux uses a child subreaper and Windows uses a Job Object to stop descendants that leave the interpreter's process group. Other Unix systems retain process-group cleanup but cannot guarantee cleanup of descendants that create a new session.

### Rust Example

```rust
use wordcraft_kernel::{execute, CodeLanguage, ExecutionRequest, PlotFormat};

fn main() {
    // R example with SVG plot capture
    let r_code = r#"
        species <- c("Species A", "Species B", "Species C")
        abundance <- c(10, 25, 5)
        cat("Total individuals:", sum(abundance), "\n")
        barplot(abundance, names.arg = species, main = "Species Abundance")
    "#;

    let req = ExecutionRequest::new(CodeLanguage::R, r_code)
        .with_timeout(10_000)
        .with_plot_capture(true)
        .with_plot_format(PlotFormat::Both);

    match execute(&req) {
        Ok(result) => {
            println!("R Output:\n{}", result.stdout);
            if let Some(png_bytes) = result.plot_png {
                println!("Raster PNG captured: {} bytes", png_bytes.len());
            }
            if let Some(svg_str) = result.plot_svg {
                println!("Vector SVG captured: {} characters", svg_str.len());
            }
        }
        Err(err) => eprintln!("Execution error: {err}"),
    }

    // Python example
    let py_code = r#"
        import numpy as np
        data = np.array([12, 15, 18, 22, 30])
        print("Mean:", np.mean(data))
    "#;

    let req_py = ExecutionRequest::new(CodeLanguage::Python, py_code);
    if let Ok(res) = execute(&req_py) {
        println!("Python Output:\n{}", res.stdout);
    }
}
```

---

## Template library

`wordcraft_kernel::templates` ships seven complete, commented snippets that run
on synthetic data out of the box:

| Template id | Language | Category | Packages |
|---|---|---|---|
| `r.shannon-simpson` | R | Diversity indices | `vegan` |
| `r.species-accumulation` | R | Species accumulation | `vegan` |
| `r.lm-confidence` | R | Regression | `ggplot2` |
| `r.ttest-anova` | R | Hypothesis testing | `ggplot2` |
| `python.data-ingest` | Python | Data ingestion | `pandas` |
| `python.correlation-heatmap` | Python | Correlation | `pandas`, `seaborn`, `matplotlib`, `scipy` |
| `python.lm-tests` | Python | Regression | `numpy`, `pandas`, `scipy` |

Browse with `templates::all()`, `templates::find(id)`,
`templates::by_language(...)` and `templates::by_category(...)`, then render and
run one:

```rust
use wordcraft_kernel::{execute, templates, ExecutionRequest, KernelError};

fn main() -> Result<(), KernelError> {
    let template = templates::find("r.ttest-anova").expect("built-in template");
    let script = template.render(&[("title", "Biomass at three sites")])?;
    let result = execute(&ExecutionRequest::new(template.language, script))?;
    println!("{}", result.stdout);
    Ok(())
}
```

Every parameter has a default, so `render_default()` always produces a runnable
script. Values are inserted verbatim because they are code (an R expression, a
column name, a path); quote or validate untrusted input for the target language
before rendering. The test suite runs each snippet when its interpreter and
listed packages are installed, and skips it with a message otherwise.

## Tabular data capture

Scripts that return a data frame or matrix fill `ExecutionResult::tables` with `TabularOutput`
values (`headers` plus a list of text rows), ready to be inserted as native tables:

- **R:** every visible top-level `data.frame` or `matrix` is captured, so `anova(fit)` keeps its
  term names as a leading column.
- **Python:** the final expression is captured when it evaluates to a `pandas.DataFrame`.
- **Explicit helper:** call `wc_table(df)` in either language for a value that is not
  the script's last expression (accepts `data.frame` or `matrix` in R, `pandas.DataFrame` in Python).

```rust
let code = r#"
data.frame(species = c("Sp1", "Sp2"), abundance = c(10, 25))
"#;

if let Ok(result) = execute(&ExecutionRequest::new(CodeLanguage::R, code)) {
    if let Some(table) = result.tables.first() {
        println!("headers: {:?}", table.headers);
        println!("first row: {:?}", table.rows.first());
    }
}
```

Both writers are bounded: at most 16 tables per run, 10,000 rows and 256 columns per
table, 4,096 characters per cell, and 250,000 total cells. A malformed or oversized payload
yields no tables instead of an error.

---

## Persistent Sessions

`execute` starts a fresh interpreter for every call, which is the right default
for an isolated preview. A document with several chunks usually wants the
opposite: load the dataset once, then keep working on it. `SessionManager` keeps
one daemon per `session_id` for that.

```rust
use std::time::Duration;
use wordcraft_kernel::{CodeLanguage, ExecutionRequest, SessionManager};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut sessions = SessionManager::new().with_idle_timeout(Duration::from_secs(600));

    // Chunk 1: load data from current working directory
    let load = ExecutionRequest::new(
        CodeLanguage::R,
        r#"data <- read.csv("abundance.csv")
           total <- sum(data$count)"#,
    )
    .with_working_dir(".");

    let res1 = sessions.execute("field-study", &load)?;
    if !res1.success {
        eprintln!("Failed to load dataset: {}", res1.stderr);
        return Ok(());
    }

    // Chunk 2: the dataset and `total` variable remain available in memory
    let summary = ExecutionRequest::new(CodeLanguage::R, "cat('Total:', total, '\n')");
    let res2 = sessions.execute("field-study", &summary)?;
    println!("{}", res2.stdout);

    // Chunk 3: restart the session to clear memory and start fresh
    sessions.restart_session("field-study")?;

    // Optional: reclaim idle daemons explicitly instead of waiting for automatic reap
    let reaped = sessions.reap_idle_sessions();
    println!("Reaped {reaped} idle session(s)");
    Ok(())
}
```

Rules that apply to session code:

- The daemon keeps its global environment, imports and working directory between
  chunks. A failed chunk leaves the session alive, and later chunks can inspect
  whatever the failed chunk had already assigned.
- Chunks must not read standard input: Python chunks receive an empty stdin stream
  while the driver reserves stdin for IPC, whereas R chunks receive null stdin and
  requests arrive over an authenticated loopback socket.
- Tabular data capture (`ExecutionResult::tables`) is supported in one-shot `execute()`;
  persistent sessions return `tables` as empty.
- Subprocess output from Python user code is collected into the chunk's own output files,
  isolating the protocol stream from spawned child processes. Collectors stop with the
  chunk, so a Python subprocess that keeps writing after its chunk may receive a broken
  pipe. In R, subprocesses inherit null stdin; their stdout and stderr are drained into the
  same chunk result as the interpreter's own output. Output is capped per chunk: excess
  bytes are discarded and `ExecutionResult::output_truncated` is set. Output written by an
  R background subprocess after its chunk returns is discarded while the session is idle;
  R cannot portably redirect an inherited descriptor, so a later active chunk may see a
  write made by that background process.
- A chunk that exceeds `timeout_ms` terminates the daemon process tree; the call returns
  `timed_out` and the next call for that id starts a fresh interpreter.
- Session ids belong to one language. Reusing an id with another language
  returns `KernelError::SessionLanguageMismatch`.
- `idle_timeout` defaults to ten minutes. A zero timeout disables reaping. Idle
  sessions are also reaped on every `execute`; call `reap_idle_sessions` if the
  host has its own timer.

---

## Testing

Run unit and integration tests:

```bash
cargo test
```

Run linter:

```bash
cargo clippy -- -D warnings
```

---

## License

Distributed under either the **MIT** or **Apache-2.0** license, at your option:

- [LICENSE-MIT](LICENSE-MIT)
- [LICENSE-APACHE](LICENSE-APACHE)

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
