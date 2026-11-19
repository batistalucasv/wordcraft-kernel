//! Persistent interpreter sessions.
//!
//! [`SessionManager`] keeps one long-lived R or Python process per
//! `session_id`, so a value created by one code chunk is still in memory when a
//! later chunk runs. Each process is a background daemon driven through its own
//! stdin/stdout pipes; user output goes to files, never to the pipe, so a chunk
//! that prints or fails cannot corrupt the protocol.
//!
//! # Wire protocol
//!
//! The host writes one length-prefixed request on the daemon's stdin:
//!
//! ```text
//! RUN <seq> <capture:0|1> <format:png|svg|both|none> <cwd_bytes_len> <code_bytes_len>\n
//! <cwd_bytes>
//! <code_bytes>
//! ```
//!
//! Because the exact byte lengths of the working directory and code payload are specified
//! in the header, arbitrary user code (including lines matching protocol words, multiline
//! strings, or non-ASCII characters) can never collide with or terminate framing.
//!
//! The daemon evaluates the code in its execution environment, writes
//! `out-<seq>.txt`, `err-<seq>.txt`, `status-<seq>.txt` and, when a plot was
//! drawn, `plot-<seq>.png` / `plot-<seq>.svg` in its private temporary
//! directory, then prints `END <seq>\n` on its dedicated protocol stdout descriptor.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::{
    CodeLanguage, ExecutionRequest, ExecutionResult, KernelError, PlotFormat, which_binary,
};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Default time an unused session is kept alive before it is reaped.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Default maximum number of concurrent active interpreter sessions.
pub const DEFAULT_MAX_SESSIONS: usize = 16;

/// Maximum bytes kept from each captured output stream (stdout and stderr) per chunk.
pub const MAX_STREAM_BYTES: usize = 10 * 1024 * 1024;

/// Maximum bytes kept from each captured plot artifact (PNG and SVG) per chunk.
pub const MAX_PLOT_BYTES: usize = 20 * 1024 * 1024;

/// Snapshot of one live session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Identifier supplied by the caller.
    pub session_id: String,
    /// Language the daemon was started for.
    pub language: CodeLanguage,
    /// Milliseconds since the last chunk finished on this session.
    pub idle_ms: u64,
    /// Operating-system process id, when the daemon is still alive.
    pub pid: Option<u32>,
}

/// Owns the R and Python daemons of one document or application.
///
/// Sessions are keyed by an arbitrary caller-chosen id. The first
/// [`execute`](Self::execute) for an id starts a daemon; later calls reuse it,
/// so variables, functions, imports and the working directory persist. Sessions
/// of different languages never share an id.
///
/// Structured tabular data capture ([`ExecutionResult::tables`]) is supported in
/// one-shot [`execute`](crate::execute); persistent sessions return `tables` as empty.
///
/// Idle sessions are reaped lazily: every call to [`execute`](Self::execute)
/// first drops sessions whose last use is older than
/// [`idle_timeout`](Self::idle_timeout), and hosts can call
/// [`reap_idle_sessions`](Self::reap_idle_sessions) on their own timer.
#[derive(Debug)]
pub struct SessionManager {
    sessions: HashMap<String, Session>,
    idle_timeout: Duration,
    max_sessions: usize,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    /// Create a manager with [`DEFAULT_IDLE_TIMEOUT`], [`DEFAULT_MAX_SESSIONS`], and no sessions.
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            max_sessions: DEFAULT_MAX_SESSIONS,
        }
    }

    /// Set how long an unused session is kept alive. A zero timeout disables
    /// automatic reaping.
    pub fn with_idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// Set the maximum number of concurrent active sessions allowed.
    pub fn with_max_sessions(mut self, max: usize) -> Self {
        self.max_sessions = max;
        self
    }

    /// Current idle timeout.
    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    /// Current maximum active sessions.
    pub fn max_sessions(&self) -> usize {
        self.max_sessions
    }

    /// Change the idle timeout of an existing manager.
    pub fn set_idle_timeout(&mut self, timeout: Duration) {
        self.idle_timeout = timeout;
    }

    /// Change the maximum active sessions of an existing manager.
    pub fn set_max_sessions(&mut self, max: usize) {
        self.max_sessions = max;
    }

    /// Run a chunk in the session called `session_id`, creating the daemon on
    /// first use.
    ///
    /// A variable defined by an earlier call with the same id is visible here.
    /// When a chunk exceeds [`ExecutionRequest::timeout_ms`] the daemon is
    /// killed and dropped, and the result reports `timed_out`.
    pub fn execute(
        &mut self,
        session_id: &str,
        request: &ExecutionRequest,
    ) -> Result<ExecutionResult, KernelError> {
        let timeout = Duration::from_millis(request.timeout_ms);
        if Instant::now().checked_add(timeout).is_none() {
            return Err(KernelError::SessionProtocol(
                "timeout exceeds maximum representable duration".to_string(),
            ));
        }

        // 1. Run idle reaping first so expired sessions are cleaned up even if request validation fails later.
        self.reap_idle_sessions();
        self.sessions.retain(|_, s| s.is_alive());

        // 2. Pre-execution validation: reject invalid requests before altering session state.
        let resolved_cwd = match request.working_dir.as_ref() {
            Some(dir) => {
                let text = match dir.to_str() {
                    Some(s) => s,
                    None => {
                        return Err(KernelError::SessionProtocol(
                            "the working directory path must be valid UTF-8".to_string(),
                        ));
                    }
                };
                if text.contains('\n') || text.contains('\r') {
                    return Err(KernelError::SessionProtocol(
                        "the working directory path contains a line break".to_string(),
                    ));
                }
                if !dir.exists() {
                    return Err(KernelError::Io(format!(
                        "working directory does not exist: {}",
                        dir.display()
                    )));
                }
                let abs = if dir.is_relative() {
                    std::env::current_dir()
                        .map(|cur| cur.join(dir))
                        .unwrap_or_else(|_| dir.clone())
                } else {
                    dir.clone()
                };
                if abs.to_str().is_none() {
                    return Err(KernelError::SessionProtocol(
                        "the working directory path must be valid UTF-8".to_string(),
                    ));
                }
                Some(abs)
            }
            None => None,
        };

        // 3. Check language compatibility and determine if a fresh daemon must be spawned.
        let needs_spawn = match self.sessions.get(session_id) {
            Some(session) => {
                if session.language != request.language {
                    return Err(KernelError::SessionLanguageMismatch {
                        session_id: session_id.to_string(),
                        existing: session.language,
                        requested: request.language,
                    });
                }
                !session.is_alive()
            }
            None => true,
        };

        if needs_spawn {
            if self.sessions.len() >= self.max_sessions && !self.sessions.contains_key(session_id) {
                return Err(KernelError::SessionCapacityExceeded {
                    max_sessions: self.max_sessions,
                });
            }
            self.sessions.remove(session_id);
            let session = Session::spawn(session_id, request.language)?;
            self.sessions.insert(session_id.to_string(), session);
        }

        let session = match self.sessions.get_mut(session_id) {
            Some(s) => s,
            None => {
                return Err(KernelError::SessionTerminated {
                    session_id: session_id.to_string(),
                    detail: "the session could not be started".to_string(),
                });
            }
        };

        let result = session.run(request, resolved_cwd.as_deref());

        let discard = match &result {
            Ok(res) => res.timed_out,
            Err(_) => true,
        };
        if discard {
            self.sessions.remove(session_id);
        }
        result
    }

    /// Drop the session's interpreter and start a fresh one.
    ///
    /// Memory, imports and the working directory return to their initial state.
    /// Restarting an id that has no session is a no-op.
    pub fn restart_session(&mut self, session_id: &str) -> Result<(), KernelError> {
        let Some(mut previous) = self.sessions.remove(session_id) else {
            return Ok(());
        };
        let language = previous.language;
        previous.kill();
        let session = Session::spawn(session_id, language)?;
        self.sessions.insert(session_id.to_string(), session);
        Ok(())
    }

    /// Kill a session and free its process. Returns whether it existed.
    pub fn close_session(&mut self, session_id: &str) -> bool {
        match self.sessions.remove(session_id) {
            Some(mut session) => {
                session.kill();
                true
            }
            None => false,
        }
    }

    /// Drop every session that has been idle for at least the current timeout.
    /// Returns how many were reaped. A zero timeout disables reaping.
    pub fn reap_idle_sessions(&mut self) -> usize {
        if self.idle_timeout.is_zero() {
            return 0;
        }
        let timeout = self.idle_timeout;
        let expired: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.last_used.elapsed() >= timeout)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            if let Some(mut session) = self.sessions.remove(id) {
                session.kill();
            }
        }
        expired.len()
    }

    /// Live sessions, sorted by id. Filters out any terminated interpreters.
    pub fn active_sessions(&self) -> Vec<SessionInfo> {
        let mut sessions: Vec<SessionInfo> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.is_alive())
            .map(|(id, session)| SessionInfo {
                session_id: id.clone(),
                language: session.language,
                idle_ms: millis(session.last_used.elapsed()),
                pid: session.pid(),
            })
            .collect();
        sessions.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        sessions
    }

    /// Whether `session_id` currently owns a live daemon.
    pub fn contains(&self, session_id: &str) -> bool {
        self.sessions
            .get(session_id)
            .map(|session| session.is_alive())
            .unwrap_or(false)
    }

    /// Language of a live session.
    pub fn language_of(&self, session_id: &str) -> Option<CodeLanguage> {
        self.sessions
            .get(session_id)
            .filter(|s| s.is_alive())
            .map(|session| session.language)
    }

    /// Number of live sessions.
    pub fn session_count(&self) -> usize {
        self.sessions
            .values()
            .filter(|session| session.is_alive())
            .count()
    }

    #[cfg(test)]
    pub(crate) fn kill_session_process_for_test(&mut self, session_id: &str) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            let _ = session.child.lock().map(|mut child| {
                let _ = child.kill();
                let _ = child.wait();
            });
        }
    }

    #[cfg(test)]
    pub(crate) fn temp_dir_for_test(&self, session_id: &str) -> Option<&Path> {
        self.sessions.get(session_id).map(|s| s.temp_dir.as_path())
    }
}

