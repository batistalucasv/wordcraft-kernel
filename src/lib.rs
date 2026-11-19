//! Execution runner for interactive code chunks in WordCraft (R and Python).
//!
//! Provides isolated, safe subprocess execution with timeout enforcement,
//! output capture (stdout, stderr, exit status), plot interception (PNG and vector SVG),
//! and structured capture of tabular data (R `data.frame` and Python `pandas.DataFrame`).
//! [`templates`] ships ready-made ecology and statistics snippets for both languages.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod templates;

use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use command_group::{CommandGroup, GroupChild};
use serde::{Deserialize, Serialize};

static SCRIPT_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Maximum bytes kept from each captured stream (stdout and stderr).
const MAX_CAPTURE_BYTES: usize = 10 * 1024 * 1024;

/// How long the supervisor gets to cancel the interpreter before it is killed.
#[cfg(unix)]
const SUPERVISOR_CLEANUP_GRACE: Duration = Duration::from_secs(4);

/// Programming language for a code chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodeLanguage {
    R,
    Python,
}

impl CodeLanguage {
    /// Canonical name of the language (lowercase).
    pub fn as_str(self) -> &'static str {
        match self {
            CodeLanguage::R => "r",
            CodeLanguage::Python => "python",
        }
    }

    /// Primary binary executable name searched on PATH.
    pub fn binary_name(self) -> &'static str {
        match self {
            CodeLanguage::R => "Rscript",
            CodeLanguage::Python => "python3",
        }
    }

    /// File extension for scripts written in this language.
    pub fn extension(self) -> &'static str {
        match self {
            CodeLanguage::R => "R",
            CodeLanguage::Python => "py",
        }
    }
}

impl fmt::Display for CodeLanguage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for CodeLanguage {
    type Err = KernelError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "r" => Ok(CodeLanguage::R),
            "python" | "py" => Ok(CodeLanguage::Python),
            other => Err(KernelError::UnsupportedLanguage(other.to_string())),
        }
    }
}

/// Image format for plot/figure capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PlotFormat {
    /// High-resolution raster PNG image.
    #[default]
    Png,
    /// Lossless vector SVG graphic.
    Svg,
    /// Capture both PNG and SVG if generated.
    Both,
}

impl PlotFormat {
    /// Whether PNG capture is requested.
    pub fn captures_png(self) -> bool {
        matches!(self, PlotFormat::Png | PlotFormat::Both)
    }

    /// Whether SVG capture is requested.
    pub fn captures_svg(self) -> bool {
        matches!(self, PlotFormat::Svg | PlotFormat::Both)
    }
}

/// Structured tabular data captured from a script.
///
/// Cells are stored as text, so the value is exactly what the script's own
/// `as.character()` / `str()` conversion produced. Every row has exactly
/// `headers.len()` cells, padded with empty strings when the source was ragged.
///
/// When the source has meaningful row labels (R `row.names` other than the default
/// sequence, Python index other than `RangeIndex`), they are kept as an extra first
/// column with an empty header. That keeps statistical output such as `anova()`
/// usable, where the term names are row names rather than a column.
///
/// Capture is bounded: at most 16 tables per execution, 10,000 rows and 256 columns
/// per table, 4,096 characters per cell, and at most 250,000 total cells across all tables.
/// Anything past those limits is dropped, and a payload larger than 64 MiB is ignored entirely.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabularOutput {
    /// Column names in display order.
    pub headers: Vec<String>,
    /// Body rows, each with one text cell per header.
    pub rows: Vec<Vec<String>>,
}

/// Maximum number of tables kept from a single execution.
const MAX_TABLES: usize = 16;
/// Maximum number of body rows kept per table.
const MAX_TABLE_ROWS: usize = 10_000;
/// Maximum number of columns kept per table.
const MAX_TABLE_COLUMNS: usize = 256;
/// Maximum number of characters kept in a single cell.
const MAX_CELL_CHARS: usize = 4_096;
/// Maximum aggregate cell count across all tables before padding to prevent memory amplification.
const MAX_TOTAL_CELLS: usize = 250_000;
/// Maximum size of the table payload a script may hand back.
const MAX_TABLES_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Request to execute code in a specific language.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRequest {
    /// Target language (R or Python).
    pub language: CodeLanguage,
    /// Script code to execute.
    pub code: String,
    /// Maximum time in milliseconds before terminating the execution.
    pub timeout_ms: u64,
    /// Whether to attempt intercepting plots/figures generated by the script.
    pub capture_plots: bool,
    /// Desired format for captured plots (PNG, SVG, or Both).
    ///
    /// `prefer_svg` upgrades `Png` to `Svg` when enabled; an explicit `Both`
    /// is preserved. Use [`ExecutionRequest::effective_plot_format`] to resolve
    /// the format actually used for capture.
    #[serde(default)]
    pub plot_format: PlotFormat,
    /// Whether SVG should be preferred over the default PNG capture.
    #[serde(default)]
    pub prefer_svg: bool,
    /// Extra Python module search directories, prepended to Python's module search path.
    #[serde(default)]
    pub python_path: Vec<PathBuf>,
    /// Optional working directory.
    pub working_dir: Option<PathBuf>,
    /// Optional path to the `wc-kernel-supervisor` helper executable.
    #[serde(default)]
    pub supervisor_path: Option<PathBuf>,
}

impl ExecutionRequest {
    /// Create a new execution request with reasonable defaults (30s timeout, plot capture enabled, PNG format).
    pub fn new(language: CodeLanguage, code: impl Into<String>) -> Self {
        Self {
            language,
            code: code.into(),
            timeout_ms: 30_000,
            capture_plots: true,
            plot_format: PlotFormat::Png,
            prefer_svg: false,
            python_path: Vec::new(),
            working_dir: None,
            supervisor_path: None,
        }
    }

