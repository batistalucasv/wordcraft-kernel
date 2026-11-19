//! Standalone CLI runner for WordCraft Kernel (`wc-kernel`).
//!
//! Executes R and Python scripts directly from the terminal, with options
//! for JSON serialization, custom timeouts, plot export, and stdin streaming.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use wordcraft_kernel::{
    CodeLanguage, ExecutionRequest, KernelError, PlotFormat, execute_with_cancellation,
};

const USAGE: &str = "\
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
    -h, --help                 Display this help message and exit.
    -V, --version              Display version information and exit.

EXAMPLES:
    wc-kernel --lang r -e 'print(1+1)'
    wc-kernel --lang py -e 'print(2+2)' --json
    wc-kernel --file analysis.R --out-plot figure.png
    wc-kernel --lang r -e 'plot(1:10)' --out-plot figure.svg
    cat script.R | wc-kernel --lang r
";

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, PartialEq, Eq)]
enum CliError {
    Usage(String),
    Runtime(String),
}

#[derive(Debug, Default)]
struct CliArgs {
    language: Option<CodeLanguage>,
    eval_snippets: Vec<String>,
    file_path: Option<PathBuf>,
    read_stdin: bool,
    timeout_ms: u64,
    json: bool,
    out_plot: Option<PathBuf>,
    plot_format: Option<PlotFormat>,
    capture_plots: bool,
    working_dir: Option<PathBuf>,
}

fn plot_format_name(format: PlotFormat) -> &'static str {
    match format {
        PlotFormat::Png => "png",
        PlotFormat::Svg => "svg",
        PlotFormat::Both => "both",
    }
}

fn plot_format_for_extension(path: &Path) -> Option<PlotFormat> {
    let extension = path.extension()?.to_str()?;
    if extension.eq_ignore_ascii_case("png") {
        Some(PlotFormat::Png)
    } else if extension.eq_ignore_ascii_case("svg") {
        Some(PlotFormat::Svg)
    } else {
        None
    }
}

fn parse_plot_format(val: &str) -> Result<PlotFormat, CliError> {
    match val.trim().to_ascii_lowercase().as_str() {
        "png" => Ok(PlotFormat::Png),
        "svg" => Ok(PlotFormat::Svg),
        "both" => Ok(PlotFormat::Both),
        other => Err(CliError::Usage(format!(
            "unsupported plot format '{other}'. Expected 'png', 'svg', or 'both'"
        ))),
    }
}

fn next_text_arg(
    args: &mut impl Iterator<Item = OsString>,
    option: &str,
    expected: &str,
) -> Result<String, CliError> {
    let value = args.next().ok_or_else(|| {
        CliError::Usage(format!("option '{option}' requires an argument {expected}"))
    })?;
    value
        .into_string()
        .map_err(|_| CliError::Usage(format!("argument for '{option}' must be valid UTF-8")))
}

fn next_path_arg(
    args: &mut impl Iterator<Item = OsString>,
    option: &str,
    expected: &str,
) -> Result<PathBuf, CliError> {
    args.next().map(PathBuf::from).ok_or_else(|| {
        CliError::Usage(format!("option '{option}' requires an argument {expected}"))
    })
}

#[cfg(unix)]
fn strip_os_prefix(value: &OsStr, prefix: &str) -> Option<OsString> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    value
        .as_bytes()
        .strip_prefix(prefix.as_bytes())
        .map(|suffix| OsString::from_vec(suffix.to_vec()))
}

#[cfg(windows)]
fn strip_os_prefix(value: &OsStr, prefix: &str) -> Option<OsString> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let value = value.encode_wide().collect::<Vec<_>>();
    let prefix = prefix.encode_utf16().collect::<Vec<_>>();
    value
        .strip_prefix(prefix.as_slice())
        .map(OsString::from_wide)
}

#[cfg(not(any(unix, windows)))]
fn strip_os_prefix(value: &OsStr, prefix: &str) -> Option<OsString> {
    value
        .to_str()
        .and_then(|value| value.strip_prefix(prefix))
        .map(OsString::from)
}