enum SessionInput {
    Pipe(ChildStdin),
    Socket(std::net::TcpStream),
}

impl Write for SessionInput {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Pipe(p) => p.write(buf),
            Self::Socket(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Pipe(p) => p.flush(),
            Self::Socket(s) => s.flush(),
        }
    }
}

struct Session {
    id: String,
    language: CodeLanguage,
    child: Mutex<Child>,
    pgid: Mutex<Option<u32>>,
    input: Option<SessionInput>,
    lines: Receiver<Option<String>>,
    temp_dir: PathBuf,
    last_used: Instant,
    seq: u64,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("language", &self.language)
            .field("temp_dir", &self.temp_dir)
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        if let Some(child) = self.0.as_mut() {
            child.try_wait()
        } else {
            Ok(None)
        }
    }

    fn into_child(mut self) -> Option<Child> {
        self.0.take()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn generate_auth_token() -> String {
    #[cfg(unix)]
    {
        if let Ok(mut f) = fs::File::open("/dev/urandom") {
            let mut buf = [0u8; 16];
            if f.read_exact(&mut buf).is_ok() {
                return buf.iter().map(|b| format!("{b:02x}")).collect();
            }
        }
    }
    format!(
        "{:016x}{:016x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

impl Session {
    fn spawn(session_id: &str, language: CodeLanguage) -> Result<Self, KernelError> {
        let binary = which_binary(language).ok_or_else(|| KernelError::InterpreterNotFound {
            language,
            binary: language.binary_name(),
        })?;
        let abs_binary = if binary.is_relative() {
            std::env::current_dir()
                .map(|cwd| cwd.join(&binary))
                .unwrap_or(binary)
        } else {
            binary
        };

        let temp_dir = Self::create_secure_temp_dir()?;
        let result = Self::spawn_in(session_id, language, &abs_binary, &temp_dir);
        if result.is_err() {
            let _ = fs::remove_dir_all(&temp_dir);
        }
        result
    }

    fn create_secure_temp_dir() -> Result<PathBuf, KernelError> {
        let base_temp = {
            let tmp = std::env::temp_dir();
            if tmp.is_relative() {
                std::env::current_dir()
                    .map(|cur| cur.join(&tmp))
                    .unwrap_or(tmp)
            } else {
                tmp
            }
        };
        let pid = std::process::id();
        for _ in 0..100 {
            let unique = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
            let rand_suffix = Instant::now().elapsed().as_nanos() as u64;
            let candidate =
                base_temp.join(format!("wordcraft-session-{pid}-{unique}-{rand_suffix:x}"));

            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700);
                match builder.create(&candidate) {
                    Ok(()) => return Ok(candidate),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => {
                        return Err(KernelError::Io(format!(
                            "create secure temp dir {}: {e}",
                            candidate.display()
                        )));
                    }
                }
            }

            #[cfg(not(unix))]
            {
                let mut builder = std::fs::DirBuilder::new();
                match builder.create(&candidate) {
                    Ok(()) => return Ok(candidate),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => {
                        return Err(KernelError::Io(format!(
                            "create secure temp dir {}: {e}",
                            candidate.display()
                        )));
                    }
                }
            }
        }
        Err(KernelError::Io(
            "failed to create unique secure temporary directory after 100 attempts".to_string(),
        ))
    }

    fn spawn_in(
        session_id: &str,
        language: CodeLanguage,
        binary: &Path,
        temp_dir: &Path,
    ) -> Result<Self, KernelError> {
        let (driver_name, driver_source) = match language {
            CodeLanguage::Python => ("driver.py", PYTHON_DRIVER),
            CodeLanguage::R => ("driver.R", R_DRIVER),
        };
        let driver_path = temp_dir.join(driver_name);
        fs::write(&driver_path, driver_source.as_bytes()).map_err(|e| {
            KernelError::ScriptPreparation(format!("write {}: {e}", driver_path.display()))
        })?;

        let stderr_path = temp_dir.join("driver-stderr.txt");
        let stderr_file = File::create(&stderr_path)
            .map_err(|e| KernelError::Io(format!("create {}: {e}", stderr_path.display())))?;

        let mut command = Command::new(binary);
        let (mut child, input) = match language {
            CodeLanguage::Python => {
                command.arg("-u");
                command
                    .arg(&driver_path)
                    .arg(temp_dir)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::from(stderr_file))
                    .current_dir(temp_dir)
                    .env("TMPDIR", temp_dir)
                    .env("TEMP", temp_dir)
                    .env("TMP", temp_dir);

                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    command.process_group(0);
                }

                let mut child = command
                    .spawn()
                    .map_err(|e| KernelError::Io(format!("spawn {}: {e}", binary.display())))?;

                let stdin = child.stdin.take().ok_or_else(|| {
                    KernelError::SessionProtocol(
                        "interpreter stdin pipe was not created".to_string(),
                    )
                })?;
                (child, Some(SessionInput::Pipe(stdin)))
            }
            CodeLanguage::R => {
                let listener = std::net::TcpListener::bind("127.0.0.1:0")
                    .map_err(|e| KernelError::Io(format!("bind R session socket: {e}")))?;
                let port = listener
                    .local_addr()
                    .map_err(|e| KernelError::Io(format!("local_addr for R session: {e}")))?
                    .port();

                let auth_token = generate_auth_token();
                let auth_path = temp_dir.join("auth.txt");
                let _ = fs::write(&auth_path, &auth_token);

                command
                    .arg(&driver_path)
                    .arg(temp_dir)
                    .arg(port.to_string())
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::from(stderr_file))
                    .current_dir(temp_dir)
                    .env("TMPDIR", temp_dir)
                    .env("TEMP", temp_dir)
                    .env("TMP", temp_dir)
                    .env("WORDCRAFT_AUTH_TOKEN", &auth_token);

                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    command.process_group(0);
                }

                let child = command
                    .spawn()
                    .map_err(|e| KernelError::Io(format!("spawn {}: {e}", binary.display())))?;

                let mut child_guard = ChildGuard::new(child);

                listener
                    .set_nonblocking(true)
                    .map_err(|e| KernelError::Io(format!("set nonblocking on R listener: {e}")))?;
                let start = Instant::now();
                let expected_auth = format!("AUTH {auth_token}");

                let stream = loop {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_nonblocking(false).map_err(|e| {
                                KernelError::Io(format!("set blocking on R socket: {e}"))
                            })?;
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                            let mut auth_reader = BufReader::new(&mut stream);
                            let mut auth_line = String::new();
                            let auth_ok = auth_reader.read_line(&mut auth_line).is_ok()
                                && auth_line.trim() == expected_auth;
                            let _ = stream.set_read_timeout(None);
                            if auth_ok {
                                break stream;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if start.elapsed() > Duration::from_secs(15) {
                                return Err(KernelError::SessionTerminated {
                                    session_id: session_id.to_string(),
                                    detail: "timeout waiting for R driver to connect".to_string(),
                                });
                            }
                            if let Ok(Some(_)) = child_guard.try_wait() {
                                return Err(KernelError::SessionTerminated {
                                    session_id: session_id.to_string(),
                                    detail:
                                        "R interpreter exited before connecting to driver socket"
                                            .to_string(),
                                });
                            }
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Err(e) => {
                            return Err(KernelError::Io(format!(
                                "accept connection from R driver: {e}"
                            )));
                        }
                    }
                };
                let child = child_guard.into_child().ok_or_else(|| {
                    KernelError::SessionProtocol(
                        "child process was dropped unexpectedly".to_string(),
                    )
                })?;
                (child, Some(SessionInput::Socket(stream)))
            }
        };

        let pgid = Mutex::new(Some(child.id()));
        let stdout: ChildStdout = child.stdout.take().ok_or_else(|| {
            KernelError::SessionProtocol("interpreter stdout pipe was not created".to_string())
        })?;

        let (sender, lines) = mpsc::sync_channel(256);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                let mut take = (&mut reader).take(65_536);
                match take.read_line(&mut line) {
                    Ok(0) | Err(_) => {
                        let _ = sender.send(None);
                        break;
                    }
                    Ok(_) => {
                        if sender.send(Some(line)).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            id: session_id.to_string(),
            language,
            child: Mutex::new(child),
            pgid,
            input,
            lines,
            temp_dir: temp_dir.to_path_buf(),
            last_used: Instant::now(),
            seq: 0,
        })
    }

    fn run(
        &mut self,
        request: &ExecutionRequest,
        resolved_cwd: Option<&Path>,
    ) -> Result<ExecutionResult, KernelError> {
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        let started = Instant::now();
        let timeout = Duration::from_millis(request.timeout_ms);
        let deadline = match started.checked_add(timeout) {
            Some(d) => d,
            None => {
                return Err(KernelError::SessionProtocol(
                    "timeout exceeds maximum representable duration".to_string(),
                ));
            }
        };
        let format = plot_format_token(request);

        let cwd_str = match resolved_cwd {
            Some(path) => match path.to_str() {
                Some(s) => s,
                None => {
                    return Err(KernelError::SessionProtocol(
                        "session working directory must be valid UTF-8".to_string(),
                    ));
                }
            },
            None => "",
        };

        let session_id = self.id.clone();
        let files = ChunkFiles::new(&self.temp_dir, seq);

        let mut input_pipe = match self.input.take() {
            Some(pipe) => pipe,
            None => {
                return Err(KernelError::SessionTerminated {
                    session_id,
                    detail: "the interpreter is no longer accepting input".to_string(),
                });
            }
        };

        let (send_tx, send_rx) = mpsc::channel();
        let code = request.code.clone();
        let cwd_string = cwd_str.to_string();
        let format_string = format.to_string();
        let capture_flag = request.capture_plots;

        std::thread::spawn(move || {
            let res = write_chunk_payload(
                &mut input_pipe,
                seq,
                capture_flag,
                &format_string,
                &cwd_string,
                &code,
            );
            let _ = send_tx.send((input_pipe, res));
        });

        let remaining = deadline.saturating_duration_since(Instant::now());
        let write_res = if remaining.is_zero() {
            self.kill();
            Err(WaitOutcome::TimedOut)
        } else {
            match send_rx.recv_timeout(remaining) {
                Ok((returned_input, Ok(()))) => {
                    self.input = Some(returned_input);
                    Ok(())
                }
                Ok((_, Err(_))) => {
                    self.kill();
                    Err(WaitOutcome::Terminated)
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.kill();
                    Err(WaitOutcome::TimedOut)
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.kill();
                    Err(WaitOutcome::Terminated)
                }
            }
        };

        match write_res {
            Ok(()) => {}
            Err(WaitOutcome::TimedOut) => {
                let duration_ms = millis(started.elapsed());
                let (stdout, stdout_trunc) = read_text(&files.out, MAX_STREAM_BYTES);
                let (stderr, stderr_trunc) = read_text(&files.err, MAX_STREAM_BYTES);
                let result = ExecutionResult {
                    stdout,
                    stderr,
                    exit_code: None,
                    success: false,
                    timed_out: true,
                    output_truncated: stdout_trunc || stderr_trunc,
                    plot_png: None,
                    plot_svg: None,
                    tables: Vec::new(),
                    duration_ms,
                };
                files.cleanup();
                return Ok(result);
            }
            Err(WaitOutcome::Terminated) => {
                let detail = self.driver_detail();
                files.cleanup();
                return Err(KernelError::SessionTerminated {
                    session_id: self.id.clone(),
                    detail,
                });
            }
            Err(WaitOutcome::Finished) => {}
        }

        let (outcome, extra_stdout, extra_truncated) = self.wait_for_end(seq, deadline);
        let duration_ms = millis(started.elapsed());

        match outcome {
            WaitOutcome::Finished => {
                let mut result = files.read_result(request, duration_ms);
                if !extra_stdout.is_empty() {
                    if result.stdout.len() + extra_stdout.len() > MAX_STREAM_BYTES {
                        let rem = MAX_STREAM_BYTES.saturating_sub(result.stdout.len());
                        result.stdout.push_str(&extra_stdout[..rem]);
                        result.output_truncated = true;
                    } else {
                        result.stdout.push_str(&extra_stdout);
                    }
                }
                if extra_truncated {
                    result.output_truncated = true;
                }
                files.cleanup();
                self.last_used = Instant::now();
                Ok(result)
            }
            WaitOutcome::TimedOut => {
                self.kill();
                let (mut stdout, stdout_trunc) = read_text(&files.out, MAX_STREAM_BYTES);
                let (stderr, stderr_trunc) = read_text(&files.err, MAX_STREAM_BYTES);
                let mut truncated = stdout_trunc || stderr_trunc || extra_truncated;
                if !extra_stdout.is_empty() {
                    if stdout.len() + extra_stdout.len() > MAX_STREAM_BYTES {
                        let rem = MAX_STREAM_BYTES.saturating_sub(stdout.len());
                        stdout.push_str(&extra_stdout[..rem]);
                        truncated = true;
                    } else {
                        stdout.push_str(&extra_stdout);
                    }
                }
                let result = ExecutionResult {
                    stdout,
                    stderr,
                    exit_code: None,
                    success: false,
                    timed_out: true,
                    output_truncated: truncated,
                    plot_png: None,
                    plot_svg: None,
                    tables: Vec::new(),
                    duration_ms,
                };
                files.cleanup();
                Ok(result)
            }
            WaitOutcome::Terminated => {
                let detail = self.driver_detail();
                self.kill();
                files.cleanup();
                Err(KernelError::SessionTerminated {
                    session_id: self.id.clone(),
                    detail,
                })
            }
        }
    }

    fn wait_for_end(&mut self, seq: u64, deadline: Instant) -> (WaitOutcome, String, bool) {
        let expected = format!("END {seq}");
        let mut extra_stdout = String::new();
        let mut extra_truncated = false;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return (WaitOutcome::TimedOut, extra_stdout, extra_truncated);
            }
            match self.lines.recv_timeout(remaining) {
                Ok(Some(line)) => {
                    let trimmed = line.trim();
                    if trimmed == expected {
                        return (WaitOutcome::Finished, extra_stdout, extra_truncated);
                    } else if !trimmed.is_empty() {
                        if extra_stdout.len() < MAX_STREAM_BYTES {
                            let rem = MAX_STREAM_BYTES - extra_stdout.len();
                            if line.len() <= rem {
                                extra_stdout.push_str(&line);
                            } else {
                                extra_stdout.push_str(&line[..rem]);
                                extra_truncated = true;
                            }
                        } else {
                            extra_truncated = true;
                        }
                    }
                }
                Ok(None) => return (WaitOutcome::Terminated, extra_stdout, extra_truncated),
                Err(RecvTimeoutError::Timeout) => {
                    return (WaitOutcome::TimedOut, extra_stdout, extra_truncated);
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return (WaitOutcome::Terminated, extra_stdout, extra_truncated);
                }
            }
        }
    }

    fn is_alive(&self) -> bool {
        match self.child.lock() {
            Ok(mut child) => match child.try_wait() {
                Ok(None) => true,
                Ok(Some(_status)) => {
                    let target_pgid = self.pgid.lock().ok().and_then(|mut g| g.take());
                    if let Some(pgid) = target_pgid {
                        Self::kill_process_group(pgid);
                    }
                    false
                }
                Err(_) => false,
            },
            Err(_) => false,
        }
    }

    fn pid(&self) -> Option<u32> {
        if self.is_alive() {
            match self.child.lock() {
                Ok(child) => Some(child.id()),
                Err(_) => None,
            }
        } else {
            None
        }
    }

    fn kill(&mut self) {
        self.input.take();
        let target_pgid = self.pgid.lock().ok().and_then(|mut g| g.take());
        if let Some(pgid) = target_pgid {
            Self::kill_process_group(pgid);
        }
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn kill_process_group(pgid: u32) {
        #[cfg(unix)]
        {
            let group = format!("-{pgid}");
            let killed = Command::new("/bin/kill")
                .args(["-9", &group])
                .output()
                .is_ok();
            if !killed {
                let killed2 = Command::new("/usr/bin/kill")
                    .args(["-9", &group])
                    .output()
                    .is_ok();
                if !killed2 {
                    let _ = Command::new("kill").args(["-9", &group]).output();
                }
            }
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pgid.to_string()])
                .output();
        }
    }

    fn driver_detail(&self) -> String {
        let path = self.temp_dir.join("driver-stderr.txt");
        let (text, _) = read_text(&path, 4096);
        let detail = tail(&text, 400);
        if detail.is_empty() {
            "the interpreter process exited".to_string()
        } else {
            detail
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.kill();
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

fn write_chunk_payload<W: Write>(
    stream: &mut W,
    seq: u64,
    capture: bool,
    format: &str,
    cwd: &str,
    code: &str,
) -> Result<(), std::io::Error> {
    let cwd_bytes = cwd.as_bytes();
    let code_bytes = code.as_bytes();
    let capture_flag = if capture { '1' } else { '0' };
    let header = format!(
        "RUN {} {} {} {} {}\n",
        seq,
        capture_flag,
        format,
        cwd_bytes.len(),
        code_bytes.len()
    );
    stream.write_all(header.as_bytes())?;
    if !cwd_bytes.is_empty() {
        stream.write_all(cwd_bytes)?;
    }
    if !code_bytes.is_empty() {
        stream.write_all(code_bytes)?;
    }
    stream.flush()
}

enum WaitOutcome {
    Finished,
    TimedOut,
    Terminated,
}

struct ChunkFiles {
    out: PathBuf,
    err: PathBuf,
    status: PathBuf,
    png: PathBuf,
    svg: PathBuf,
}

impl ChunkFiles {
    fn new(dir: &Path, seq: u64) -> Self {
        Self {
            out: dir.join(format!("out-{seq}.txt")),
            err: dir.join(format!("err-{seq}.txt")),
            status: dir.join(format!("status-{seq}.txt")),
            png: dir.join(format!("plot-{seq}.png")),
            svg: dir.join(format!("plot-{seq}.svg")),
        }
    }

    fn read_result(&self, request: &ExecutionRequest, duration_ms: u64) -> ExecutionResult {
        let (stdout, stdout_trunc) = read_text(&self.out, MAX_STREAM_BYTES);
        let (mut stderr, stderr_trunc) = read_text(&self.err, MAX_STREAM_BYTES);
        let (status, _) = read_text(&self.status, 1024);
        let success = status.trim() == "ok";
        if status.trim().is_empty() {
            if !stderr.is_empty() && !stderr.ends_with('\n') {
                stderr.push('\n');
            }
            stderr.push_str("the session did not report a status for this chunk");
        }

        let effective_format = request.effective_plot_format();
        let (plot_png, png_trunc) = if request.capture_plots && effective_format.captures_png() {
            read_png(&self.png, MAX_PLOT_BYTES)
        } else {
            (None, false)
        };
        let (plot_svg, svg_trunc) = if request.capture_plots && effective_format.captures_svg() {
            read_svg(&self.svg, MAX_PLOT_BYTES)
        } else {
            (None, false)
        };

        let output_truncated = stdout_trunc || stderr_trunc || png_trunc || svg_trunc;

        ExecutionResult {
            stdout,
            stderr,
            exit_code: Some(i32::from(!success)),
            success,
            timed_out: false,
            output_truncated,
            plot_png,
            plot_svg,
            tables: Vec::new(),
            duration_ms,
        }
    }

    fn cleanup(&self) {
        let _ = fs::remove_file(&self.out);
        let _ = fs::remove_file(&self.err);
        let _ = fs::remove_file(&self.status);
        let _ = fs::remove_file(&self.png);
        let _ = fs::remove_file(&self.svg);
    }
}

fn plot_format_token(request: &ExecutionRequest) -> &'static str {
    if !request.capture_plots {
        return "none";
    }
    match request.effective_plot_format() {
        PlotFormat::Png => "png",
        PlotFormat::Svg => "svg",
        PlotFormat::Both => "both",
    }
}

fn read_text(path: &Path, limit: usize) -> (String, bool) {
    let Ok(file) = File::open(path) else {
        return (String::new(), false);
    };
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();
    let _ = reader
        .by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut buffer);
    let truncated = buffer.len() > limit;
    if truncated {
        buffer.truncate(limit);
    }
    (String::from_utf8_lossy(&buffer).into_owned(), truncated)
}