    /// Set a custom timeout in milliseconds.
    pub fn with_timeout(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Set whether plots should be captured.
    pub fn with_plot_capture(mut self, capture: bool) -> Self {
        self.capture_plots = capture;
        self
    }

    /// Set the desired plot format.
    pub fn with_plot_format(mut self, format: PlotFormat) -> Self {
        self.plot_format = format;
        self
    }

    /// Set SVG preference flag directly.
    pub fn with_prefer_svg(mut self, prefer: bool) -> Self {
        self.prefer_svg = prefer;
        self
    }

    /// Resolve the effective capture format from `plot_format` and `prefer_svg`.
    ///
    /// `prefer_svg` upgrades the default `Png` capture to `Svg`; an explicit
    /// `Both` keeps both formats. This is the single rule used by the builder
    /// and by deserialized requests.
    pub fn effective_plot_format(&self) -> PlotFormat {
        match (self.plot_format, self.prefer_svg) {
            (PlotFormat::Png, true) => PlotFormat::Svg,
            (format, _) => format,
        }
    }

    /// Add a Python module search directory.
    pub fn with_python_path(mut self, dir: impl Into<PathBuf>) -> Self {
        self.python_path.push(dir.into());
        self
    }

    /// Set an explicit working directory.
    pub fn with_working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Set an explicit path to the `wc-kernel-supervisor` helper.
    pub fn with_supervisor_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.supervisor_path = Some(path.into());
        self
    }
}

/// Result of executing a code chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// Process exit code, or None if terminated by signal/timeout.
    pub exit_code: Option<i32>,
    /// Whether the process exited cleanly with code 0.
    pub success: bool,
    /// True if execution was halted due to timeout limit.
    pub timed_out: bool,
    /// True if stdout or stderr exceeded the capture limit and was truncated.
    #[serde(default)]
    pub output_truncated: bool,
    /// PNG bytes of plot/chart if generated and captured.
    pub plot_png: Option<Vec<u8>>,
    /// Vector SVG XML string of plot/chart if generated and captured.
    pub plot_svg: Option<String>,
    /// Tabular values returned by the script (`data.frame` in R, `pandas.DataFrame` in Python).
    #[serde(default)]
    pub tables: Vec<TabularOutput>,
    /// Elapsed wall-clock execution time in milliseconds.
    pub duration_ms: u64,
}

/// Error encountered during kernel operations.
#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error("Language '{0}' is not supported")]
    UnsupportedLanguage(String),

    #[error("Interpreter for {language} ('{binary}') was not found in PATH")]
    InterpreterNotFound { language: CodeLanguage, binary: &'static str },

    #[error("The wc-kernel-supervisor helper was not found")]
    SupervisorNotFound,

    #[error("execution interrupted")]
    Interrupted,

    #[error("I/O error during execution: {0}")]
    Io(String),

    #[error("Failed to prepare script: {0}")]
    ScriptPreparation(String),

    #[error("Template '{template}' has no parameter named '{parameter}'")]
    UnknownTemplateParameter { template: String, parameter: String },

    #[error("Template '{template}' references an undefined placeholder '{placeholder}'")]
    UndefinedPlaceholder { template: String, placeholder: String },

    #[error("Template '{template}' has an unterminated placeholder")]
    UnterminatedPlaceholder { template: String },
}

/// Check if the interpreter for a given language is available on the host system.
pub fn is_language_available(language: CodeLanguage) -> bool {
    which_binary(language).is_some()
}

/// Search PATH for the language's binary.
pub fn which_binary(language: CodeLanguage) -> Option<PathBuf> {
    let name = language.binary_name();
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe_candidate = dir.join(format!("{name}.exe"));
            if exe_candidate.is_file() {
                return Some(exe_candidate);
            }
        }
    }
    None
}

/// Execute a code chunk and collect outputs.
pub fn execute(request: &ExecutionRequest) -> Result<ExecutionResult, KernelError> {
    execute_with_cancellation(request, &AtomicBool::new(false))
}