fn set_file_path(cli: &mut CliArgs, path: PathBuf) -> Result<(), CliError> {
    if cli.file_path.is_some() {
        return Err(CliError::Usage(
            "multiple --file options are not allowed".to_string(),
        ));
    }
    cli.file_path = Some(path);
    Ok(())
}

fn write_all(handle: &mut impl Write, bytes: &[u8]) -> Result<(), CliError> {
    match handle.write_all(bytes).and_then(|()| handle.flush()) {
        Ok(()) => Ok(()),
        // The reader closed the pipe. Stop reporting, but let the completed
        // script's own exit status decide how the CLI ends.
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(CliError::Runtime(format!(
            "failed to write output: {error}"
        ))),
    }
}

fn write_stdout(text: &str) -> Result<(), CliError> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    write_all(&mut handle, text.as_bytes())
}

fn write_stderr(text: &str) -> Result<(), CliError> {
    let stderr = io::stderr();
    let mut handle = stderr.lock();
    write_all(&mut handle, text.as_bytes())
}

/// Read all of stdin while staying responsive to cancellation.
///
/// The blocking read lives in a helper thread because the installed signal
/// handler restarts interrupted reads. `None` reports a cancelled run.
fn read_stdin(cancelled: &AtomicBool) -> Result<Option<String>, CliError> {
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("wc-kernel-stdin".to_string())
        .spawn(move || {
            let mut buffer = String::new();
            let result = io::stdin().read_to_string(&mut buffer).map(|_| buffer);
            let _ = sender.send(result);
        })
        .map_err(|error| CliError::Runtime(format!("failed to read from stdin: {error}")))?;

    loop {
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(Ok(text)) => return Ok(Some(text)),
            Ok(Err(error)) => {
                return Err(CliError::Runtime(format!(
                    "failed to read from stdin: {error}"
                )));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if cancelled.load(Ordering::Acquire) {
                    return Ok(None);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(CliError::Runtime(
                    "failed to read from stdin: reader stopped".to_string(),
                ));
            }
        }
    }
}