fn read_png(path: &Path, limit: usize) -> (Option<Vec<u8>>, bool) {
    let Ok(file) = File::open(path) else {
        return (None, false);
    };
    let mut reader = BufReader::new(file);
    let mut bytes = Vec::new();
    let _ = reader
        .by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes);
    let truncated = bytes.len() > limit;
    if truncated {
        return (None, true);
    }
    if bytes.len() > 64 && bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        (Some(bytes), false)
    } else {
        (None, false)
    }
}

fn read_svg(path: &Path, limit: usize) -> (Option<String>, bool) {
    let (content, truncated) = read_text(path, limit);
    if truncated {
        return (None, true);
    }
    if content.len() > 64 && content.contains("<svg") && content.trim_end().ends_with("</svg>") {
        (Some(content), false)
    } else {
        (None, false)
    }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn tail(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.trim().to_string();
    }
    text.chars()
        .skip(total - max_chars)
        .collect::<String>()
        .trim()
        .to_string()
}

const PYTHON_DRIVER: &str = r##"import os
import sys
import traceback
import types

BASE = sys.argv[1] if len(sys.argv) > 1 else "."

PROTO_IN_FD = os.dup(0)
if hasattr(os, "set_inheritable"):
    os.set_inheritable(PROTO_IN_FD, False)