/// Execute a code chunk, stopping the supervisor when `cancelled` becomes true.
pub fn execute_with_cancellation(request: &ExecutionRequest, cancelled: &AtomicBool) -> Result<ExecutionResult, KernelError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(KernelError::Interrupted);
    }

    let binary = which_binary(request.language)
        .ok_or_else(|| KernelError::InterpreterNotFound { language: request.language, binary: request.language.binary_name() })?;
    // `--cwd` changes the supervisor's working directory, so a relative PATH
    // entry must be resolved against this process before it is handed over.
    let binary = std::path::absolute(&binary).map_err(|error| KernelError::Io(format!("resolve interpreter {}: {error}", binary.display())))?;

    let mut temp_dir = std::env::temp_dir();
    let mut created = false;
    for _ in 0..10 {
        let unique_id =
            format!("wordcraft_{}_{}_{}", std::process::id(), SCRIPT_COUNTER.fetch_add(1, Ordering::Relaxed), Instant::now().elapsed().as_nanos());
        let candidate = std::env::temp_dir().join(&unique_id);
        match create_scratch_dir(&candidate) {
            Ok(()) => {
                temp_dir = candidate;
                created = true;
                break;
            }
            Err(KernelError::Io(_)) => continue,
            Err(e) => return Err(e),
        }
    }
    if !created {
        return Err(KernelError::Io("failed to create exclusive scratch directory after 10 attempts".to_string()));
    }
    let _guard = TempDirGuard(&temp_dir);

    let plot_png_path = temp_dir.join("__wordcraft_plot__.png");
    let plot_svg_path = temp_dir.join("__wordcraft_plot__.svg");
    let tables_file = temp_dir.join("__wordcraft_tables__.json");
    let user_file = temp_dir.join(format!("user.{}", request.language.extension()));
    let script_file = temp_dir.join(format!("script.{}", request.language.extension()));
    let plot_format = request.effective_plot_format();

    fs::write(&user_file, request.code.as_bytes()).map_err(|e| KernelError::ScriptPreparation(format!("write {}: {e}", user_file.display())))?;

    let mut wrapped_code = prepare_wrapped_code(request.language, request.capture_plots, plot_format);
    if request.language == CodeLanguage::Python && !request.python_path.is_empty() {
        let paths = request.python_path.iter().map(|path| python_path_expression(path)).collect::<Result<Vec<_>, _>>()?;
        let paths_literal = format!("[{}]", paths.join(","));
        // Import the wrapper's standard-library dependencies before the user
        // search paths are inserted, so a sibling `ast.py` or `json.py` cannot
        // replace them. They stay cached for the wrapper's own imports.
        wrapped_code = format!(
            "(lambda _wc_sys, _wc_ast, _wc_json, _wc_paths: [_wc_sys.path.insert(0, _wc_path) for _wc_path in reversed(_wc_paths)])(__import__('sys'), __import__('ast'), __import__('json'), {paths_literal})\n{wrapped_code}"
        );
    }
    fs::write(&script_file, wrapped_code.as_bytes()).map_err(|e| KernelError::ScriptPreparation(format!("write {}: {e}", script_file.display())))?;

    let supervisor = find_supervisor_binary(request.supervisor_path.as_deref())?;
    let result_file = temp_dir.join("__wordcraft_result__.bin");
    let supervisor_stderr_path = temp_dir.join("__wordcraft_supervisor_stderr__.log");
    let mut cmd = Command::new(&supervisor);
    cmd.arg("--run").arg(&binary).arg(&script_file).arg(&result_file).arg(request.timeout_ms.to_string());
    if request.language == CodeLanguage::R {
        cmd.env("WORDCRAFT_USER_SCRIPT", &user_file)
            .env("WORDCRAFT_TABLES_FILE", &tables_file)
            .env("WORDCRAFT_PLOT_PNG", &plot_png_path)
            .env("WORDCRAFT_PLOT_SVG", &plot_svg_path);
    } else {
        cmd.arg(&user_file).arg(&tables_file).arg(&plot_png_path).arg(&plot_svg_path);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::from(
        create_private_file(&supervisor_stderr_path)
            .map_err(|error| KernelError::Io(format!("create supervisor diagnostic file {}: {error}", supervisor_stderr_path.display())))?,
    ));

    if let Some(ref dir) = request.working_dir {
        cmd.current_dir(dir);
    } else {
        cmd.current_dir(&temp_dir);
    }

    let mut child = cmd.group_spawn().map_err(|e| KernelError::Io(format!("spawn supervisor {}: {e}", supervisor.display())))?;
    let supervisor_timeout = Duration::from_millis(request.timeout_ms).saturating_add(Duration::from_secs(5));
    let status = match wait_for_supervisor(&mut child, supervisor_timeout, cancelled) {
        Ok(status) => status,
        Err(error) => {
            let cleanup = terminate_supervisor_process_tree(&mut child);
            if error.kind() == std::io::ErrorKind::Interrupted {
                cleanup.map_err(|cleanup| KernelError::Io(format!("execution interrupted; cleanup failed: {cleanup}")))?;
                return Err(KernelError::Interrupted);
            }
            let cleanup_message = cleanup.err().map(|cleanup| format!("; cleanup failed: {cleanup}")).unwrap_or_default();
            let stderr = read_supervisor_stderr(&supervisor_stderr_path)
                .map_err(|read_error| KernelError::Io(format!("supervisor failed: {error}{cleanup_message}; read diagnostics: {read_error}")))?;
            return Err(KernelError::Io(format!(
                "supervisor failed: {error}{cleanup_message}{}",
                if stderr.is_empty() { String::new() } else { format!(": {}", stderr.trim()) }
            )));
        }
    };
    let stderr = read_supervisor_stderr(&supervisor_stderr_path).map_err(|e| KernelError::Io(format!("read supervisor diagnostics: {e}")))?;
    if !status.success() {
        return Err(KernelError::Io(format!(
            "supervisor exited with {status}{}",
            if stderr.is_empty() { String::new() } else { format!(": {}", stderr.trim()) }
        )));
    }
    let outcome = read_supervisor_result(&result_file).map_err(|e| KernelError::Io(format!("read supervisor result: {e}")))?;
    let mut stdout = outcome.stdout;
    let mut stderr = outcome.stderr;
    let mut output_truncated = outcome.output_truncated;
    // Invalid bytes expand to three bytes each once they are decoded, so the
    // byte limit checked on the wire is not the limit of the returned strings.
    let stdout_truncated = truncate_output(&mut stdout, MAX_CAPTURE_BYTES);
    let stderr_truncated = truncate_output(&mut stderr, MAX_CAPTURE_BYTES);
    if stdout_truncated || stderr_truncated {
        output_truncated = true;
    }

    let mut plot_png = None;
    if request.capture_plots && plot_format.captures_png() && plot_png_path.is_file() {
        plot_png =
            fs::read(&plot_png_path).ok().filter(|bytes| bytes.len() > 64 && bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]));
    }

    let mut plot_svg = None;
    if request.capture_plots && plot_format.captures_svg() && plot_svg_path.is_file() {
        plot_svg = fs::read_to_string(&plot_svg_path).ok().filter(|content| is_complete_svg(content));
    }

    let tables = read_tables(&tables_file);

    drop(_guard);

    Ok(ExecutionResult {
        stdout,
        stderr,
        exit_code: outcome.exit_code,
        success: outcome.success,
        timed_out: outcome.timed_out,
        output_truncated,
        plot_png,
        plot_svg,
        tables,
        duration_ms: outcome.duration_ms,
    })
}

struct SupervisorResult {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    success: bool,
    timed_out: bool,
    output_truncated: bool,
    duration_ms: u64,
}

#[cfg(unix)]
fn python_path_expression(path: &Path) -> Result<String, KernelError> {
    use std::os::unix::ffi::OsStrExt;

    let hex = path.as_os_str().as_bytes().iter().map(|byte| format!("{byte:02x}")).collect::<Vec<_>>().join("");
    Ok(format!("__import__('os').fsdecode(bytes.fromhex('{hex}'))"))
}

#[cfg(windows)]
fn python_path_expression(path: &Path) -> Result<String, KernelError> {
    use std::os::windows::ffi::OsStrExt;

    let bytes = path.as_os_str().encode_wide().flat_map(u16::to_le_bytes).collect::<Vec<_>>();
    let hex = bytes.iter().map(|byte| format!("{byte:02x}")).collect::<Vec<_>>().join("");
    Ok(format!("bytes.fromhex('{hex}').decode('utf-16-le', 'surrogatepass')"))
}