fn parse_args<I, S>(args: I) -> Result<Option<CliArgs>, CliError>
where
    I: Iterator<Item = S>,
    S: Into<OsString>,
{
    let mut args = args.map(Into::into);
    let mut cli = CliArgs {
        timeout_ms: 30_000,
        capture_plots: true,
        ..Default::default()
    };

    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else {
            let path_option = [
                ("--file=", 0_u8),
                ("-f=", 0),
                ("--out-plot=", 1),
                ("--working-dir=", 2),
                ("--cwd=", 2),
                ("-C=", 2),
            ]
            .into_iter()
            .find_map(|(prefix, kind)| strip_os_prefix(&arg, prefix).map(|path| (kind, path)));
            if let Some((kind, path)) = path_option {
                match kind {
                    0 => set_file_path(&mut cli, PathBuf::from(path))?,
                    1 => cli.out_plot = Some(PathBuf::from(path)),
                    _ => cli.working_dir = Some(PathBuf::from(path)),
                }
                continue;
            }
            if arg.to_string_lossy().starts_with('-') {
                return Err(CliError::Usage(format!(
                    "unrecognized option '{}'",
                    arg.to_string_lossy()
                )));
            }
            set_file_path(&mut cli, PathBuf::from(arg))?;
            continue;
        };
        if arg == "-h" || arg == "--help" {
            write_stdout(USAGE)?;
            return Ok(None);
        } else if arg == "-V" || arg == "--version" {
            write_stdout(&format!("wc-kernel {VERSION}\n"))?;
            return Ok(None);
        } else if arg == "--json" {
            cli.json = true;
        } else if arg == "--no-plot" {
            cli.capture_plots = false;
        } else if arg == "--stdin" {
            cli.read_stdin = true;
        } else if arg == "-l" || arg == "--lang" {
            let val = next_text_arg(&mut args, arg, "<r|py>")?;
            let lang = CodeLanguage::from_str(&val).map_err(|_| {
                CliError::Usage(format!(
                    "unsupported language '{val}'. Expected 'r' or 'py'/'python'"
                ))
            })?;
            cli.language = Some(lang);
        } else if let Some(val) = arg.strip_prefix("--lang=") {
            let lang = CodeLanguage::from_str(val).map_err(|_| {
                CliError::Usage(format!(
                    "unsupported language '{val}'. Expected 'r' or 'py'/'python'"
                ))
            })?;
            cli.language = Some(lang);
        } else if let Some(val) = arg.strip_prefix("-l=") {
            let lang = CodeLanguage::from_str(val).map_err(|_| {
                CliError::Usage(format!(
                    "unsupported language '{val}'. Expected 'r' or 'py'/'python'"
                ))
            })?;
            cli.language = Some(lang);
        } else if arg == "-e" || arg == "--eval" {
            let val = next_text_arg(&mut args, arg, "<code>")?;
            cli.eval_snippets.push(val);
        } else if let Some(val) = arg.strip_prefix("--eval=") {
            cli.eval_snippets.push(val.to_string());
        } else if let Some(val) = arg.strip_prefix("-e=") {
            cli.eval_snippets.push(val.to_string());
        } else if arg == "-f" || arg == "--file" {
            set_file_path(&mut cli, next_path_arg(&mut args, arg, "<path>")?)?;
        } else if let Some(val) = arg.strip_prefix("--file=") {
            set_file_path(&mut cli, PathBuf::from(val))?;
        } else if let Some(val) = arg.strip_prefix("-f=") {
            set_file_path(&mut cli, PathBuf::from(val))?;
        } else if arg == "-t" || arg == "--timeout" {
            let val = next_text_arg(&mut args, arg, "<ms>")?;
            let ms: u64 = val.parse().map_err(|_| {
                CliError::Usage(format!(
                    "invalid timeout '{val}'. Expected a positive integer in milliseconds"
                ))
            })?;
            if ms == 0 {
                return Err(CliError::Usage(
                    "timeout must be greater than zero".to_string(),
                ));
            }
            cli.timeout_ms = ms;
        } else if let Some(val) = arg.strip_prefix("--timeout=") {
            let ms: u64 = val.parse().map_err(|_| {
                CliError::Usage(format!(
                    "invalid timeout '{val}'. Expected a positive integer in milliseconds"
                ))
            })?;
            if ms == 0 {
                return Err(CliError::Usage(
                    "timeout must be greater than zero".to_string(),
                ));
            }
            cli.timeout_ms = ms;
        } else if let Some(val) = arg.strip_prefix("-t=") {
            let ms: u64 = val.parse().map_err(|_| {
                CliError::Usage(format!(
                    "invalid timeout '{val}'. Expected a positive integer in milliseconds"
                ))
            })?;
            if ms == 0 {
                return Err(CliError::Usage(
                    "timeout must be greater than zero".to_string(),
                ));
            }
            cli.timeout_ms = ms;
        } else if arg == "--out-plot" {
            cli.out_plot = Some(next_path_arg(&mut args, arg, "<path>")?);
        } else if let Some(val) = arg.strip_prefix("--out-plot=") {
            cli.out_plot = Some(PathBuf::from(val));
        } else if arg == "--plot-format" {
            let val = next_text_arg(&mut args, arg, "<png|svg|both>")?;
            cli.plot_format = Some(parse_plot_format(&val)?);
        } else if let Some(val) = arg.strip_prefix("--plot-format=") {
            cli.plot_format = Some(parse_plot_format(val)?);
        } else if arg == "-C" || arg == "--cwd" || arg == "--working-dir" {
            cli.working_dir = Some(next_path_arg(&mut args, arg, "<dir>")?);
        } else if let Some(val) = arg.strip_prefix("--cwd=") {
            cli.working_dir = Some(PathBuf::from(val));
        } else if let Some(val) = arg.strip_prefix("--working-dir=") {
            cli.working_dir = Some(PathBuf::from(val));
        } else if let Some(val) = arg.strip_prefix("-C=") {
            cli.working_dir = Some(PathBuf::from(val));
        } else if arg == "-" {
            if cli.file_path.is_some() || cli.read_stdin {
                return Err(CliError::Usage(
                    "multiple input sources specified".to_string(),
                ));
            }
            cli.read_stdin = true;
        } else if !arg.starts_with('-') {
            if cli.file_path.is_some() {
                return Err(CliError::Usage(format!("unexpected argument '{arg}'")));
            }
            cli.file_path = Some(PathBuf::from(arg));
        } else {
            return Err(CliError::Usage(format!("unrecognized option '{arg}'")));
        }
    }

    if !cli.capture_plots && cli.out_plot.is_some() {
        return Err(CliError::Usage(
            "cannot specify '--out-plot' when '--no-plot' is enabled".to_string(),
        ));
    }

    if let Some(ref path) = cli.out_plot
        && let Some(extension_format) = plot_format_for_extension(path)
    {
        match cli.plot_format {
            None => cli.plot_format = Some(extension_format),
            Some(requested) if requested != extension_format && requested != PlotFormat::Both => {
                return Err(CliError::Usage(format!(
                    "conflicting --plot-format '{}' for output file '{}'. Use '{}' or 'both'",
                    plot_format_name(requested),
                    path.display(),
                    plot_format_name(extension_format),
                )));
            }
            Some(_) => {}
        }
    }

    if let Some(ref dir) = cli.working_dir
        && !dir.is_dir()
    {
        return Err(CliError::Usage(format!(
            "working directory '{}' does not exist or is not a directory",
            dir.display()
        )));
    }

    Ok(Some(cli))
}