PROTO_IN = os.fdopen(PROTO_IN_FD, "rb", buffering=0)

PROTO_OUT_FD = os.dup(1)
if hasattr(os, "set_inheritable"):
    os.set_inheritable(PROTO_OUT_FD, False)
PROTO_OUT = os.fdopen(PROTO_OUT_FD, "wb", buffering=0)

DEVNULL_FD = os.open(os.devnull, os.O_RDONLY)
os.dup2(DEVNULL_FD, 0)
os.close(DEVNULL_FD)
sys.stdin = open(os.devnull, "r", encoding="utf-8")

class StableBuffer:
    def __init__(self, fd, parent):
        self.fd = fd
        self.parent = parent

    def write(self, data):
        if not isinstance(data, (bytes, bytearray, memoryview)):
            data = bytes(data)
        return self.parent._write_bytes(bytes(data))

    def flush(self):
        pass

    def isatty(self):
        return False

    def fileno(self):
        return self.fd

    def writable(self):
        return True


class StableStream:
    def __init__(self, fd, max_bytes=10 * 1024 * 1024 + 1):
        self.fd = fd
        self.max_bytes = max_bytes
        self.bytes_written = 0
        self.truncated = False
        self.buffer = StableBuffer(fd, self)

    def fileno(self):
        return self.fd

    def isatty(self):
        return False

    def writable(self):
        return True

    @property
    def encoding(self):
        return "utf-8"

    @property
    def errors(self):
        return "replace"

    def reset_chunk(self):
        self.bytes_written = 0
        self.truncated = False

    def _write_bytes(self, b):
        if self.bytes_written >= self.max_bytes:
            self.truncated = True
            return len(b)
        rem = self.max_bytes - self.bytes_written
        to_write = b[:rem]
        if len(to_write) < len(b):
            self.truncated = True
        os.write(self.fd, to_write)
        self.bytes_written += len(to_write)
        return len(b)

    def write(self, data):
        if not isinstance(data, str):
            data = str(data)
        b = data.encode("utf-8", errors="replace")
        self._write_bytes(b)
        return len(data)

    def flush(self):
        pass