#[cfg(not(any(unix, windows)))]
fn python_path_expression(path: &Path) -> Result<String, KernelError> {
    path.to_str()
        .ok_or_else(|| KernelError::Io("Python module path is not valid UTF-8".to_string()))
        .and_then(|path| serde_json::to_string(path).map_err(|error| KernelError::Io(format!("encode Python module path: {error}"))))
}

fn find_supervisor_binary(configured: Option<&Path>) -> Result<PathBuf, KernelError> {
    if let Some(path) = configured {
        return if path.is_file() {
            fs::canonicalize(path).map_err(|error| KernelError::Io(format!("resolve supervisor path: {error}")))
        } else {
            Err(KernelError::Io(format!("configured supervisor {} does not exist", path.display())))
        };
    }

    let executable_name = if cfg!(windows) { "wc-kernel-supervisor.exe" } else { "wc-kernel-supervisor" };

    if let Ok(current_exe) = std::env::current_exe()
        && let Some(directory) = current_exe.parent()
    {
        let candidate = directory.join(executable_name);
        if candidate.is_file() {
            return fs::canonicalize(candidate).map_err(|error| KernelError::Io(format!("resolve supervisor beside current executable: {error}")));
        }
    }

    #[cfg(test)]
    if let Ok(current_exe) = std::env::current_exe() {
        for directory in current_exe.ancestors() {
            let candidate = directory.join(executable_name);
            if candidate.is_file() {
                return fs::canonicalize(candidate).map_err(|error| KernelError::Io(format!("resolve supervisor from test target: {error}")));
            }
        }
    }

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(executable_name);
            if candidate.is_file() {
                return fs::canonicalize(candidate).map_err(|error| KernelError::Io(format!("resolve supervisor from PATH: {error}")));
            }
        }
    }

    Err(KernelError::SupervisorNotFound)
}

fn wait_for_supervisor(child: &mut GroupChild, timeout: Duration, cancelled: &AtomicBool) -> std::io::Result<std::process::ExitStatus> {
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "execution interrupted"));
        }
        if start.elapsed() >= timeout {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "supervisor exceeded its cleanup deadline"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn terminate_supervisor_process_tree(child: &mut GroupChild) -> std::io::Result<()> {
    // Ask the supervisor to stop the interpreter and its workers itself, so it
    // can run its own cleanup before it exits.
    #[cfg(unix)]
    {
        use nix::errno::Errno;
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;

        let signalled = i32::try_from(child.id()).is_ok_and(|pid| matches!(kill(Pid::from_raw(pid), Signal::SIGTERM), Ok(()) | Err(Errno::ESRCH)));
        if signalled {
            let deadline = Instant::now() + SUPERVISOR_CLEANUP_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => return Ok(()),
                    Ok(None) => {}
                    Err(error) => return Err(error),
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    force_terminate_supervisor_process_group(child)
}

/// Hard fallback: the supervisor ignored the cancellation request.
///
/// Only the group this process created is signalled. A recorded interpreter
/// PID cannot prove that its group is still the interpreter's, so the
/// interpreter itself is left to the supervisor's cancellation handler.
fn force_terminate_supervisor_process_group(child: &mut GroupChild) -> std::io::Result<()> {
    match child.kill() {
        Ok(()) => child.wait().map(|_| ()),
        Err(kill_error) => match child.try_wait() {
            Ok(Some(_)) => child.wait().map(|_| ()),
            Ok(None) => Err(kill_error),
            Err(wait_error) => Err(std::io::Error::other(format!("kill supervisor: {kill_error}; check process status: {wait_error}"))),
        },
    }
}

fn create_private_file(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn read_supervisor_stderr(path: &Path) -> std::io::Result<String> {
    let stderr = fs::File::open(path)?;
    let mut bytes = Vec::new();
    stderr.take(4096).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_supervisor_result(path: &Path) -> std::io::Result<SupervisorResult> {
    const HEADER_LEN: usize = 25;
    const MAGIC: &[u8; 4] = b"WCK1";

    let mut file = fs::File::open(path)?;
    let mut header = [0_u8; HEADER_LEN];
    file.read_exact(&mut header)?;
    if &header[..4] != MAGIC {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid supervisor result header"));
    }

    let flags = header[4];
    if flags & !0b11 != 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid supervisor result flags"));
    }
    let raw_exit_code =
        i32::from_le_bytes(header[5..9].try_into().map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid exit code"))?);
    let duration_ms =
        u64::from_le_bytes(header[9..17].try_into().map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid duration"))?);
    let stdout_len =
        u32::from_le_bytes(header[17..21].try_into().map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid stdout length"))?)
            as usize;
    let stderr_len =
        u32::from_le_bytes(header[21..25].try_into().map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid stderr length"))?)
            as usize;

    if stdout_len > MAX_CAPTURE_BYTES || stderr_len > MAX_CAPTURE_BYTES {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "supervisor result exceeds the output limit"));
    }

    let mut stdout = vec![0_u8; stdout_len];
    let mut stderr = vec![0_u8; stderr_len];
    file.read_exact(&mut stdout)?;
    file.read_exact(&mut stderr)?;
    let mut trailing = [0_u8; 1];
    if file.read(&mut trailing)? != 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "supervisor result has trailing data"));
    }

    let exit_code = (raw_exit_code != i32::MIN).then_some(raw_exit_code);
    let timed_out = flags & 1 != 0;
    Ok(SupervisorResult {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        exit_code,
        success: exit_code == Some(0) && !timed_out,
        timed_out,
        output_truncated: flags & 2 != 0,
        duration_ms,
    })
}

/// Whether `content` is a non-trivial and complete SVG document.
///
/// A truncated file can contain `<svg` and exceed the size threshold, so the
/// closing root element is required to avoid returning partial output.
fn is_complete_svg(content: &str) -> bool {
    content.len() > 64 && content.contains("<svg") && content.trim_end().ends_with("</svg>")
}

/// Shorten `text` to at most `limit` bytes without splitting a character.
fn truncate_output(text: &mut String, limit: usize) -> bool {
    if text.len() <= limit {
        return false;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
}

/// Create an owner-only directory leaf exclusively.
fn create_scratch_dir(path: &Path) -> Result<(), KernelError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(path).map_err(|e| KernelError::Io(format!("create scratch dir {}: {e}", path.display())))
    }
    #[cfg(not(unix))]
    {
        fs::DirBuilder::new().create(path).map_err(|e| KernelError::Io(format!("create scratch dir {}: {e}", path.display())))
    }
}

/// RAII guard that removes a scratch directory and all its contents on drop.
struct TempDirGuard<'a>(&'a Path);

impl Drop for TempDirGuard<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.0);
    }
}

