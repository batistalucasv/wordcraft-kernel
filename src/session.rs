//! Persistent interpreter sessions.
//!
//! [`SessionManager`] keeps one long-lived R or Python process per
//! `session_id`, so a value created by one code chunk is still in memory when a
//! later chunk runs. Each process is a background daemon with an isolated
//! protocol channel (Python stdin/stdout or an authenticated R socket); user
//! output is captured separately so it cannot corrupt the protocol.
//!
//! # Wire protocol
//!
//! Both drivers read one length-prefixed request from their protocol channel
//! (Python stdin or the authenticated R socket):
//!
//! ```text
//! RUN <seq> <capture:0|1> <format:png|svg|both|none> <cwd_bytes_len> <code_bytes_len> <paths_bytes_len>\n
//! <cwd_bytes>
//! <code_bytes>
//! <paths_blob>
//! ```
//!
//! `<paths_blob>` is a sequence of `u32` little-endian length prefixes followed by
//! the Python search-path bytes. The R driver reads and ignores the blob and always
//! sends a length of zero, because `python_path` applies only to Python sessions.
//!
//! Because the exact byte lengths of the working directory, code payload and search
//! paths are specified in the header, arbitrary user code (including lines matching
//! protocol words, multiline strings, or non-ASCII characters) can never collide with
//! or terminate framing.
//!
//! The daemon evaluates the code in its execution environment, writes
//! `out-<seq>.txt`, `err-<seq>.txt`, `status-<seq>.txt` and, when a plot was
//! drawn, `plot-<seq>.png` / `plot-<seq>.svg` in its private temporary
//! directory, then sends `END <seq>\n` over its dedicated protocol channel
//! (Python stdout or the authenticated R socket).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
#[cfg(unix)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use command_group::{CommandGroup, GroupChild};
use serde::{Deserialize, Serialize};

use crate::{
    CodeLanguage, ExecutionRequest, ExecutionResult, KernelError, MAX_CAPTURE_BYTES, MAX_PLOT_BYTES, PlotFormat, create_scratch_dir,
    create_unique_scratch_dir, read_bounded_bytes, read_png_bounded, read_svg_bounded, truncate_to_char_boundary, which_binary,
};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Default time an unused session is kept alive before it is reaped.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Default maximum number of concurrent active interpreter sessions.
pub const DEFAULT_MAX_SESSIONS: usize = 16;

/// Maximum bytes kept from each captured output stream (stdout and stderr) per chunk.
pub const MAX_STREAM_BYTES: usize = MAX_CAPTURE_BYTES;

/// Time allowed for a freshly spawned interpreter to connect and authenticate.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

/// Longest an unauthenticated loopback peer may hold the R accept loop.
const AUTH_PEER_TIMEOUT: Duration = Duration::from_secs(1);

/// Quiet period used to decide that a daemon's output streams are drained when
/// pipe-level readiness cannot be queried (non-Unix platforms).
#[cfg(not(unix))]
const STREAM_DRAIN_QUIET: Duration = Duration::from_millis(2);
#[cfg(unix)]
const STREAM_DRAIN_QUIET: Duration = Duration::from_millis(10);