STABLE_STDOUT = StableStream(1)
STABLE_STDERR = StableStream(2)

try:
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib._pylab_helpers import Gcf
except Exception:
    plt = None
    Gcf = None

USER_MOD = types.ModuleType("__main__")
USER_MOD.__file__ = "<wordcraft-chunk>"
sys.modules["__main__"] = USER_MOD
NAMESPACE = USER_MOD.__dict__

def read_exact(n):
    buf = bytearray()
    while len(buf) < n:
        chunk = PROTO_IN.read(n - len(buf))
        if not chunk:
            break
        buf.extend(chunk)
    return bytes(buf)

def get_all_figures():
    if plt is None:
        return []
    if Gcf is not None:
        try:
            return [mgr.canvas.figure for mgr in Gcf.get_all_fig_managers()]
        except Exception:
            pass
    figs = []
    try:
        cur_num = plt.gcf().number if plt.get_fignums() else None
        for num in plt.get_fignums():
            figs.append(plt.figure(num))
        if cur_num is not None:
            plt.figure(cur_num)
    except Exception:
        pass
    return figs

def snapshot_figures():
    snaps = {}
    for fig in get_all_figures():
        try:
            num = getattr(fig, "number", None)
            if num is None:
                continue
            art_count = sum(len(ax.get_children()) for ax in fig.axes)
            stale = bool(getattr(fig, "stale", False))
            snaps[num] = (id(fig), len(fig.axes), art_count, stale)
        except Exception:
            pass
    return snaps

