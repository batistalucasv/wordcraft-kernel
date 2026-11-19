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
- **Clean Output Streams:** Strict separation of standard output (`stdout`), standard error (`stderr`), exit codes, and elapsed execution time.
- **Standalone CLI (`wc-kernel`):** Command-line binary for direct terminal execution, inspection, piped scripts, and JSON integration with external tools.

---

## Command-Line Interface (`wc-kernel`)

The crate provides a standalone CLI binary, `wc-kernel`, for terminal execution and automation.

### Installation / Build

```bash
cargo build --release --bin wc-kernel
```

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
                               Working directory for script execution.
    -h, --help                 Display help information and exit.
    -V, --version              Display version information and exit.
```

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

Distributed under either the **MIT** or **Apache-2.0** license.