/// Substitute all placeholders in `template` in a single pass so that inserted values
/// are never re-scanned for marker occurrences.
fn substitute_markers(template: &str, replacements: &[(&str, &str)]) -> String {
    let mut result = String::with_capacity(template.len() + 1024);
    let mut remainder = template;
    while !remainder.is_empty() {
        let mut earliest: Option<(usize, &str, &str)> = None;
        for &(marker, value) in replacements {
            if let Some(pos) = remainder.find(marker) {
                match earliest {
                    None => earliest = Some((pos, marker, value)),
                    Some((earliest_pos, _, _)) if pos < earliest_pos => {
                        earliest = Some((pos, marker, value));
                    }
                    _ => {}
                }
            }
        }
        if let Some((pos, marker, value)) = earliest {
            result.push_str(&remainder[..pos]);
            result.push_str(value);
            remainder = &remainder[pos + marker.len()..];
        } else {
            result.push_str(remainder);
            break;
        }
    }
    result
}

/// Build the script that wraps the user's code to capture plots and tabular data.
///
/// The user's code is written to its own file and evaluated by the wrapper, so the
/// last visible value (`data.frame` or matrix in R, final expression in Python) can be
/// inspected without rewriting or escaping the original source.
fn prepare_wrapped_code(lang: CodeLanguage, capture_plots: bool, format: PlotFormat) -> String {
    let (setup, finally) = if capture_plots {
        match lang {
            CodeLanguage::R => match format {
                PlotFormat::Png => (
                    r#"  has_plot <- FALSE
  setHook("plot.new", function() { has_plot <<- TRUE; assign(".wc_has_plot", TRUE, envir = .GlobalEnv) })
  # ggplot2 and lattice draw through grid, which never fires the plot.new hook.
  setHook("grid.newpage", function() { has_plot <<- TRUE; assign(".wc_has_plot", TRUE, envir = .GlobalEnv) })
  png(plot_png, width = 1600, height = 1200, res = 200)
"#
                    .to_string(),
                    r#"    while (dev.cur() != 1) dev.off()
    if (!isTRUE(has_plot) && (!exists(".wc_has_plot", envir = .GlobalEnv) || !isTRUE(get(".wc_has_plot", envir = .GlobalEnv)))) {
      if (file.exists(plot_png)) file.remove(plot_png)
    }
"#
                    .to_string(),
                ),
                PlotFormat::Svg => (
                    r#"  has_plot <- FALSE
  setHook("plot.new", function() { has_plot <<- TRUE; assign(".wc_has_plot", TRUE, envir = .GlobalEnv) })
  # ggplot2 and lattice draw through grid, which never fires the plot.new hook.
  setHook("grid.newpage", function() { has_plot <<- TRUE; assign(".wc_has_plot", TRUE, envir = .GlobalEnv) })
  svg(plot_svg, width = 8, height = 6)
"#
                    .to_string(),
                    r#"    while (dev.cur() != 1) dev.off()
    if (!isTRUE(has_plot) && (!exists(".wc_has_plot", envir = .GlobalEnv) || !isTRUE(get(".wc_has_plot", envir = .GlobalEnv)))) {
      if (file.exists(plot_svg)) file.remove(plot_svg)
    }
"#
                    .to_string(),
                ),
                PlotFormat::Both => (
                    r#"  has_plot <- FALSE
  png(plot_png, width = 1600, height = 1200, res = 200)
  capture_device <- as.integer(dev.cur())

  wrapper <- local({
    closed <- FALSE
    driver_has_plot <- FALSE
    orig <- grDevices::dev.off
    cap_dev <- capture_device
    fn <- function(which = grDevices::dev.cur()) {
      target <- if (missing(which)) as.integer(grDevices::dev.cur()) else as.integer(which)[1]
      if (!is.na(target) && target == cap_dev) {
        closed <<- TRUE
      }
      orig(which)
    }
    attr(fn, "driver") <- list(
      orig = orig,
      cap_dev = cap_dev,
      is_closed = function() closed,
      set_has_plot = function() { driver_has_plot <<- TRUE },
      has_plot = function() driver_has_plot
    )
    fn
  })

  unlockBinding("dev.off", as.environment("package:grDevices"))
  assign("dev.off", wrapper, envir = as.environment("package:grDevices"))
  lockBinding("dev.off", as.environment("package:grDevices"))

  unlockBinding("dev.off", asNamespace("grDevices"))
  assign("dev.off", wrapper, envir = asNamespace("grDevices"))
  lockBinding("dev.off", asNamespace("grDevices"))

  hook <- function() {
    has_plot <<- TRUE
    assign(".wc_has_plot", TRUE, envir = .GlobalEnv)
    drv <- attr(tryCatch(as.environment("package:grDevices")$dev.off, error = function(e) NULL), "driver")
    if (!is.null(drv)) drv$set_has_plot()
  }
  setHook("plot.new", hook)
  # ggplot2 and lattice draw through grid, which never fires the plot.new hook.
  setHook("grid.newpage", hook)
  dev.control("enable")
"#
                    .to_string(),
                    r#"    drv <- attr(tryCatch(as.environment("package:grDevices")$dev.off, error = function(e) NULL), "driver")
    if (!is.null(drv)) {
      orig <- drv$orig
      unlockBinding("dev.off", as.environment("package:grDevices"))
      assign("dev.off", orig, envir = as.environment("package:grDevices"))
      lockBinding("dev.off", as.environment("package:grDevices"))
      unlockBinding("dev.off", asNamespace("grDevices"))
      assign("dev.off", orig, envir = asNamespace("grDevices"))
      lockBinding("dev.off", asNamespace("grDevices"))
    }

    plot_drawn <- isTRUE(has_plot) ||
                  (!is.null(drv) && isTRUE(drv$has_plot())) ||
                  (exists(".wc_has_plot", envir = .GlobalEnv) && isTRUE(get(".wc_has_plot", envir = .GlobalEnv)))
    is_closed <- if (!is.null(drv)) isTRUE(drv$is_closed()) else FALSE
    cap_dev <- if (!is.null(drv)) drv$cap_dev else capture_device

    if (plot_drawn && !is_closed) {
      open_devs <- dev.list()
      if (!is.null(open_devs) && cap_dev %in% open_devs) {
        dev.set(cap_dev)
        dev.copy(svg, plot_svg, width = 8, height = 6)
        if (dev.cur() != 1) dev.off()
      }
    }
    while (dev.cur() != 1) dev.off()
    if (!plot_drawn) {
      if (file.exists(plot_png)) file.remove(plot_png)
      if (file.exists(plot_svg)) file.remove(plot_svg)
    }
"#
                    .to_string(),
                ),
            },
            CodeLanguage::Python => {
                let save_png = if format.captures_png() { r#"plt.savefig(_wc_plot_png_path, dpi=200, bbox_inches="tight")"# } else { "" };
                let save_svg = if format.captures_svg() { r#"plt.savefig(_wc_plot_svg_path, format="svg", bbox_inches="tight")"# } else { "" };
                (
                    r#"_wc_has_plt = False
try:
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    _wc_has_plt = True
except Exception:
    pass
"#
                    .to_string(),
                    format!(
                        r#"    if _wc_has_plt and plt is not None:
        try:
            if plt.get_fignums():
                {save_png}
                {save_svg}
                plt.close("all")
        except Exception:
            pass
"#
                    ),
                )
            }
        }
    } else {
        (String::new(), String::new())
    };

    let replacements = [("__PLOT_SETUP__", setup.as_str()), ("__PLOT_FINALLY__", finally.as_str())];

    match lang {
        CodeLanguage::R => substitute_markers(R_TEMPLATE, &replacements),
        CodeLanguage::Python => substitute_markers(PY_TEMPLATE, &replacements),
    }
}

/// Read the table payload written by the script, clamping it to bounded sizes.
///
/// Any unreadable, oversized or malformed payload yields no tables instead of an error.
fn read_tables(path: &Path) -> Vec<TabularOutput> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return Vec::new(),
    };
    if !metadata.is_file() || metadata.len() > MAX_TABLES_FILE_BYTES {
        return Vec::new();
    }
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };
    read_tables_from_reader(file, MAX_TABLES_FILE_BYTES, MAX_TOTAL_CELLS)
}