def capture_plots(seq, fmt, before_snaps):
    if plt is None:
        return
    try:
        figs = get_all_figures()
        after_snaps = snapshot_figures()
        active_fig = None
        for fig in figs:
            num = getattr(fig, "number", None)
            if num is None:
                continue
            before = before_snaps.get(num)
            if before is None or before[0] != id(fig):
                active_fig = fig
                break
            after = after_snaps.get(num)
            if before[1:3] != (after[1:3] if after else None):
                active_fig = fig
                break
            is_stale_now = bool(getattr(fig, "stale", False))
            was_stale_before = before[3]
            if is_stale_now and not was_stale_before:
                active_fig = fig
                break
        if active_fig is None:
            return

        if fmt in ("png", "both"):
            active_fig.savefig(
                os.path.join(BASE, "plot-%s.png" % seq),
                dpi=200,
                bbox_inches="tight",
            )
        if fmt in ("svg", "both"):
            active_fig.savefig(
                os.path.join(BASE, "plot-%s.svg" % seq),
                format="svg",
                bbox_inches="tight",
            )
    except Exception:
        pass
    finally:
        for fig in get_all_figures():
            try:
                fig.stale = False
            except Exception:
                pass

def main():
    saved_stdout_fd = os.dup(1)
    saved_stderr_fd = os.dup(2)

    while True:
        line = PROTO_IN.readline()
        if not line:
            return 0
        header = line.decode("utf-8", errors="replace").strip()
        parts = header.split()
        if len(parts) != 6 or parts[0] != "RUN":
            continue
        seq = parts[1]
        capture = parts[2] == "1"
        fmt = parts[3]
        cwd_len = int(parts[4])
        code_len = int(parts[5])

        cwd = ""
        if cwd_len > 0:
            cwd = read_exact(cwd_len).decode("utf-8", errors="replace")

        code = read_exact(code_len).decode("utf-8", errors="replace")

        out_path = os.path.join(BASE, "out-%s.txt" % seq)
        err_path = os.path.join(BASE, "err-%s.txt" % seq)
        status_path = os.path.join(BASE, "status-%s.txt" % seq)

        out_file = open(out_path, "w", encoding="utf-8", buffering=1)
        err_file = open(err_path, "w", encoding="utf-8", buffering=1)

        os.dup2(out_file.fileno(), 1)
        os.dup2(err_file.fileno(), 2)

        STABLE_STDOUT.reset_chunk()
        STABLE_STDERR.reset_chunk()
        sys.stdout = STABLE_STDOUT
        sys.stderr = STABLE_STDERR

        status = "ok"
        before_figures = snapshot_figures()

        try:
            if cwd:
                os.chdir(cwd)
            exec(compile(code + "\n", "<wordcraft-chunk>", "exec"), NAMESPACE)
        except SystemExit as exc:
            status = "error"
            try:
                err_file.write("The chunk tried to exit the session: %s\n" % exc)
            except Exception:
                pass
        except BaseException:
            status = "error"
            try:
                err_file.write(traceback.format_exc())
            except Exception:
                pass
        finally:
            try:
                out_file.flush()
            except Exception:
                pass
            try:
                err_file.flush()
            except Exception:
                pass
            try:
                os.dup2(saved_stdout_fd, 1)
                os.dup2(saved_stderr_fd, 2)
            except Exception:
                pass
            try:
                out_file.close()
            except Exception:
                pass
            try:
                err_file.close()
            except Exception:
                pass
            sys.stdout = STABLE_STDOUT
            sys.stderr = STABLE_STDERR

        if capture:
            capture_plots(seq, fmt, before_figures)
        else:
            for fig in get_all_figures():
                try:
                    fig.stale = False
                except Exception:
                    pass

        with open(status_path, "w", encoding="utf-8") as sf:
            sf.write(status)

        PROTO_OUT.write(("END %s\n" % seq).encode("utf-8"))
        PROTO_OUT.flush()