fn infer_language_from_path(path: &Path) -> Option<CodeLanguage> {
    let ext = path.extension()?.to_str()?;
    match ext.to_ascii_lowercase().as_str() {
        "r" => Some(CodeLanguage::R),
        "py" => Some(CodeLanguage::Python),
        _ => None,
    }
}

fn run(cancelled: &AtomicBool) -> Result<ExitCode, CliError> {
    let args = std::env::args_os().skip(1);
    let maybe_cli = parse_args(args)?;
    let cli = match maybe_cli {
        Some(c) => c,
        None => return Ok(ExitCode::SUCCESS),
    };

    let (code, inferred_lang, source_dir) = if !cli.eval_snippets.is_empty() {
        if cli.file_path.is_some() || cli.read_stdin {
            return Err(CliError::Usage(
                "cannot combine -e/--eval with --file, positional file, or --stdin".to_string(),
            ));
        }
        (cli.eval_snippets.join("\n"), None, None)
    } else if let Some(ref path) = cli.file_path {
        if cli.read_stdin {
            return Err(CliError::Usage(
                "cannot combine --file with --stdin".to_string(),
            ));
        }
        if path.as_os_str() == "-" {
            let Some(text) = read_stdin(cancelled)? else {
                return Ok(ExitCode::from(130));
            };
            (text, None, None)
        } else {
            let content = fs::read_to_string(path).map_err(|e| {
                CliError::Runtime(format!("failed to read file '{}': {e}", path.display()))
            })?;
            let inferred = infer_language_from_path(path);
            let source_dir = fs::canonicalize(path)
                .ok()
                .and_then(|canonical| canonical.parent().map(Path::to_path_buf));
            (content, inferred, source_dir)
        }
    } else if cli.read_stdin || !io::stdin().is_terminal() {
        let Some(text) = read_stdin(cancelled)? else {
            return Ok(ExitCode::from(130));
        };
        (text, None, None)
    } else {
        return Err(CliError::Usage(
            "no code provided. Specify -e <code>, --file <path>, or pipe via stdin".to_string(),
        ));
    };

    let language = match (cli.language, inferred_lang) {
        (Some(lang), _) => lang,
        (None, Some(lang)) => lang,
        (None, None) => {
            return Err(CliError::Usage(
                "missing required argument --lang <r|py>. Language could not be inferred from file extension"
                    .to_string(),
            ));
        }
    };

    let plot_format = match cli.plot_format {
        Some(fmt) => fmt,
        None => {
            if let Some(ref path) = cli.out_plot {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    if ext.eq_ignore_ascii_case("svg") {
                        PlotFormat::Svg
                    } else {
                        PlotFormat::Png
                    }
                } else {
                    PlotFormat::Png
                }
            } else {
                PlotFormat::Png
            }
        }
    };

    let mut req = ExecutionRequest::new(language, code)
        .with_timeout(cli.timeout_ms)
        .with_plot_capture(cli.capture_plots)
        .with_plot_format(plot_format);

    if language == CodeLanguage::Python
        && let Some(ref dir) = source_dir
    {
        req = req.with_python_path(dir);
    }

    match cli.working_dir {
        Some(dir) => req = req.with_working_dir(dir),
        None => {
            let cwd = std::env::current_dir()
                .map_err(|e| CliError::Runtime(format!("failed to read current directory: {e}")))?;
            req = req.with_working_dir(cwd);
        }
    }

    let result = match execute_with_cancellation(&req, cancelled) {
        Ok(res) => res,
        Err(KernelError::Interrupted) => return Ok(ExitCode::from(130)),
        Err(err) => {
            return Err(CliError::Runtime(err.to_string()));
        }
    };

    let mut export_error = None;
    if let Some(ref plot_path) = cli.out_plot {
        let target_format = plot_format_for_extension(plot_path).unwrap_or(match plot_format {
            PlotFormat::Svg => PlotFormat::Svg,
            PlotFormat::Png | PlotFormat::Both => PlotFormat::Png,
        });

        let written = match target_format {
            PlotFormat::Svg => {
                if let Some(ref svg_str) = result.plot_svg {
                    match write_file_with_parent_dir(plot_path, svg_str.as_bytes()) {
                        Ok(()) => true,
                        Err(err) => {
                            export_error = Some(err);
                            false
                        }
                    }
                } else {
                    false
                }
            }
            PlotFormat::Png | PlotFormat::Both => {
                if let Some(ref bytes) = result.plot_png {
                    match write_file_with_parent_dir(plot_path, bytes) {
                        Ok(()) => true,
                        Err(err) => {
                            export_error = Some(err);
                            false
                        }
                    }
                } else {
                    false
                }
            }
        };

        if !written && export_error.is_none() && cli.capture_plots {
            write_stderr(&format!(
                "wc-kernel: warning: script did not produce a {} plot; '{}' was not written\n",
                plot_format_name(target_format),
                plot_path.display(),
            ))?;
        }
    }

    if cli.json {
        let json_str = serde_json::to_string_pretty(&result)
            .map_err(|e| CliError::Runtime(format!("failed to serialize JSON output: {e}")))?;
        write_stdout(&format!("{json_str}\n"))?;
    } else {
        if !result.stdout.is_empty() {
            write_stdout(&result.stdout)?;
        }
        if !result.stderr.is_empty() {
            write_stderr(&result.stderr)?;
        }
        if result.timed_out {
            write_stderr(&format!(
                "wc-kernel: execution timed out after {} ms\n",
                cli.timeout_ms
            ))?;
        }
    }

    if result.output_truncated {
        write_stderr(
            "wc-kernel: warning: output exceeded the capture limit; captured output was truncated\n",
        )?;
    }

    if let Some(err) = export_error {
        return Err(err);
    }

    if result.success {
        Ok(ExitCode::SUCCESS)
    } else if result.timed_out {
        Ok(ExitCode::from(124))
    } else {
        let code = match result.exit_code {
            Some(c) => {
                let clamped = c.clamp(0, 255) as u8;
                if clamped == 0 { 1 } else { clamped }
            }
            None => 1,
        };
        Ok(ExitCode::from(code))
    }
}