/// Read and validate tabular data from a reader under strict byte and cell budgets.
fn read_tables_from_reader<R: Read>(reader: R, max_bytes: u64, max_cells: usize) -> Vec<TabularOutput> {
    let mut raw = Vec::new();
    let mut limited = reader.take(max_bytes.saturating_add(1));
    if limited.read_to_end(&mut raw).is_err() || raw.len() > max_bytes as usize {
        return Vec::new();
    }
    let payload: RawTables = match serde_json::from_slice(&raw) {
        Ok(payload) => payload,
        Err(_) => return Vec::new(),
    };

    let tables: Vec<RawTable> = payload.tables.into_iter().take(MAX_TABLES).collect();
    let mut total_projected_cells: usize = 0;
    for table in &tables {
        let width = table.headers.len().min(MAX_TABLE_COLUMNS);
        let row_count = table.rows.len().min(MAX_TABLE_ROWS);
        match width.checked_mul(row_count) {
            Some(cells) => {
                total_projected_cells = match total_projected_cells.checked_add(cells) {
                    Some(sum) => sum,
                    None => return Vec::new(),
                };
            }
            None => return Vec::new(),
        }
    }
    if total_projected_cells > max_cells {
        return Vec::new();
    }

    tables.into_iter().filter_map(normalize_table).collect()
}

/// Clamp a table to the documented limits and make every row as wide as the headers.
fn normalize_table(table: RawTable) -> Option<TabularOutput> {
    let mut headers: Vec<String> = table.headers.into_iter().map(truncate_cell).collect();
    if headers.is_empty() {
        return None;
    }
    headers.truncate(MAX_TABLE_COLUMNS);
    let width = headers.len();

    let rows = table
        .rows
        .into_iter()
        .take(MAX_TABLE_ROWS)
        .map(|row| {
            let mut row: Vec<String> = row.into_iter().map(truncate_cell).collect();
            row.truncate(width);
            row.resize(width, String::new());
            row
        })
        .collect();

    Some(TabularOutput { headers, rows })
}

/// Keep at most [`MAX_CELL_CHARS`] characters, never splitting a UTF-8 sequence.
fn truncate_cell(text: String) -> String {
    if text.chars().count() <= MAX_CELL_CHARS {
        return text;
    }
    text.chars().take(MAX_CELL_CHARS).collect()
}

#[derive(Deserialize)]
struct RawTables {
    #[serde(default)]
    tables: Vec<RawTable>,
}

#[derive(Deserialize)]
struct RawTable {
    #[serde(default)]
    headers: Vec<String>,
    #[serde(default)]
    rows: Vec<Vec<String>>,
}