if __name__ == "__main__":
    raise SystemExit(main())
"##;

const R_DRIVER: &str = r##"options(warn = 1L)

.wc_driver <- base::new.env(parent = base::baseenv())
.wc_driver$.wc_driver <- .wc_driver

.wc_driver$args <- base::commandArgs(trailingOnly = TRUE)
.wc_driver$base <- if (base::length(.wc_driver$args) > 0L) .wc_driver$args[[1L]] else base::tempdir()
.wc_driver$port <- if (base::length(.wc_driver$args) >= 2L) base::as.integer(.wc_driver$args[[2L]]) else NA_integer_
.wc_driver$has_plot <- FALSE

.wc_driver$hook_fn <- function(...) { .wc_driver$has_plot <- TRUE }
base::environment(.wc_driver$hook_fn) <- .wc_driver
base::setHook("plot.new", .wc_driver$hook_fn)
base::setHook("grid.newpage", .wc_driver$hook_fn)

.wc_driver$close_sinks <- function() {
  for (i in base::seq_len(64L)) {
    if (base::sink.number(type = "message") <= 0L) break
    base::try(base::sink(type = "message"), silent = TRUE)
  }
  for (i in base::seq_len(64L)) {
    if (base::sink.number() <= 0L) break
    base::try(base::sink(), silent = TRUE)
  }
}
base::environment(.wc_driver$close_sinks) <- .wc_driver

.wc_driver$wrap_devoff <- function(cap_dev) {
  closed <- FALSE
  orig <- grDevices::dev.off
  wrapper <- function(which = grDevices::dev.cur()) {
    target <- if (base::missing(which)) base::as.integer(grDevices::dev.cur()) else base::as.integer(which)[1L]
    if (!base::is.na(target) && target == cap_dev) {
      closed <<- TRUE
    }
    orig(which)
  }
  base::list(
    is_closed = function() closed,
    install = function() {
      base::tryCatch({
        base::unlockBinding("dev.off", base::as.environment("package:grDevices"))
        base::assign("dev.off", wrapper, envir = base::as.environment("package:grDevices"))
        base::lockBinding("dev.off", base::as.environment("package:grDevices"))
        base::unlockBinding("dev.off", base::asNamespace("grDevices"))
        base::assign("dev.off", wrapper, envir = base::asNamespace("grDevices"))
        base::lockBinding("dev.off", base::asNamespace("grDevices"))
      }, error = function(e) NULL)
    },
    uninstall = function() {
      base::tryCatch({
        base::unlockBinding("dev.off", base::as.environment("package:grDevices"))
        base::assign("dev.off", orig, envir = base::as.environment("package:grDevices"))
        base::lockBinding("dev.off", base::as.environment("package:grDevices"))
        base::unlockBinding("dev.off", base::asNamespace("grDevices"))
        base::assign("dev.off", orig, envir = base::asNamespace("grDevices"))
        base::lockBinding("dev.off", base::asNamespace("grDevices"))
      }, error = function(e) NULL)
    }
  )
}
base::environment(.wc_driver$wrap_devoff) <- .wc_driver

.wc_driver$protect_con <- function(driver_con) {
  driver_id <- base::as.integer(driver_con)
  orig_close <- base::close.connection
  wrapper <- function(con, ...) {
    target_id <- base::tryCatch(base::as.integer(con), error = function(e) -1L)
    if (!base::is.na(target_id) && target_id == driver_id) {
      return(base::invisible())
    }
    orig_close(con, ...)
  }
  base::tryCatch({
    base::unlockBinding("close.connection", base::baseenv())
    base::assign("close.connection", wrapper, envir = base::baseenv())
    base::lockBinding("close.connection", base::baseenv())
  }, error = function(e) NULL)
}
base::environment(.wc_driver$protect_con) <- .wc_driver