/// Hard upper bound for draining a daemon's output streams after a chunk.
const STREAM_DRAIN_LIMIT: Duration = Duration::from_millis(250);

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
        Self { sessions: HashMap::new(), idle_timeout: DEFAULT_IDLE_TIMEOUT, max_sessions: DEFAULT_MAX_SESSIONS }
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
    ///
    /// `timeout_ms` bounds chunk execution only. A new R session has a separate
    /// fixed 15-second handshake deadline; if it cannot connect and authenticate
    /// in time, the spawn error is returned instead of a timed-out result. Python
    /// sessions return from startup as soon as the process is launched and do not
    /// wait for a readiness handshake, so a cold Python import counts against
    /// `timeout_ms` and can return `timed_out` before the chunk runs.
    pub fn execute(&mut self, session_id: &str, request: &ExecutionRequest) -> Result<ExecutionResult, KernelError> {
        let timeout = Duration::from_millis(request.timeout_ms);
        if Instant::now().checked_add(timeout).is_none() {
            return Err(KernelError::SessionProtocol("timeout exceeds maximum representable duration".to_string()));
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
                        return Err(KernelError::SessionProtocol("the working directory path must be valid UTF-8".to_string()));
                    }
                };
                if text.contains('\n') || text.contains('\r') {
                    return Err(KernelError::SessionProtocol("the working directory path contains a line break".to_string()));
                }
                if !dir.exists() {
                    return Err(KernelError::Io(format!("working directory does not exist: {}", dir.display())));
                }
                if !dir.is_dir() {
                    return Err(KernelError::Io(format!("working directory is not a directory: {}", dir.display())));
                }
                let abs =
                    if dir.is_relative() { std::env::current_dir().map(|cur| cur.join(dir)).unwrap_or_else(|_| dir.clone()) } else { dir.clone() };
                if abs.to_str().is_none() {
                    return Err(KernelError::SessionProtocol("the working directory path must be valid UTF-8".to_string()));
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
                return Err(KernelError::SessionCapacityExceeded { max_sessions: self.max_sessions });
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
        let expired: Vec<String> =
            self.sessions.iter().filter(|(_, session)| session.last_used.elapsed() >= timeout).map(|(id, _)| id.clone()).collect();
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
        self.sessions.get(session_id).map(|session| session.is_alive()).unwrap_or(false)
    }

    /// Language of a live session.
    pub fn language_of(&self, session_id: &str) -> Option<CodeLanguage> {
        self.sessions.get(session_id).filter(|s| s.is_alive()).map(|session| session.language)
    }

    /// Number of live sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.values().filter(|session| session.is_alive()).count()
    }

    #[cfg(test)]
    pub(crate) fn kill_session_process_for_test(&mut self, session_id: &str) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            let _ = session.child.lock().map(|mut guard| {
                if let Some(mut child) = guard.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
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
    child: Mutex<Option<GroupChild>>,
    input: Option<SessionInput>,
    lines: Receiver<Option<String>>,
    capture: Option<Arc<StreamCapture>>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureStream {
    Stdout,
    Stderr,
}

#[derive(Debug)]
struct CaptureState {
    active: Option<u64>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
    saw_chunk_output: bool,
    stderr_tail: Vec<u8>,
    last_read: Instant,
}

/// Bounded, chunk-scoped capture of a daemon's inherited stdout and stderr.
///
/// Bytes are associated with whichever chunk is active when they arrive, and
/// output written while no chunk is active is discarded. R cannot portably
/// redirect the OS descriptors inherited by background subprocesses, so a
/// subprocess from an earlier chunk that writes while a later chunk is active is
/// attributed to that later chunk. Excess bytes are drained and discarded once
/// the per-stream budget is spent, which keeps a verbose daemon from filling the
/// scratch volume.
#[derive(Debug)]
struct StreamCapture {
    state: Mutex<CaptureState>,
    drained: Condvar,
    #[cfg(unix)]
    stdout_probe: Option<std::os::fd::OwnedFd>,
    #[cfg(unix)]
    stderr_probe: Option<std::os::fd::OwnedFd>,
    #[cfg(unix)]
    stdout_idle: AtomicBool,
    #[cfg(unix)]
    stderr_idle: AtomicBool,
}

impl StreamCapture {
    fn new() -> Self {
        Self {
            state: Mutex::new(CaptureState {
                active: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                saw_chunk_output: false,
                stderr_tail: Vec::new(),
                last_read: Instant::now(),
            }),
            drained: Condvar::new(),
            #[cfg(unix)]
            stdout_probe: None,
            #[cfg(unix)]
            stderr_probe: None,
            #[cfg(unix)]
            stdout_idle: AtomicBool::new(true),
            #[cfg(unix)]
            stderr_idle: AtomicBool::new(true),
        }
    }

    #[cfg(unix)]
    fn set_probes(&mut self, stdout: Option<std::os::fd::OwnedFd>, stderr: Option<std::os::fd::OwnedFd>) {
        self.stdout_probe = stdout;
        self.stderr_probe = stderr;
    }

    #[cfg(unix)]
    fn set_pump_idle(&self, stream: CaptureStream, idle: bool) {
        let flag = match stream {
            CaptureStream::Stdout => &self.stdout_idle,
            CaptureStream::Stderr => &self.stderr_idle,
        };
        flag.store(idle, Ordering::Release);
        if idle {
            self.drained.notify_all();
        }
    }

    #[cfg(unix)]
    fn pumps_idle(&self) -> bool {
        self.stdout_idle.load(Ordering::Acquire) && self.stderr_idle.load(Ordering::Acquire)
    }

    /// Whether either inherited pipe still holds bytes the pump has not read.
    #[cfg(unix)]
    fn pipe_has_data(&self) -> bool {
        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
        use std::os::fd::AsFd;

        [self.stdout_probe.as_ref(), self.stderr_probe.as_ref()].into_iter().flatten().any(|fd| {
            let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
            match poll(&mut fds, PollTimeout::ZERO) {
                Ok(ready) => ready > 0 && fds[0].revents().is_some_and(|events| events.contains(PollFlags::POLLIN)),
                Err(_) => false,
            }
        })
    }

    /// Windows has no portable peek for anonymous pipe handles, so the quiet
    /// period below is the only drain signal there.
    #[cfg(not(unix))]
    fn pipe_has_data(&self) -> bool {
        false
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CaptureState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn start(&self, seq: u64) {
        // Discard output that was written while the session was idle before this
        // chunk becomes the owner of anything the pump reads next.
        let deadline = Instant::now() + STREAM_DRAIN_LIMIT;
        loop {
            #[cfg(unix)]
            let drained_now = !self.pipe_has_data() && self.pumps_idle();
            #[cfg(not(unix))]
            let drained_now = {
                let state = self.lock();
                state.last_read.elapsed() >= STREAM_DRAIN_QUIET
            };
            if drained_now || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        let mut state = self.lock();
        state.active = Some(seq);
        state.stdout.clear();
        state.stderr.clear();
        state.stdout_truncated = false;
        state.stderr_truncated = false;
        state.saw_chunk_output = false;
        state.last_read = Instant::now();
    }

    fn push(&self, stream: CaptureStream, bytes: &[u8]) {
        let mut state = self.lock();
        state.last_read = Instant::now();
        if stream == CaptureStream::Stderr {
            const TAIL_BYTES: usize = 4096;
            state.stderr_tail.extend_from_slice(bytes);
            if state.stderr_tail.len() > TAIL_BYTES {
                let excess = state.stderr_tail.len() - TAIL_BYTES;
                state.stderr_tail.drain(..excess);
            }
        }
        if state.active.is_some() {
            state.saw_chunk_output = true;
            match stream {
                CaptureStream::Stdout => {
                    let remaining = MAX_STREAM_BYTES.saturating_sub(state.stdout.len());
                    if bytes.len() > remaining {
                        state.stdout_truncated = true;
                        state.stdout.extend_from_slice(&bytes[..remaining]);
                    } else {
                        state.stdout.extend_from_slice(bytes);
                    }
                }
                CaptureStream::Stderr => {
                    let remaining = MAX_STREAM_BYTES.saturating_sub(state.stderr.len());
                    if bytes.len() > remaining {
                        state.stderr_truncated = true;
                        state.stderr.extend_from_slice(&bytes[..remaining]);
                    } else {
                        state.stderr.extend_from_slice(bytes);
                    }
                }
            }
        }
        self.drained.notify_all();
    }

    /// Return the output captured for `seq`, waiting briefly for in-flight reads.
    fn finish(&self, seq: u64) -> (String, String, bool) {
        let mut state = self.lock();
        if state.active != Some(seq) {
            return (String::new(), String::new(), false);
        }
        let deadline = Instant::now() + STREAM_DRAIN_LIMIT;
        loop {
            // On Unix, pipe readiness + pump idle state tells us synchronously whether
            // anything written before `END` still sits in the pipe or pump thread,
            // avoiding any fixed post-chunk sleep.
            #[cfg(unix)]
            let done = !self.pipe_has_data() && self.pumps_idle();
            #[cfg(not(unix))]
            let done = !self.pipe_has_data() && (!state.saw_chunk_output || state.last_read.elapsed() >= STREAM_DRAIN_QUIET);
            if done {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            let wait = deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(2));
            let (guard, _) = self.drained.wait_timeout(state, wait).unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
        }
        state.active = None;
        let stdout = String::from_utf8_lossy(&state.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&state.stderr).into_owned();
        let truncated = state.stdout_truncated || state.stderr_truncated;
        state.stdout.clear();
        state.stderr.clear();
        (stdout, stderr, truncated)
    }

    fn stderr_tail(&self) -> String {
        let state = self.lock();
        String::from_utf8_lossy(&state.stderr_tail).into_owned()
    }
}

#[cfg(unix)]
fn pump_stream<R: Read + std::os::fd::AsFd>(mut reader: R, stream: CaptureStream, capture: Arc<StreamCapture>) {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

    let mut buffer = [0u8; 8192];
    loop {
        capture.set_pump_idle(stream, true);
        let mut fds = [PollFd::new(reader.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }
        capture.set_pump_idle(stream, false);
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => capture.push(stream, &buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    capture.set_pump_idle(stream, true);
}

#[cfg(not(unix))]
fn pump_stream<R: Read>(mut reader: R, stream: CaptureStream, capture: Arc<StreamCapture>) {
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => capture.push(stream, &buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn pump_protocol_lines<R: Read>(reader: R, sender: mpsc::SyncSender<Option<String>>) {
    let mut reader = BufReader::new(reader);
    loop {
        let mut bytes = Vec::new();
        let mut take = (&mut reader).take(65_536);
        match take.read_until(b'\n', &mut bytes) {
            Ok(0) | Err(_) => {
                let _ = sender.send(None);
                break;
            }
            Ok(_) => {
                let line = String::from_utf8_lossy(&bytes).into_owned();
                if sender.send(Some(line)).is_err() {
                    break;
                }
            }
        }
    }
}

struct ChildGuard {
    child: Option<GroupChild>,
}

impl ChildGuard {
    fn new(child: GroupChild) -> Self {
        Self { child: Some(child) }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        if let Some(child) = self.child.as_mut() { child.inner().try_wait() } else { Ok(None) }
    }

    fn child_mut(&mut self) -> Option<&mut GroupChild> {
        self.child.as_mut()
    }

    fn into_child(mut self) -> Option<GroupChild> {
        self.child.take()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn generate_auth_token() -> Result<String, KernelError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| KernelError::Io(format!("secure random for R session token: {error}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn read_auth_line(stream: &mut std::net::TcpStream, expected: &[u8], deadline: Instant) -> std::io::Result<bool> {
    stream.set_nonblocking(true)?;
    let mut line = Vec::with_capacity(64);
    let mut buffer = [0u8; 64];

    loop {
        if Instant::now() >= deadline {
            return Ok(false);
        }

        match stream.read(&mut buffer) {
            Ok(0) => return Ok(false),
            Ok(read) => {
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                let bytes = &buffer[..read];
                let line_end = bytes.iter().position(|byte| *byte == b'\n');
                let bytes_before_newline = line_end.map_or(bytes, |end| &bytes[..end]);
                if line.len().saturating_add(bytes_before_newline.len()) > 256 {
                    return Ok(false);
                }
                line.extend_from_slice(bytes_before_newline);

                if line_end.is_some() {
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    return Ok(line == expected);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(false);
                }
                std::thread::sleep(remaining.min(Duration::from_millis(1)));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error),
        }
    }
}

impl Session {
    fn spawn(session_id: &str, language: CodeLanguage) -> Result<Self, KernelError> {
        let binary = which_binary(language).ok_or_else(|| KernelError::InterpreterNotFound { language, binary: language.binary_name() })?;
        let abs_binary = if binary.is_relative() { std::env::current_dir().map(|cwd| cwd.join(&binary)).unwrap_or(binary) } else { binary };

        let temp_dir = create_unique_scratch_dir("wordcraft-session", &SESSION_COUNTER, 100)?;
        let result = Self::spawn_in(session_id, language, &abs_binary, &temp_dir);
        if result.is_err() {
            let _ = fs::remove_dir_all(&temp_dir);
        }
        result
    }

    fn spawn_in(session_id: &str, language: CodeLanguage, binary: &Path, temp_dir: &Path) -> Result<Self, KernelError> {
        let (driver_name, driver_source) = match language {
            CodeLanguage::Python => ("driver.py", PYTHON_DRIVER),
            CodeLanguage::R => ("driver.R", R_DRIVER),
        };
        let driver_path = temp_dir.join(driver_name);
        fs::write(&driver_path, driver_source.as_bytes())
            .map_err(|e| KernelError::ScriptPreparation(format!("write {}: {e}", driver_path.display())))?;

        let stderr_path = temp_dir.join("driver-stderr.txt");

        let work_dir = temp_dir.join("work");
        create_scratch_dir(&work_dir).map_err(|e| KernelError::Io(format!("create session work dir {}: {e}", work_dir.display())))?;

        let mut capture: Option<Arc<StreamCapture>> = None;

        let mut command = Command::new(binary);
        let (mut child_guard, input, protocol_socket) = match language {
            CodeLanguage::Python => {
                let stderr_file = File::create(&stderr_path).map_err(|e| KernelError::Io(format!("create {}: {e}", stderr_path.display())))?;
                command
                    .arg("-u")
                    .arg(&driver_path)
                    .arg(temp_dir)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::from(stderr_file))
                    .current_dir(&work_dir)
                    .env("TMPDIR", temp_dir)
                    .env("TEMP", temp_dir)
                    .env("TMP", temp_dir);

                let mut child_guard =
                    ChildGuard::new(command.group_spawn().map_err(|e| KernelError::Io(format!("spawn {}: {e}", binary.display())))?);

                let stdin = child_guard
                    .child_mut()
                    .and_then(|child| child.inner().stdin.take())
                    .ok_or_else(|| KernelError::SessionProtocol("interpreter stdin pipe was not created".to_string()))?;
                (child_guard, Some(SessionInput::Pipe(stdin)), None)
            }
            CodeLanguage::R => {
                let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| KernelError::Io(format!("bind R session socket: {e}")))?;
                let port = listener.local_addr().map_err(|e| KernelError::Io(format!("local_addr for R session: {e}")))?.port();

                let auth_token = generate_auth_token()?;
                let auth_path = temp_dir.join("auth.txt");
                let _ = fs::write(&auth_path, &auth_token);

                command
                    .arg("--vanilla")
                    .arg(&driver_path)
                    .arg(temp_dir)
                    .arg(port.to_string())
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .current_dir(&work_dir)
                    .env("TMPDIR", temp_dir)
                    .env("TEMP", temp_dir)
                    .env("TMP", temp_dir)
                    .env("WORDCRAFT_AUTH_TOKEN", &auth_token);

                let mut child_guard =
                    ChildGuard::new(command.group_spawn().map_err(|e| KernelError::Io(format!("spawn {}: {e}", binary.display())))?);

                let stdout = child_guard
                    .child_mut()
                    .and_then(|child| child.inner().stdout.take())
                    .ok_or_else(|| KernelError::SessionProtocol("R stdout pipe was not created".to_string()))?;
                let stderr = child_guard
                    .child_mut()
                    .and_then(|child| child.inner().stderr.take())
                    .ok_or_else(|| KernelError::SessionProtocol("R stderr pipe was not created".to_string()))?;
                let mut stream_capture = StreamCapture::new();
                #[cfg(unix)]
                {
                    use std::os::fd::AsFd;
                    stream_capture.set_probes(stdout.as_fd().try_clone_to_owned().ok(), stderr.as_fd().try_clone_to_owned().ok());
                }
                let stream_capture = Arc::new(stream_capture);
                let writer = Arc::clone(&stream_capture);
                std::thread::Builder::new()
                    .name("wc-session-r-stdout".to_string())
                    .spawn(move || pump_stream(stdout, CaptureStream::Stdout, writer))
                    .map_err(|e| KernelError::Io(format!("spawn R stdout pump: {e}")))?;
                let writer = Arc::clone(&stream_capture);
                std::thread::Builder::new()
                    .name("wc-session-r-stderr".to_string())
                    .spawn(move || pump_stream(stderr, CaptureStream::Stderr, writer))
                    .map_err(|e| KernelError::Io(format!("spawn R stderr pump: {e}")))?;
                capture = Some(stream_capture);

                listener.set_nonblocking(true).map_err(|e| KernelError::Io(format!("set nonblocking on R listener: {e}")))?;
                let deadline = Instant::now() + STARTUP_TIMEOUT;
                let expected_auth = format!("AUTH {auth_token}");
                let mut accept_backoff = Duration::from_millis(1);

                let stream = loop {
                    if Instant::now() >= deadline {
                        return Err(KernelError::SessionTerminated {
                            session_id: session_id.to_string(),
                            detail: "timeout waiting for R driver to connect".to_string(),
                        });
                    }
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            // An unauthenticated peer may hold this attempt only briefly;
                            // the legitimate driver still has until the startup deadline.
                            let peer_deadline = (Instant::now() + AUTH_PEER_TIMEOUT).min(deadline);
                            let auth_ok = read_auth_line(&mut stream, expected_auth.as_bytes(), peer_deadline)
                                .map_err(|e| KernelError::Io(format!("read R driver authentication: {e}")))?;
                            if auth_ok {
                                stream.set_nonblocking(false).map_err(|e| KernelError::Io(format!("set blocking on R socket: {e}")))?;
                                break stream;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if let Ok(Some(_)) = child_guard.try_wait() {
                                return Err(KernelError::SessionTerminated {
                                    session_id: session_id.to_string(),
                                    detail: "R interpreter exited before connecting to driver socket".to_string(),
                                });
                            }
                            let remaining = deadline.saturating_duration_since(Instant::now());
                            std::thread::sleep(accept_backoff.min(remaining));
                            accept_backoff = (accept_backoff * 2).min(Duration::from_millis(10));
                        }
                        Err(e) => {
                            return Err(KernelError::Io(format!("accept connection from R driver: {e}")));
                        }
                    }
                };
                let protocol_socket = stream.try_clone().map_err(|e| KernelError::Io(format!("clone R protocol socket: {e}")))?;
                (child_guard, Some(SessionInput::Socket(stream)), Some(protocol_socket))
            }
        };

        let (sender, lines) = mpsc::sync_channel(256);
        let protocol_reader: Box<dyn Read + Send> = match protocol_socket {
            Some(stream) => Box::new(stream),
            None => Box::new(
                child_guard
                    .child_mut()
                    .and_then(|child| child.inner().stdout.take())
                    .ok_or_else(|| KernelError::SessionProtocol("interpreter stdout pipe was not created".to_string()))?,
            ),
        };
        std::thread::Builder::new()
            .name("wc-session-protocol".to_string())
            .spawn(move || pump_protocol_lines(protocol_reader, sender))
            .map_err(|e| KernelError::Io(format!("spawn session protocol reader: {e}")))?;

        let child = child_guard.into_child().ok_or_else(|| KernelError::SessionProtocol("child process was dropped unexpectedly".to_string()))?;

        Ok(Self {
            id: session_id.to_string(),
            language,
            child: Mutex::new(Some(child)),
            input,
            lines,
            capture,
            temp_dir: temp_dir.to_path_buf(),
            last_used: Instant::now(),
            seq: 0,
        })
    }

    fn run(&mut self, request: &ExecutionRequest, resolved_cwd: Option<&Path>) -> Result<ExecutionResult, KernelError> {
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        let started = Instant::now();
        let timeout = Duration::from_millis(request.timeout_ms);
        let deadline = match started.checked_add(timeout) {
            Some(d) => d,
            None => {
                return Err(KernelError::SessionProtocol("timeout exceeds maximum representable duration".to_string()));
            }
        };
        let format = plot_format_token(request);

        let cwd_str = match resolved_cwd {
            Some(path) => match path.to_str() {
                Some(s) => s,
                None => {
                    return Err(KernelError::SessionProtocol("session working directory must be valid UTF-8".to_string()));
                }
            },
            None => "",
        };

        let session_id = self.id.clone();
        let files = ChunkFiles::new(&self.temp_dir, seq, self.language);
        let python_paths = python_path_bytes(request);

        if let Some(capture) = self.capture.as_ref() {
            capture.start(seq);
        }

        let mut input_pipe = match self.input.take() {
            Some(pipe) => pipe,
            None => {
                return Err(KernelError::SessionTerminated { session_id, detail: "the interpreter is no longer accepting input".to_string() });
            }
        };

        let remaining = deadline.saturating_duration_since(Instant::now());
        let write_res = if remaining.is_zero() {
            drop(input_pipe);
            self.kill();
            Err(WaitOutcome::TimedOut)
        } else if let SessionInput::Socket(mut stream) = input_pipe {
            // Write directly on TCP sockets with SO_SNDTIMEO rather than spawning a thread per chunk.
            let _ = stream.set_write_timeout(Some(remaining));
            match write_chunk_payload(&mut stream, seq, request.capture_plots, format, cwd_str, &request.code, &python_paths) {
                Ok(()) => {
                    let _ = stream.set_write_timeout(None);
                    self.input = Some(SessionInput::Socket(stream));
                    Ok(())
                }
                Err(error) if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) || Instant::now() >= deadline => {
                    drop(stream);
                    self.kill();
                    Err(WaitOutcome::TimedOut)
                }
                Err(_) => {
                    drop(stream);
                    self.kill();
                    Err(WaitOutcome::Terminated)
                }
            }
        } else {
            let (send_tx, send_rx) = mpsc::channel();
            let code = request.code.clone();
            let cwd_string = cwd_str.to_string();
            let format_string = format.to_string();
            let capture_flag = request.capture_plots;

            let writer = std::thread::Builder::new().name("wc-session-request-writer".to_string()).spawn(move || {
                let res = write_chunk_payload(&mut input_pipe, seq, capture_flag, &format_string, &cwd_string, &code, &python_paths);
                let _ = send_tx.send((input_pipe, res));
            });
            if let Err(error) = writer {
                self.kill();
                return Err(KernelError::Io(format!("spawn session request writer: {error}")));
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.kill();
                let _ = send_rx.recv_timeout(Duration::from_millis(250));
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
                        // Killing the daemon closes the request pipe, so the writer
                        // thread observes a broken pipe and exits instead of blocking.
                        let _ = send_rx.recv_timeout(Duration::from_millis(250));
                        Err(WaitOutcome::TimedOut)
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        self.kill();
                        Err(WaitOutcome::Terminated)
                    }
                }
            }
        };

        match write_res {
            Ok(()) => {}
            Err(WaitOutcome::TimedOut) => {
                let duration_ms = millis(started.elapsed());
                let (captured_stdout, captured_stderr, captured_truncated) = self.finish_capture(seq);
                let (stdout, stderr, truncated) = assemble_streams(&files, &captured_stdout, &captured_stderr, captured_truncated);
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
                return Ok(result);
            }
            Err(WaitOutcome::Terminated) => {
                let _ = self.finish_capture(seq);
                let detail = self.driver_detail();
                files.cleanup();
                return Err(KernelError::SessionTerminated { session_id: self.id.clone(), detail });
            }
            Err(WaitOutcome::Finished) => {}
        }

        let (outcome, extra_stdout, extra_truncated) = self.wait_for_end(seq, deadline);
        let duration_ms = millis(started.elapsed());

        match outcome {
            WaitOutcome::Finished => {
                let (mut captured_stdout, captured_stderr, captured_truncated) = self.finish_capture(seq);
                let mut truncated = captured_truncated;
                if append_bounded_text(&mut captured_stdout, &extra_stdout, MAX_STREAM_BYTES) {
                    truncated = true;
                }
                if extra_truncated {
                    truncated = true;
                }
                let (stdout, stderr, truncated) = assemble_streams(&files, &captured_stdout, &captured_stderr, truncated);
                let result = files.read_result(request, duration_ms, stdout, stderr, truncated);
                files.cleanup();
                self.last_used = Instant::now();
                Ok(result)
            }
            WaitOutcome::TimedOut => {
                self.kill();
                let (mut captured_stdout, captured_stderr, captured_truncated) = self.finish_capture(seq);
                let mut truncated = captured_truncated;
                if append_bounded_text(&mut captured_stdout, &extra_stdout, MAX_STREAM_BYTES) {
                    truncated = true;
                }
                if extra_truncated {
                    truncated = true;
                }
                let (stdout, stderr, truncated) = assemble_streams(&files, &captured_stdout, &captured_stderr, truncated);
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
                let _ = self.finish_capture(seq);
                let detail = self.driver_detail();
                self.kill();
                files.cleanup();
                Err(KernelError::SessionTerminated { session_id: self.id.clone(), detail })
            }
        }
    }

    fn finish_capture(&self, seq: u64) -> (String, String, bool) {
        match self.capture.as_ref() {
            Some(capture) => capture.finish(seq),
            None => (String::new(), String::new(), false),
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
                    } else if !trimmed.is_empty() && append_bounded_text(&mut extra_stdout, &line, MAX_STREAM_BYTES) {
                        extra_truncated = true;
                    }
                }
                Ok(None) => return (WaitOutcome::Terminated, extra_stdout, extra_truncated),
                Err(RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline {
                        return (WaitOutcome::TimedOut, extra_stdout, extra_truncated);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return (WaitOutcome::Terminated, extra_stdout, extra_truncated);
                }
            }
        }
    }

    fn is_alive(&self) -> bool {
        match self.child.lock() {
            Ok(mut guard) => {
                let Some(child) = guard.as_mut() else {
                    return false;
                };
                match child.inner().try_wait() {
                    Ok(None) => true,
                    Ok(Some(_status)) => {
                        if let Some(mut dead) = guard.take() {
                            let _ = dead.kill();
                            let _ = dead.wait();
                        }
                        false
                    }
                    Err(_) => false,
                }
            }
            Err(_) => false,
        }
    }

    fn pid(&self) -> Option<u32> {
        if self.is_alive() {
            match self.child.lock() {
                Ok(guard) => guard.as_ref().map(|child| child.id()),
                Err(_) => None,
            }
        } else {
            None
        }
    }

    fn kill(&mut self) {
        self.input.take();
        if let Ok(mut guard) = self.child.lock()
            && let Some(mut child) = guard.take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn driver_detail(&self) -> String {
        if let Some(capture) = self.capture.as_ref() {
            let detail = tail(&capture.stderr_tail(), 400);
            if !detail.is_empty() {
                return detail;
            }
        }
        let path = self.temp_dir.join("driver-stderr.txt");
        let (text, _) = read_text(&path, 4096);
        let detail = tail(&text, 400);
        if detail.is_empty() { "the interpreter process exited".to_string() } else { detail }
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
    python_paths: &[Vec<u8>],
) -> Result<(), std::io::Error> {
    let cwd_bytes = cwd.as_bytes();
    let code_bytes = code.as_bytes();
    let mut paths_bytes = Vec::new();
    for path in python_paths {
        let length = u32::try_from(path.len())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "a Python search path exceeds the protocol length limit"))?;
        paths_bytes.extend_from_slice(&length.to_le_bytes());
        paths_bytes.extend_from_slice(path);
    }
    let capture_flag = if capture { '1' } else { '0' };
    let header = format!("RUN {} {} {} {} {} {}\n", seq, capture_flag, format, cwd_bytes.len(), code_bytes.len(), paths_bytes.len());
    stream.write_all(header.as_bytes())?;
    if !cwd_bytes.is_empty() {
        stream.write_all(cwd_bytes)?;
    }
    if !code_bytes.is_empty() {
        stream.write_all(code_bytes)?;
    }
    if !paths_bytes.is_empty() {
        stream.write_all(&paths_bytes)?;
    }
    stream.flush()
}

/// Encode Python search paths so the interpreter can rebuild their native form.
#[cfg(unix)]
fn python_path_bytes(request: &ExecutionRequest) -> Vec<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    if request.language != CodeLanguage::Python {
        return Vec::new();
    }
    request.python_path.iter().map(|path| path.as_os_str().as_bytes().to_vec()).collect()
}

#[cfg(windows)]
fn python_path_bytes(request: &ExecutionRequest) -> Vec<Vec<u8>> {
    use std::os::windows::ffi::OsStrExt;
    if request.language != CodeLanguage::Python {
        return Vec::new();
    }
    request.python_path.iter().map(|path| path.as_os_str().encode_wide().flat_map(u16::to_le_bytes).collect()).collect()
}

#[cfg(not(any(unix, windows)))]
fn python_path_bytes(request: &ExecutionRequest) -> Vec<Vec<u8>> {
    if request.language != CodeLanguage::Python {
        return Vec::new();
    }
    request.python_path.iter().map(|path| path.to_string_lossy().into_owned().into_bytes()).collect()
}

/// Merge driver-written chunk files with output captured from inherited streams.
fn assemble_streams(files: &ChunkFiles, captured_stdout: &str, captured_stderr: &str, captured_truncated: bool) -> (String, String, bool) {
    let (mut stdout, mut stderr, mut truncated) = if files.has_stream_files {
        let (out, out_trunc) = read_text(&files.out, MAX_STREAM_BYTES);
        let (err, err_trunc) = read_text(&files.err, MAX_STREAM_BYTES);
        (out, err, captured_truncated || out_trunc || err_trunc)
    } else {
        (String::new(), String::new(), captured_truncated)
    };
    if append_bounded_text(&mut stdout, captured_stdout, MAX_STREAM_BYTES) {
        truncated = true;
    }
    if append_bounded_text(&mut stderr, captured_stderr, MAX_STREAM_BYTES) {
        truncated = true;
    }
    (stdout, stderr, truncated)
}

enum WaitOutcome {
    Finished,
    TimedOut,
    Terminated,
}

struct ChunkFiles {
    has_stream_files: bool,
    out: PathBuf,
    err: PathBuf,
    status: PathBuf,
    collector_error: PathBuf,
    png: PathBuf,
    svg: PathBuf,
}

impl ChunkFiles {
    fn new(dir: &Path, seq: u64, language: CodeLanguage) -> Self {
        Self {
            has_stream_files: language == CodeLanguage::Python,
            out: dir.join(format!("out-{seq}.txt")),
            err: dir.join(format!("err-{seq}.txt")),
            status: dir.join(format!("status-{seq}.txt")),
            collector_error: dir.join(format!("collector-error-{seq}.txt")),
            png: dir.join(format!("plot-{seq}.png")),
            svg: dir.join(format!("plot-{seq}.svg")),
        }
    }

    fn read_result(&self, request: &ExecutionRequest, duration_ms: u64, stdout: String, stderr: String, output_truncated: bool) -> ExecutionResult {
        let mut stderr = stderr;
        let mut output_truncated = output_truncated;
        let (status, _) = read_text(&self.status, 1024);
        let collector_error = if self.has_stream_files {
            let (err, _) = read_text(&self.collector_error, 4096);
            err.trim().to_string()
        } else {
            String::new()
        };
        let success = status.split_whitespace().next() == Some("ok") && collector_error.is_empty();
        if status.split_whitespace().any(|word| word == "truncated") {
            output_truncated = true;
        }
        if status.trim().is_empty() {
            if !stderr.is_empty() && !stderr.ends_with('\n') {
                stderr.push('\n');
            }
            stderr.push_str("the session did not report a status for this chunk");
        }
        if !collector_error.is_empty() {
            if !stderr.is_empty() && !stderr.ends_with('\n') {
                stderr.push('\n');
            }
            stderr.push_str("the session could not write captured output: ");
            stderr.push_str(&collector_error);
            stderr.push('\n');
        }

        let effective_format = request.effective_plot_format();
        let (plot_png, png_trunc) =
            if request.capture_plots && effective_format.captures_png() { read_png_bounded(&self.png, MAX_PLOT_BYTES) } else { (None, false) };
        let (plot_svg, svg_trunc) =
            if request.capture_plots && effective_format.captures_svg() { read_svg_bounded(&self.svg, MAX_PLOT_BYTES) } else { (None, false) };

        let output_truncated = output_truncated || png_trunc || svg_trunc;

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
        if self.has_stream_files {
            let _ = fs::remove_file(&self.out);
            let _ = fs::remove_file(&self.err);
            let _ = fs::remove_file(&self.collector_error);
        }
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

fn append_bounded_text(target: &mut String, source: &str, limit: usize) -> bool {
    if target.len() >= limit {
        return !source.is_empty();
    }
    let remaining_budget = limit - target.len();
    if source.len() <= remaining_budget {
        target.push_str(source);
        false
    } else {
        let valid_slice = truncate_to_char_boundary(source, remaining_budget);
        target.push_str(valid_slice);
        true
    }
}

fn read_text(path: &Path, limit: usize) -> (String, bool) {
    let Ok((buffer, truncated)) = read_bounded_bytes(path, limit) else {
        return (String::new(), false);
    };
    let s = String::from_utf8_lossy(&buffer);
    let truncated = truncated || s.len() > limit;
    let valid_slice = truncate_to_char_boundary(&s, limit);
    (valid_slice.to_string(), truncated)
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn tail(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.trim().to_string();
    }
    text.chars().skip(total - max_chars).collect::<String>().trim().to_string()
}

const PYTHON_DRIVER: &str = r##"import os

os.environ.setdefault("MPLBACKEND", "Agg")
import sys
import threading
import time
import traceback
import types

if os.name == "posix":
    import selectors
elif os.name == "nt":
    import ctypes
    import ctypes.wintypes as _wintypes
    import msvcrt
    import _winapi

    _KERNEL32 = ctypes.WinDLL("kernel32", use_last_error=True)
    _KERNEL32.SetStdHandle.argtypes = [_wintypes.DWORD, _wintypes.HANDLE]
    _KERNEL32.SetStdHandle.restype = _wintypes.BOOL
    _STD_HANDLES = {
        0: _winapi.STD_INPUT_HANDLE,
        1: _winapi.STD_OUTPUT_HANDLE,
        2: _winapi.STD_ERROR_HANDLE,
    }


def _redirect_std_fd(src_fd, dst_fd):
    os.dup2(src_fd, dst_fd)
    if os.name == "nt":
        std_id = _STD_HANDLES.get(dst_fd)
        if std_id is not None:
            _KERNEL32.SetStdHandle(std_id, msvcrt.get_osfhandle(dst_fd))


BASE = sys.argv[1] if len(sys.argv) > 1 else "."

PROTO_IN_FD = os.dup(0)
if hasattr(os, "set_inheritable"):
    os.set_inheritable(PROTO_IN_FD, False)
PROTO_IN = os.fdopen(PROTO_IN_FD, "rb", buffering=0)

PROTO_OUT_FD = os.dup(1)
if hasattr(os, "set_inheritable"):
    os.set_inheritable(PROTO_OUT_FD, False)
PROTO_OUT = os.fdopen(PROTO_OUT_FD, "wb", buffering=0)

DEVNULL_R = os.open(os.devnull, os.O_RDONLY)
DEVNULL_W = os.open(os.devnull, os.O_WRONLY)
_redirect_std_fd(DEVNULL_R, 0)
os.close(DEVNULL_R)
_redirect_std_fd(DEVNULL_W, 1)
_redirect_std_fd(DEVNULL_W, 2)
sys.stdin = open(os.devnull, "r", encoding="utf-8")

def write_all(fd, data):
    view = memoryview(data)
    written = 0
    while written < len(view):
        accepted = os.write(fd, view[written:])
        if accepted <= 0:
            raise OSError("short write to descriptor %d" % fd)
        written += accepted

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
        write_all(self.fd, to_write)
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
DIAG_STDERR = STABLE_STDERR

class BoundedSink:
    """Write at most max_bytes to a chunk file and keep draining excess."""

    def __init__(self, path, max_bytes=10 * 1024 * 1024 + 1):
        self._file = open(path, "wb", buffering=0)
        self._remaining = max_bytes
        self._lock = threading.Lock()
        self._closed = False
        self.truncated = False
        self.error = None

    def fail(self, exc):
        with self._lock:
            if self.error is None:
                self.error = exc
            self._remaining = 0

    def write(self, data):
        with self._lock:
            if self._closed:
                return
            if self._remaining <= 0:
                if data and self.error is None:
                    self.truncated = True
                return
            chunk = data[: self._remaining]
            try:
                write_all(self._file.fileno(), chunk)
            except Exception as exc:
                self.error = exc
                self._remaining = 0
                return
            self._remaining -= len(chunk)
            if len(chunk) < len(data):
                self.truncated = True

    def close(self):
        with self._lock:
            if self._closed:
                return
            self._closed = True
            try:
                self._file.close()
            except Exception:
                pass


def collect_stream_posix(read_fd, sink, stop_fd):
    os.set_blocking(read_fd, False)
    os.set_blocking(stop_fd, False)
    sel = selectors.DefaultSelector()
    sel.register(read_fd, selectors.EVENT_READ, "data")
    sel.register(stop_fd, selectors.EVENT_READ, "stop")
    try:
        while True:
            events = sel.select()
            stop_hit = False
            for key, _ in events:
                if key.data == "stop":
                    stop_hit = True
                else:
                    while True:
                        try:
                            data = os.read(read_fd, 65536)
                        except BlockingIOError:
                            break
                        except Exception as exc:
                            sink.fail(exc)
                            return
                        if not data:
                            return
                        sink.write(data)
            if stop_hit:
                # Drain bytes already buffered in the pipe before exiting so
                # background subprocesses holding the write end cannot stall EOF.
                while True:
                    try:
                        data = os.read(read_fd, 65536)
                    except BlockingIOError:
                        break
                    except Exception as exc:
                        sink.fail(exc)
                        return
                    if not data:
                        break
                    sink.write(data)
                return
    finally:
        try:
            sel.close()
        except Exception:
            pass
        for fd in (read_fd, stop_fd):
            try:
                os.close(fd)
            except Exception:
                pass


def collect_stream_blocking(read_fd, sink):
    try:
        while True:
            try:
                data = os.read(read_fd, 65536)
            except Exception as exc:
                sink.fail(exc)
                break
            if not data:
                break
            sink.write(data)
    finally:
        try:
            os.close(read_fd)
        except Exception:
            pass


plt = None
Gcf = None

def refresh_matplotlib():
    """Bind Matplotlib pyplot/Gcf once user code imports matplotlib.pyplot."""
    global plt, Gcf
    if plt is not None:
        return
    plt_module = sys.modules.get("matplotlib.pyplot")
    if plt_module is None:
        return
    plt = plt_module
    try:
        from matplotlib._pylab_helpers import Gcf as Gcf_module
        Gcf = Gcf_module
    except Exception:
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

def track_figure_draws():
    drawn = set()
    restorers = []
    for fig in get_all_figures():
        try:
            canvas = fig.canvas
            original_draw = canvas.draw
            def tracked_draw(*args, _draw=original_draw, _fig=fig, **kwargs):
                if bool(getattr(_fig, "stale", False)):
                    drawn.add(id(_fig))
                return _draw(*args, **kwargs)
            canvas.draw = tracked_draw
            restorers.append((canvas, original_draw, tracked_draw))
        except Exception:
            pass
    return drawn, restorers

def restore_figure_draws(restorers):
    for canvas, original_draw, tracked_draw in restorers:
        try:
            if canvas.draw is tracked_draw:
                canvas.draw = original_draw
        except Exception:
            pass

def capture_plots(seq, fmt, before_snaps, drawn_figures):
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
            if id(fig) in drawn_figures:
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
    while True:
        line = PROTO_IN.readline()
        if not line:
            return 0
        header = line.decode("utf-8", errors="replace").strip()
        parts = header.split()
        if len(parts) != 7 or parts[0] != "RUN":
            continue
        seq = parts[1]
        capture = parts[2] == "1"
        fmt = parts[3]
        cwd_len = int(parts[4])
        code_len = int(parts[5])
        paths_len = int(parts[6])

        cwd = ""
        if cwd_len > 0:
            cwd = read_exact(cwd_len).decode("utf-8", errors="replace")

        code = read_exact(code_len).decode("utf-8", errors="replace")

        python_paths = []
        if paths_len > 0:
            blob = read_exact(paths_len)
            position = 0
            while position + 4 <= len(blob):
                length = int.from_bytes(blob[position : position + 4], "little")
                position += 4
                raw = blob[position : position + length]
                position += length
                if os.name == "nt":
                    python_paths.append(raw.decode("utf-16-le", "surrogatepass"))
                else:
                    python_paths.append(os.fsdecode(raw))

        out_path = os.path.join(BASE, "out-%s.txt" % seq)
        err_path = os.path.join(BASE, "err-%s.txt" % seq)
        status_path = os.path.join(BASE, "status-%s.txt" % seq)

        out_sink = BoundedSink(out_path)
        err_sink = BoundedSink(err_path)
        out_read, out_write = os.pipe()
        err_read, err_write = os.pipe()
        _redirect_std_fd(out_write, 1)
        os.close(out_write)
        _redirect_std_fd(err_write, 2)
        os.close(err_write)
        if os.name == "posix":
            out_stop_r, out_stop_w = os.pipe()
            err_stop_r, err_stop_w = os.pipe()
            out_thread = threading.Thread(
                target=collect_stream_posix,
                args=(out_read, out_sink, out_stop_r),
                daemon=True,
            )
            err_thread = threading.Thread(
                target=collect_stream_posix,
                args=(err_read, err_sink, err_stop_r),
                daemon=True,
            )
        else:
            out_stop_w = None
            err_stop_w = None
            out_thread = threading.Thread(
                target=collect_stream_blocking,
                args=(out_read, out_sink),
                daemon=True,
            )
            err_thread = threading.Thread(
                target=collect_stream_blocking,
                args=(err_read, err_sink),
                daemon=True,
            )
        out_thread.start()
        err_thread.start()

        STABLE_STDOUT.reset_chunk()
        STABLE_STDERR.reset_chunk()
        sys.stdout = STABLE_STDOUT
        sys.stderr = STABLE_STDERR

        status = "ok"
        before_figures = {}
        drawn_figures = set()
        draw_restorers = []
        requested_paths = []
        original_positions = {}

        try:
            if cwd:
                os.chdir(cwd)
            for path in python_paths:
                if path not in requested_paths:
                    requested_paths.append(path)
            for path in requested_paths:
                if path in sys.path:
                    original_positions[path] = sys.path.index(path)
            for path in reversed(requested_paths):
                while path in sys.path:
                    sys.path.remove(path)
                sys.path.insert(0, path)
            refresh_matplotlib()
            before_figures = snapshot_figures()
            drawn_figures, draw_restorers = track_figure_draws()
            exec(compile(code + "\n", "<wordcraft-chunk>", "exec"), NAMESPACE)
        except SystemExit as exc:
            status = "error"
            try:
                DIAG_STDERR.write("The chunk tried to exit the session: %s\n" % exc)
            except Exception:
                pass
        except BaseException:
            status = "error"
            try:
                DIAG_STDERR.write(traceback.format_exc())
            except Exception:
                pass
        finally:
            # Capture runs while the request paths are still installed, because
            # savefig can fire draw_event callbacks that import user modules.
            try:
                restore_figure_draws(draw_restorers)
                refresh_matplotlib()
                if capture:
                    capture_plots(seq, fmt, before_figures, drawn_figures)
            finally:
                for path in requested_paths:
                    while path in sys.path:
                        sys.path.remove(path)
                for path, index in original_positions.items():
                    sys.path.insert(min(index, len(sys.path)), path)
            try:
                DIAG_STDERR.flush()
            except Exception:
                pass
            # Close the chunk's pipe write ends; child processes that inherited
            # them keep their own copies until they exit.
            try:
                _redirect_std_fd(DEVNULL_W, 1)
                _redirect_std_fd(DEVNULL_W, 2)
            except Exception:
                pass
            # Wake POSIX selectors so a background subprocess holding the write
            # ends does not keep this chunk waiting for EOF.
            for stop_w in (out_stop_w, err_stop_w):
                if stop_w is not None:
                    try:
                        os.write(stop_w, b"x")
                    except Exception:
                        pass
                    try:
                        os.close(stop_w)
                    except Exception:
                        pass
            out_thread.join(0.1)
            err_thread.join(0.1)
            out_sink.close()
            err_sink.close()
            sys.stdout = STABLE_STDOUT
            sys.stderr = STABLE_STDERR

        if not capture:
            for fig in get_all_figures():
                try:
                    fig.stale = False
                except Exception:
                    pass

        collector_errors = []
        if out_sink.error is not None:
            collector_errors.append("stdout: %s" % out_sink.error)
        if err_sink.error is not None:
            collector_errors.append("stderr: %s" % err_sink.error)
        if collector_errors:
            with open(
                os.path.join(BASE, "collector-error-%s.txt" % seq),
                "w",
                encoding="utf-8",
                errors="replace",
            ) as error_file:
                error_file.write("\n".join(collector_errors))

        status_text = status
        if (
            out_sink.truncated
            or err_sink.truncated
            or STABLE_STDOUT.truncated
            or STABLE_STDERR.truncated
        ):
            status_text += " truncated"

        with open(status_path, "w", encoding="utf-8") as sf:
            sf.write(status_text)

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

.wc_driver$read_exact <- function(con, n) {
  if (base::is.na(n) || n < 0L) {
    return(NULL)
  }
  if (n == 0L) {
    return(base::raw(0L))
  }
  chunks <- base::list()
  collected <- 0L
  while (collected < n) {
    chunk <- base::readBin(con, what = "raw", n = n - collected)
    if (base::length(chunk) == 0L) {
      return(NULL)
    }
    chunks[[base::length(chunks) + 1L]] <- chunk
    collected <- collected + base::length(chunk)
  }
  if (base::length(chunks) == 1L) chunks[[1L]] else base::unlist(chunks, use.names = FALSE)
}
base::environment(.wc_driver$read_exact) <- .wc_driver

.wc_driver$run <- function() {
  if (base::is.na(.wc_driver$port) || .wc_driver$port <= 0L) {
    base::stop("R session driver requires a valid TCP port")
  }
  con <- base::socketConnection(host = "127.0.0.1", port = .wc_driver$port, open = "r+b", blocking = TRUE, timeout = 2147483647L)
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
    if (base::length(parts) != 7L || !base::identical(parts[[1L]], "RUN")) next
    seq <- parts[[2L]]
    capture <- base::identical(parts[[3L]], "1")
    fmt <- parts[[4L]]
    cwd_len <- base::suppressWarnings(base::as.integer(parts[[5L]]))
    code_len <- base::suppressWarnings(base::as.integer(parts[[6L]]))
    paths_len <- base::suppressWarnings(base::as.integer(parts[[7L]]))

    cwd_raw <- .wc_driver$read_exact(con, cwd_len)
    if (base::is.null(cwd_raw)) break
    code_raw <- .wc_driver$read_exact(con, code_len)
    if (base::is.null(code_raw)) break
    if (!base::is.na(paths_len) && paths_len > 0L) {
      if (base::is.null(.wc_driver$read_exact(con, paths_len))) break
    }

    status_path <- base::file.path(.wc_driver$base, base::paste0("status-", seq, ".txt"))
    png_path <- base::file.path(.wc_driver$base, base::paste0("plot-", seq, ".png"))
    svg_path <- base::file.path(.wc_driver$base, base::paste0("plot-", seq, ".svg"))

    status <- "ok"
    error_message <- ""
    .wc_driver$has_plot <- FALSE

    cwd <- ""
    if (cwd_len > 0L) {
      cwd_res <- base::tryCatch({
        s <- base::rawToChar(cwd_raw)
        base::Encoding(s) <- "UTF-8"
        if (!base::validUTF8(s)) base::stop("cwd is not valid UTF-8")
        s
      }, error = base::identity)
      if (base::inherits(cwd_res, "error")) {
        status <- "error"
        error_message <- base::conditionMessage(cwd_res)
      } else {
        cwd <- cwd_res
      }
    }

    if (base::identical(status, "ok") && base::nzchar(cwd)) {
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

    if (!base::is.null(parsed)) {
      failed <- base::tryCatch({
        for (i in base::seq_along(parsed)) {
          val <- base::withVisible(base::eval(parsed[[i]], envir = .GlobalEnv))
          if (base::isTRUE(val$visible)) {
            base_print <- base::call("::", base::quote(base), base::quote(print))
            base_quote <- base::call("::", base::quote(base), base::quote(quote))
            quoted_value <- base::as.call(base::list(base_quote, val$value))
            print_call <- base::as.call(base::list(base_print, quoted_value))
            base::eval(print_call, envir = .GlobalEnv)
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
      base::try(base::cat("Error: ", error_message, "\n", sep = "", file = base::stderr()), silent = TRUE)
    }

    .wc_driver$close_sinks()

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
    base::try(base::flush(base::stdout()), silent = TRUE)
    base::try(base::flush(base::stderr()), silent = TRUE)
    base::writeLines(base::paste0("END ", seq), con, useBytes = TRUE)
    base::flush(con)
  }
}

base::environment(.wc_driver$run) <- .wc_driver
.wc_driver$run()
"##;

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(prefix: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "{prefix}-{}-{}",
                std::process::id(),
                SESSION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir(&dir).expect("create test dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn authentication_read_obeys_the_startup_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let address = listener.local_addr().expect("listener address");
        let client = std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(address).expect("connect client");
            for byte in b"AUTH token\n" {
                if stream.write_all(&[*byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let (mut stream, _) = listener.accept().expect("accept client");
        let started = Instant::now();
        let authenticated = read_auth_line(&mut stream, b"AUTH token", started + Duration::from_millis(50)).expect("read auth line");
        drop(stream);
        client.join().expect("join client");

        assert!(!authenticated);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    struct ChunkedReader {
        data: Vec<u8>,
        position: usize,
        chunk: usize,
    }

    impl Read for ChunkedReader {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.position >= self.data.len() {
                return Ok(0);
            }
            let end = (self.position + self.chunk).min(self.data.len());
            let slice = &self.data[self.position..end];
            let written = slice.len().min(out.len());
            out[..written].copy_from_slice(&slice[..written]);
            self.position += written;
            Ok(written)
        }
    }

    #[test]
    fn protocol_reader_survives_a_split_multibyte_character() {
        let mut data = vec![b'A'; 65_535];
        data.push(0xC3);
        data.extend_from_slice(b"\xA9\nEND 1\n");
        let (sender, receiver) = mpsc::sync_channel(8);
        let reader = ChunkedReader { data, position: 0, chunk: 2 };

        pump_protocol_lines(reader, sender);

        let first = receiver.recv().expect("first frame").expect("first frame data");
        assert!(first.starts_with('A'), "the frame before the split must survive");
        let second = receiver.recv().expect("split remainder").expect("split remainder data");
        assert!(second.ends_with('\n'), "the remainder must keep its line break");
        assert_eq!(receiver.recv().expect("END frame"), Some("END 1\n".to_string()));
        assert!(matches!(receiver.recv(), Ok(None)));
    }

    #[cfg(unix)]
    #[test]
    fn startup_guard_reaps_the_process_group_on_drop() {
        let dir = TestDir::new("wordcraft-child-guard");
        let marker = dir.path().join("descendant.marker");
        let ready = dir.path().join("descendant.ready");
        let script = dir.path().join("leader.sh");
        fs::write(&script, "(sh -c 'echo ready > \"$WC_READY\"; sleep 0.6; echo alive > \"$WC_MARKER\"' &)\nsleep 30\n")
            .expect("write leader script");

        let mut command = Command::new("sh");
        command.arg(&script).env("WC_READY", &ready).env("WC_MARKER", &marker).stdout(Stdio::null()).stderr(Stdio::null());
        let child = ChildGuard::new(command.group_spawn().expect("spawn leader"));

        let deadline = Instant::now() + Duration::from_secs(2);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "the background worker must report readiness before the guard is dropped");
        drop(child);

        std::thread::sleep(Duration::from_millis(900));
        assert!(!marker.exists(), "the startup guard must reap the interpreter process group");
    }

    #[test]
    fn collector_error_is_reported_in_the_result() {
        let dir = TestDir::new("wordcraft-collector-error");
        let files = ChunkFiles::new(dir.path(), 7, CodeLanguage::Python);
        fs::write(&files.status, b"ok").expect("write status");
        fs::write(&files.collector_error, b"stdout: no space left on device").expect("write collector error");

        let request = ExecutionRequest::new(CodeLanguage::Python, "print('x')");
        let result = files.read_result(&request, 1, "x\n".to_string(), String::new(), false);

        assert!(!result.success, "a collector write failure must not report success");
        assert!(result.stderr.contains("no space left on device"), "stderr was: {:?}", result.stderr);
        files.cleanup();
    }
}