const R_TEMPLATE: &str = r##"options(warn = 1)
local({
  user_script <- Sys.getenv("WORDCRAFT_USER_SCRIPT")
  plot_png <- Sys.getenv("WORDCRAFT_PLOT_PNG")
  plot_svg <- Sys.getenv("WORDCRAFT_PLOT_SVG")

  driver <- new.env(parent = emptyenv())
  driver$tables_file <- Sys.getenv("WORDCRAFT_TABLES_FILE")
  driver$tables <- list()

  json_escape <- function(value) {
    text <- as.character(value)
    if (length(text) == 0L || is.na(text[1L])) {
      return("\"\"")
    }
    chars <- strsplit(text[1L], "", fixed = TRUE)[[1L]]
    if (length(chars) == 0L) {
      return("\"\"")
    }
    escaped <- vapply(chars, function(ch) {
      code <- suppressWarnings(utf8ToInt(ch))
      if (is.na(code)) {
        ""
      } else if (ch == "\"") {
        "\\\""
      } else if (ch == "\\") {
        "\\\\"
      } else if (ch == "\b") {
        "\\b"
      } else if (ch == "\f") {
        "\\f"
      } else if (ch == "\n") {
        "\\n"
      } else if (ch == "\r") {
        "\\r"
      } else if (ch == "\t") {
        "\\t"
      } else if (code < 32L || code == 127L) {
        sprintf("\\u%04x", code)
      } else {
        ch
      }
    }, character(1), USE.NAMES = FALSE)
    paste0("\"", paste0(escaped, collapse = ""), "\"")
  }

  cell <- function(value, depth = 0L) {
    if (is.null(value) || length(value) == 0L || depth > 8L) {
      return("")
    }
    if (is.list(value)) {
      return(cell(value[[1L]], depth + 1L))
    }
    if (length(value) > 1L) {
      value <- value[1L]
    }
    if (is.na(value)) {
      return("NA")
    }
    if (is.factor(value)) {
      text <- tryCatch(as.character(value), error = function(e) "")
      if (length(text) == 0L || is.na(text[1L])) {
        return("NA")
      }
      return(substr(text[1L], 1L, 4096L))
    }
    text <- tryCatch(as.character(value), error = function(e) "")
    if (length(text) == 0L || is.na(text[1L])) {
      return("")
    }
    substr(text[1L], 1L, 4096L)
  }

  capture <- function(value) {
    if (length(driver$tables) >= 16L) {
      return(invisible(FALSE))
    }
    if (!is.data.frame(value) && !is.matrix(value)) {
      return(invisible(FALSE))
    }
    count <- min(nrow(value), 10000L)

    has_labels <- FALSE
    labels <- NULL
    if (is.matrix(value)) {
      rn_mat <- rownames(value)
      if (!is.null(rn_mat)) {
        retained_labels <- as.character(rn_mat[seq_len(count)])
        has_labels <- !identical(retained_labels, as.character(seq_len(count)))
        labels <- retained_labels
      }
    } else {
      if (.row_names_info(value, 1L) > 0L) {
        rn_df <- .row_names_info(value, 0L)
        is_compact <- is.integer(rn_df) && length(rn_df) == 2L && is.na(rn_df[1L])
        if (!is_compact) {
          retained_labels <- as.character(rn_df[seq_len(count)])
          has_labels <- !identical(retained_labels, as.character(seq_len(count)))
          labels <- retained_labels
        }
      }
    }

    max_cols <- if (has_labels) 255L else 256L
    cols_count <- min(ncol(value), max_cols)
    if (cols_count == 0L && !has_labels) {
      return(invisible(FALSE))
    }

    raw_headers <- if (is.matrix(value)) colnames(value) else names(value)
    df <- if (is.matrix(value)) {
      base::as.data.frame(value[seq_len(count), seq_len(cols_count), drop = FALSE])
    } else {
      base::as.data.frame(value)
    }
    if (is.null(raw_headers) || length(raw_headers) < cols_count) {
      raw_headers <- names(df)
    }
    if (is.null(raw_headers)) {
      raw_headers <- rep("", cols_count)
    }
    headers <- raw_headers[seq_len(cols_count)]
    headers <- vapply(headers, function(h) substr(as.character(h), 1L, 4096L), character(1), USE.NAMES = FALSE)

    if (has_labels) {
      headers <- c("", headers)
    }
    rows <- vector("list", count)
    df_sub <- df[seq_len(cols_count)]
    for (index in seq_len(count)) {
      cells <- if (cols_count > 0L) {
        vapply(df_sub, function(column) cell(column[index]), character(1), USE.NAMES = FALSE)
      } else {
        character(0)
      }
      if (has_labels) {
        cells <- c(cell(labels[index]), cells)
      }
      rows[[index]] <- cells
    }
    driver$tables[[length(driver$tables) + 1L]] <- list(headers = as.list(headers), rows = rows)
    invisible(TRUE)
  }

  write_tables <- function() {
    if (length(driver$tables) == 0L) {
      return(invisible(NULL))
    }
    max_budget <- if (exists(".wc_budget", envir = globalenv())) get(".wc_budget", envir = globalenv()) else 65011712L
    tryCatch({
      con <- base::file(driver$tables_file, open = "wt", encoding = "UTF-8")
      tryCatch({
        cat("{\"tables\":[", file = con, sep = "")
        bytes_written <- 11L
        table_written <- 0L
        closing_reserve <- 4L

        for (table in driver$tables) {
          headers <- vapply(table$headers, json_escape, character(1), USE.NAMES = FALSE)
          h_json <- paste0("[", paste0(headers, collapse = ","), "]")
          prefix <- paste0(if (table_written > 0L) "," else "", "{\"headers\":", h_json, ",\"rows\":[")
          prefix_bytes <- nchar(prefix, type = "bytes")
          if (bytes_written + prefix_bytes + closing_reserve > max_budget) {
            break
          }
          cat(prefix, file = con, sep = "")
          bytes_written <- bytes_written + prefix_bytes
          table_written <- table_written + 1L
          row_written <- 0L

          for (row in table$rows) {
            cells <- vapply(row, json_escape, character(1), USE.NAMES = FALSE)
            r_json <- paste0("[", paste0(cells, collapse = ","), "]")
            r_str <- paste0(if (row_written > 0L) "," else "", r_json)
            r_bytes <- nchar(r_str, type = "bytes")
            if (bytes_written + r_bytes + closing_reserve > max_budget) {
              break
            }
            cat(r_str, file = con, sep = "")
            bytes_written <- bytes_written + r_bytes
            row_written <- row_written + 1L
          }
          cat("]}", file = con, sep = "")
          bytes_written <- bytes_written + 2L
          if (bytes_written + 2L > max_budget) {
            break
          }
        }
        cat("]}\n", file = con, sep = "")
      }, finally = {
        try(base::close(con), silent = TRUE)
      })
    }, error = function(e) invisible(NULL))
    invisible(NULL)
  }

  assign("wc_table", function(value) {
    if (!is.data.frame(value) && !is.matrix(value)) {
      stop("wc_table() expects a data.frame or matrix")
    }
    capture(value)
    invisible(value)
  }, envir = globalenv())

  expressions <- parse(file = user_script, encoding = "UTF-8", keep.source = FALSE)

__PLOT_SETUP__

  tryCatch({
    for (expr in expressions) {
      result <- base::withVisible(base::eval(expr, envir = globalenv()))
      base::assign(".Last.value", result$value, envir = base::.GlobalEnv)
      if (isTRUE(result$visible)) {
        if (!((is.data.frame(result$value) || is.matrix(result$value)) &&
              isTRUE(capture(result$value)))) {
          print(result$value)
        }
      }
    }
  }, finally = {
    write_tables()
__PLOT_FINALLY__  })
})
"##;