.wc_driver$run <- function() {
  con <- if (!base::is.na(.wc_driver$port) && .wc_driver$port > 0L) {
    base::socketConnection(host = "127.0.0.1", port = .wc_driver$port, open = "r+b", blocking = TRUE, timeout = 2147483647L)
  } else {
    base::file("stdin", "rb")
  }
  base::options(timeout = 2147483647L)
  .wc_driver$protect_con(con)

  auth_token <- base::Sys.getenv("WORDCRAFT_AUTH_TOKEN")
  if (!base::nzchar(auth_token)) {
    auth_file <- base::file.path(.wc_driver$base, "auth.txt")
    if (base::file.exists(auth_file)) {
      auth_lines <- base::tryCatch(base::readLines(auth_file, n = 1L, warn = FALSE), error = function(e) "")
      if (base::length(auth_lines) > 0L) auth_token <- auth_lines[[1L]]
    }
  }
  base::writeLines(base::paste0("AUTH ", auth_token), con, useBytes = TRUE)
  base::flush(con)

  repeat {
    header_line <- base::readLines(con, n = 1L, warn = FALSE)
    if (base::length(header_line) == 0L) break
    parts <- base::strsplit(header_line[[1L]], "[[:space:]]+")[[1L]]
    if (base::length(parts) != 6L || !base::identical(parts[[1L]], "RUN")) next
    seq <- parts[[2L]]
    capture <- base::identical(parts[[3L]], "1")
    fmt <- parts[[4L]]
    cwd_len <- base::as.integer(parts[[5L]])
    code_len <- base::as.integer(parts[[6L]])

    cwd <- ""
    if (cwd_len > 0L) {
      cwd_raw <- base::readBin(con, what = "raw", n = cwd_len)
      cwd_res <- base::tryCatch({
        s <- base::rawToChar(cwd_raw)
        base::Encoding(s) <- "UTF-8"
        if (!base::validUTF8(s)) base::stop("cwd is not valid UTF-8")
        s
      }, error = function(e) "")
      cwd <- cwd_res
    }

    code_raw <- base::readBin(con, what = "raw", n = code_len)

    out_path <- base::file.path(.wc_driver$base, base::paste0("out-", seq, ".txt"))
    err_path <- base::file.path(.wc_driver$base, base::paste0("err-", seq, ".txt"))
    status_path <- base::file.path(.wc_driver$base, base::paste0("status-", seq, ".txt"))
    png_path <- base::file.path(.wc_driver$base, base::paste0("plot-", seq, ".png"))
    svg_path <- base::file.path(.wc_driver$base, base::paste0("plot-", seq, ".svg"))

    status <- "ok"
    error_message <- ""
    .wc_driver$has_plot <- FALSE

    if (base::nzchar(cwd)) {
      changed <- base::tryCatch({ base::setwd(cwd); TRUE }, error = base::identity)
      if (base::inherits(changed, "error")) {
        status <- "error"
        error_message <- base::conditionMessage(changed)
      }
    }

    parsed <- NULL
    if (base::identical(status, "ok")) {
      code_res <- base::tryCatch({
        s <- base::rawToChar(code_raw)
        base::Encoding(s) <- "UTF-8"
        if (!base::validUTF8(s)) {
          base::stop("code payload is not valid UTF-8")
        }
        s
      }, error = base::identity)
      if (base::inherits(code_res, "error")) {
        status <- "error"
        error_message <- base::conditionMessage(code_res)
      } else {
        parsed <- base::tryCatch(base::parse(text = code_res, encoding = "UTF-8"), error = base::identity)
        if (base::inherits(parsed, "error")) {
          status <- "error"
          error_message <- base::conditionMessage(parsed)
          parsed <- NULL
        }
      }
    }

    .wc_driver$png_dev <- NA_integer_
    .wc_driver$svg_dev <- NA_integer_
    .wc_driver$device_open <- FALSE
    dev_tracker <- NULL

    if (capture && base::identical(status, "ok")) {
      .wc_driver$device_open <- base::tryCatch({
        if (fmt == "png" || fmt == "both") {
          grDevices::png(png_path, width = 1600, height = 1200, res = 200)
          .wc_driver$png_dev <- base::as.integer(grDevices::dev.cur())
          dev_tracker <- .wc_driver$wrap_devoff(.wc_driver$png_dev)
          dev_tracker$install()
        } else {
          grDevices::svg(svg_path, width = 8, height = 6)
          .wc_driver$svg_dev <- base::as.integer(grDevices::dev.cur())
          dev_tracker <- .wc_driver$wrap_devoff(.wc_driver$svg_dev)
          dev_tracker$install()
        }
        if (fmt == "both") {
          grDevices::dev.control("enable")
        }
        TRUE
      }, error = function(e) FALSE)
    }

    out_con <- base::file(out_path, open = "wt", encoding = "UTF-8")
    err_con <- base::file(err_path, open = "wt", encoding = "UTF-8")
    base::sink(out_con)
    base::sink(err_con, type = "message")

    if (!base::is.null(parsed)) {
      failed <- base::tryCatch({
        for (i in base::seq_along(parsed)) {
          val <- base::withVisible(base::eval(parsed[[i]], envir = .GlobalEnv))
          if (base::isTRUE(val$visible)) {
            base::eval(
              base::as.call(base::list(base::as.name("print"), base::call("quote", val$value))),
              envir = .GlobalEnv
            )
          }
        }
        NULL
      }, error = base::identity)
      if (base::inherits(failed, "error")) {
        status <- "error"
        error_message <- base::conditionMessage(failed)
      }
    }
    if (base::nzchar(error_message)) {
      err_valid <- base::tryCatch(base::isTRUE(base::isOpen(err_con)), error = function(e) FALSE)
      if (err_valid) {
        base::try(base::cat("Error: ", error_message, "\n", sep = "", file = err_con), silent = TRUE)
      } else {
        base::try(base::cat("Error: ", error_message, "\n", sep = "", file = err_path, append = TRUE), silent = TRUE)
      }
    }

    .wc_driver$close_sinks()
    base::try(base::close(err_con), silent = TRUE)
    base::try(base::close(out_con), silent = TRUE)

    if (!base::is.null(dev_tracker)) {
      dev_tracker$uninstall()
    }

    is_closed <- if (!base::is.null(dev_tracker)) base::isTRUE(dev_tracker$is_closed()) else FALSE

    if (.wc_driver$device_open && !is_closed) {
      if (base::isTRUE(.wc_driver$has_plot)) {
        if (fmt == "both" && !base::is.na(.wc_driver$png_dev) && .wc_driver$png_dev %in% grDevices::dev.list()) {
          grDevices::dev.set(.wc_driver$png_dev)
          svg_copy_dev <- base::tryCatch({
            copy_dev <- grDevices::dev.copy(grDevices::svg, svg_path, width = 8, height = 6)
            base::as.integer(copy_dev)
          }, error = function(e) NA_integer_)
          if (!base::is.na(svg_copy_dev) && svg_copy_dev %in% grDevices::dev.list()) {
            grDevices::dev.off(svg_copy_dev)
          }
        }
      }
      if (!base::is.na(.wc_driver$png_dev) && .wc_driver$png_dev %in% grDevices::dev.list()) {
        grDevices::dev.off(.wc_driver$png_dev)
      }
      if (!base::is.na(.wc_driver$svg_dev) && .wc_driver$svg_dev %in% grDevices::dev.list()) {
        grDevices::dev.off(.wc_driver$svg_dev)
      }
      if (!base::isTRUE(.wc_driver$has_plot)) {
        if (base::file.exists(png_path)) base::try(base::file.remove(png_path), silent = TRUE)
        if (base::file.exists(svg_path)) base::try(base::file.remove(svg_path), silent = TRUE)
      }
    }

    base::writeLines(status, status_path, useBytes = TRUE)
    base::cat("\nEND ", seq, "\n", sep = "")
    base::flush(base::stdout())
  }
}

base::environment(.wc_driver$run) <- .wc_driver
.wc_driver$run()
"##;