fn write_file_with_parent_dir(path: &Path, content: &[u8]) -> Result<(), CliError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| {
            CliError::Runtime(format!(
                "failed to create directory '{}': {e}",
                parent.display()
            ))
        })?;
    }
    fs::write(path, content).map_err(|e| {
        CliError::Runtime(format!("failed to write plot to '{}': {e}", path.display()))
    })
}

fn main() -> ExitCode {
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&cancelled);
    if let Err(error) = ctrlc::set_handler(move || signal_flag.store(true, Ordering::Release)) {
        let _ = writeln!(
            io::stderr(),
            "wc-kernel: error: failed to install the interrupt handler: {error}"
        );
        return ExitCode::FAILURE;
    }

    match run(&cancelled) {
        Ok(code) => code,
        Err(CliError::Usage(msg)) => {
            let mut stderr = io::stderr();
            let _ = writeln!(stderr, "wc-kernel: error: {msg}");
            let _ = writeln!(stderr, "Try 'wc-kernel --help' for more information.");
            ExitCode::from(2)
        }
        Err(CliError::Runtime(msg)) => {
            let _ = writeln!(io::stderr(), "wc-kernel: error: {msg}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_cli(args: Vec<String>) -> io::Result<Option<CliArgs>> {
        parse_args(args.into_iter()).map_err(|error| io::Error::other(format!("{error:?}")))
    }

    fn assert_usage_error(args: Vec<String>) -> io::Result<()> {
        match parse_args(args.into_iter()) {
            Err(CliError::Usage(_)) => Ok(()),
            Err(error) => Err(io::Error::other(format!(
                "unexpected parser error: {error:?}"
            ))),
            Ok(_) => Err(io::Error::other("expected a usage error")),
        }
    }

    #[test]
    fn test_parse_help() -> io::Result<()> {
        let args = vec!["--help".to_string()];
        let parsed = parse_cli(args)?;
        assert!(parsed.is_none());
        Ok(())
    }

    #[test]
    fn test_parse_version() -> io::Result<()> {
        let args = vec!["-V".to_string()];
        let parsed = parse_cli(args)?;
        assert!(parsed.is_none());
        Ok(())
    }

    #[test]
    fn test_parse_eval_and_lang() -> io::Result<()> {
        let args = vec![
            "--lang".to_string(),
            "r".to_string(),
            "-e".to_string(),
            "print(1+1)".to_string(),
        ];
        let parsed = parse_cli(args)?.ok_or_else(|| io::Error::other("expected CLI arguments"))?;
        assert_eq!(parsed.language, Some(CodeLanguage::R));
        assert_eq!(parsed.eval_snippets, vec!["print(1+1)"]);
        assert_eq!(parsed.timeout_ms, 30_000);
        assert!(!parsed.json);
        Ok(())
    }

    #[test]
    fn test_parse_json_timeout_and_out_plot() -> io::Result<()> {
        let args = vec![
            "-l".to_string(),
            "py".to_string(),
            "-e".to_string(),
            "print(2)".to_string(),
            "--json".to_string(),
            "--timeout".to_string(),
            "5000".to_string(),
            "--out-plot".to_string(),
            "out.png".to_string(),
            "--plot-format".to_string(),
            "both".to_string(),
        ];
        let parsed = parse_cli(args)?.ok_or_else(|| io::Error::other("expected CLI arguments"))?;
        assert_eq!(parsed.language, Some(CodeLanguage::Python));
        assert!(parsed.json);
        assert_eq!(parsed.timeout_ms, 5000);
        assert_eq!(parsed.out_plot, Some(PathBuf::from("out.png")));
        assert_eq!(parsed.plot_format, Some(PlotFormat::Both));
        Ok(())
    }

    #[test]
    fn test_parse_file_positional() -> io::Result<()> {
        let args = vec!["script.py".to_string()];
        let parsed = parse_cli(args)?.ok_or_else(|| io::Error::other("expected CLI arguments"))?;
        assert_eq!(parsed.file_path, Some(PathBuf::from("script.py")));
        Ok(())
    }

    #[test]
    fn test_infer_language() {
        assert_eq!(
            infer_language_from_path(Path::new("test.R")),
            Some(CodeLanguage::R)
        );
        assert_eq!(
            infer_language_from_path(Path::new("test.r")),
            Some(CodeLanguage::R)
        );
        assert_eq!(
            infer_language_from_path(Path::new("test.py")),
            Some(CodeLanguage::Python)
        );
        assert_eq!(infer_language_from_path(Path::new("test.txt")), None);
    }

    #[test]
    fn test_parse_invalid_args() -> io::Result<()> {
        assert_usage_error(vec!["--unrecognized".to_string()])?;
        assert_usage_error(vec!["--timeout".to_string(), "0".to_string()])?;
        assert_usage_error(vec!["--lang".to_string(), "rust".to_string()])?;
        assert_usage_error(vec!["--plot-format".to_string(), "invalid".to_string()])?;
        Ok(())
    }
}