const PY_TEMPLATE: &str = r##"import ast
import json as _wc_json
import sys as _wc_sys

_wc_user_path = _wc_sys.argv[1]
_wc_tables_path = _wc_sys.argv[2]
_wc_plot_png_path = _wc_sys.argv[3]
_wc_plot_svg_path = _wc_sys.argv[4]
_wc_sys.argv = [_wc_user_path]
_wc_globals = globals()
_wc_globals["__file__"] = _wc_user_path
_wc_tables = []

try:
    import pandas as _wc_pandas
except Exception:
    _wc_pandas = None

__PLOT_SETUP__

def _wc_cell(value):
    if value is None:
        return "NA"
    if _wc_pandas is not None:
        try:
            is_scalar = getattr(_wc_pandas.api.types, "is_scalar", None)
            if is_scalar(value) if is_scalar else (not hasattr(value, "__len__") or isinstance(value, (str, bytes))):
                if _wc_pandas.isna(value):
                    return "NA"
        except (TypeError, ValueError):
            pass
    text = str(value).encode("utf-8", "replace").decode("utf-8")
    return text[:4096] if len(text) > 4096 else text


def _wc_capture(value):
    if len(_wc_tables) >= 16:
        return None
    if _wc_pandas is None or not isinstance(value, _wc_pandas.DataFrame):
        return None
    try:
        has_labels = not (
            isinstance(value.index, _wc_pandas.RangeIndex)
            and value.index.equals(_wc_pandas.RangeIndex(len(value)))
        )
        max_cols = 255 if has_labels else 256
        sliced = value.iloc[:10000, :max_cols]
        raw_headers = [_wc_cell(col) for col in sliced.columns]
        headers = ([""] + raw_headers) if has_labels else raw_headers
        if not headers:
            return None
        rows = []
        if has_labels:
            for row in sliced.itertuples(index=True, name=None):
                rows.append([_wc_cell(c) for c in row])
        else:
            for row in sliced.itertuples(index=False, name=None):
                rows.append([_wc_cell(c) for c in row])
        _wc_tables.append({"headers": headers, "rows": rows})
    except Exception:
        return None
    return None


def wc_table(value):
    if _wc_pandas is None or not isinstance(value, _wc_pandas.DataFrame):
        raise TypeError("wc_table() expects a pandas.DataFrame")
    _wc_capture(value)
    return None


def _wc_write_tables():
    if not _wc_tables:
        return
    max_budget = _wc_globals.get("__wc_budget__", 62 * 1024 * 1024)
    try:
        with open(_wc_tables_path, "w", encoding="utf-8") as handle:
            handle.write('{"tables":[')
            bytes_written = 11
            table_written = 0
            closing_reserve = 4
            for table in _wc_tables:
                headers = table.get("headers", [])
                rows = table.get("rows", [])
                h_json = _wc_json.dumps(headers, ensure_ascii=False)
                prefix = ("," if table_written > 0 else "") + '{"headers":' + h_json + ',"rows":['
                prefix_bytes = len(prefix.encode("utf-8"))
                if bytes_written + prefix_bytes + closing_reserve > max_budget:
                    break
                handle.write(prefix)
                bytes_written += prefix_bytes
                table_written += 1
                row_written = 0
                for row in rows:
                    r_json = _wc_json.dumps(row, ensure_ascii=False)
                    r_str = ("," if row_written > 0 else "") + r_json
                    r_bytes = len(r_str.encode("utf-8"))
                    if bytes_written + r_bytes + closing_reserve > max_budget:
                        break
                    handle.write(r_str)
                    bytes_written += r_bytes
                    row_written += 1
                handle.write("]}")
                bytes_written += 2
                if bytes_written + 2 > max_budget:
                    break
            handle.write("]}\n")
    except Exception:
        pass


def _wc_run_user_code():
    with open(_wc_user_path, "rb") as handle:
        source = handle.read()
    tree = ast.parse(source, filename=_wc_user_path)
    returns_last = bool(tree.body) and isinstance(tree.body[-1], ast.Expr)
    if returns_last:
        tree.body[-1] = ast.Assign(
            targets=[ast.Name(id="__wc_last_value__", ctx=ast.Store())],
            value=tree.body[-1].value,
        )
        ast.fix_missing_locations(tree)
    _wc_globals["__file__"] = _wc_user_path
    exec(compile(tree, _wc_user_path, "exec"), _wc_globals)
    return returns_last


try:
    if _wc_run_user_code():
        _wc_capture(_wc_globals.get("__wc_last_value__"))
finally:
    _wc_write_tables()
__PLOT_FINALLY__"##;

#[cfg(test)]
mod template_tests;

#[cfg(test)]
mod tests;
