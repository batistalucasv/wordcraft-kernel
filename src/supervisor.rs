use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
#[cfg(not(windows))]
use std::process::Child;
use std::process::{ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(windows)]
use command_group::{CommandGroup, GroupChild};

const MAX_CAPTURE_BYTES: usize = 10 * 1024 * 1024;
#[cfg(target_os = "linux")]
const MAX_SUPERVISED_CHILDREN: usize = 65_536;
#[cfg(target_os = "linux")]
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);

struct CaptureSink {
    bytes: Arc<Mutex<Vec<u8>>>,
    truncated: Arc<AtomicBool>,
}

impl Clone for CaptureSink {
    fn clone(&self) -> Self {
        Self {
            bytes: Arc::clone(&self.bytes),
            truncated: Arc::clone(&self.truncated),
        }
    }
}

impl CaptureSink {
    fn new() -> Self {
        Self {
            bytes: Arc::new(Mutex::new(Vec::new())),
            truncated: Arc::new(AtomicBool::new(false)),
        }
    }

    fn is_truncated(&self) -> bool {
        self.truncated.load(Ordering::Acquire)
    }
}

struct WaitOutcome {
    status: Option<ExitStatus>,
    timed_out: bool,
    output_limited: bool,
}

enum ManagedChild {
    #[cfg(unix)]
    Unix(Child),
    #[cfg(windows)]
    Windows(GroupChild),
    #[cfg(not(any(unix, windows)))]
    Other(Child),
}

impl ManagedChild {
    fn id(&self) -> u32 {
        match self {
            #[cfg(unix)]
            Self::Unix(child) => child.id(),
            #[cfg(windows)]
            Self::Windows(child) => child.id(),
            #[cfg(not(any(unix, windows)))]
            Self::Other(child) => child.id(),
        }
    }

    fn take_stdout(&mut self) -> Option<ChildStdout> {
        match self {
            #[cfg(unix)]
            Self::Unix(child) => child.stdout.take(),
            #[cfg(windows)]
            Self::Windows(child) => child.inner().stdout.take(),
            #[cfg(not(any(unix, windows)))]
            Self::Other(child) => child.stdout.take(),
        }
    }

    fn take_stderr(&mut self) -> Option<ChildStderr> {
        match self {
            #[cfg(unix)]
            Self::Unix(child) => child.stderr.take(),
            #[cfg(windows)]
            Self::Windows(child) => child.inner().stderr.take(),
            #[cfg(not(any(unix, windows)))]
            Self::Other(child) => child.stderr.take(),
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self {
            #[cfg(unix)]
            Self::Unix(child) => child.try_wait(),
            #[cfg(windows)]
            Self::Windows(child) => child.try_wait(),
            #[cfg(not(any(unix, windows)))]
            Self::Other(child) => child.try_wait(),
        }
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        match self {
            #[cfg(unix)]
            Self::Unix(child) => child.wait(),
            #[cfg(windows)]
            Self::Windows(child) => child.wait(),
            #[cfg(not(any(unix, windows)))]
            Self::Other(child) => child.wait(),
        }
    }
}

fn spawn_child(command: &mut Command) -> io::Result<ManagedChild> {
    #[cfg(unix)]
    {
        #[cfg(not(target_os = "linux"))]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        command.spawn().map(ManagedChild::Unix)
    }
    #[cfg(windows)]
    {
        command.group_spawn().map(ManagedChild::Windows)
    }
    #[cfg(not(any(unix, windows)))]
    {
        command.spawn().map(ManagedChild::Other)
    }
}

pub fn run() -> io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    if args.next() != Some(OsString::from("--run")) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected --run",
        ));
    }
    let binary = next_path(&mut args, "interpreter path")?;
    let script = next_path(&mut args, "script path")?;
    let result_file = next_path(&mut args, "result path")?;
    let timeout_ms = args
        .next()
        .and_then(|arg| arg.into_string().ok())
        .and_then(|arg| arg.parse::<u64>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid timeout"))?;
    if args.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unexpected arguments",
        ));
    }

    #[cfg(target_os = "linux")]
    nix::sys::prctl::set_child_subreaper(true)
        .map_err(|error| io::Error::other(format!("enable child subreaper: {error}")))?;

    let result = execute_child(&binary, &script, Duration::from_millis(timeout_ms))
        .map_err(|error| io::Error::other(format!("supervise interpreter: {error}")))?;
    write_result(&result_file, result)
        .map_err(|error| io::Error::other(format!("write result: {error}")))
}

fn next_path(args: &mut impl Iterator<Item = OsString>, name: &str) -> io::Result<PathBuf> {
    args.next()
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("missing {name}")))
}

struct SupervisorResult {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: Option<ExitStatus>,
    timed_out: bool,
    output_truncated: bool,
    duration_ms: u64,
}

fn execute_child(binary: &Path, script: &Path, timeout: Duration) -> io::Result<SupervisorResult> {
    let mut command = Command::new(binary);
    command
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let start = Instant::now();
    let mut child = spawn_child(&mut command)
        .map_err(|error| io::Error::other(format!("spawn interpreter: {error}")))?;
    let child_pid = child.id();

    let stdout_pipe = child.take_stdout();
    let stderr_pipe = child.take_stderr();
    let (Some(stdout_pipe), Some(stderr_pipe)) = (stdout_pipe, stderr_pipe) else {
        return Err(cleanup_after_failure(
            io::Error::other("interpreter output pipes were not created"),
            cleanup_process_tree(&mut child, child_pid, false),
            None,
        ));
    };

    if let Err(error) =
        set_pipe_nonblocking(&stdout_pipe).and_then(|()| set_pipe_nonblocking(&stderr_pipe))
    {
        return Err(cleanup_after_failure(
            error,
            cleanup_process_tree(&mut child, child_pid, false),
            None,
        ));
    }

    let (stdout_sink, stdout_cancel, stdout_reader) =
        match spawn_capture(stdout_pipe, MAX_CAPTURE_BYTES) {
            Ok(reader) => reader,
            Err(error) => {
                return Err(cleanup_after_failure(
                    error,
                    cleanup_process_tree(&mut child, child_pid, false),
                    None,
                ));
            }
        };
    let (stderr_sink, stderr_cancel, stderr_reader) =
        match spawn_capture(stderr_pipe, MAX_CAPTURE_BYTES) {
            Ok(reader) => reader,
            Err(error) => {
                let cleanup = cleanup_process_tree(&mut child, child_pid, false);
                stdout_cancel.store(true, Ordering::Release);
                let reader_cleanup = join_capture_reader(stdout_reader);
                return Err(cleanup_after_failure(error, cleanup, Some(reader_cleanup)));
            }
        };

    let outcome = wait_with_timeout(&mut child, timeout, &stdout_sink, &stderr_sink)
        .map_err(|error| io::Error::other(format!("wait for interpreter: {error}")));
    let child_reaped = outcome
        .as_ref()
        .is_ok_and(|outcome| outcome.status.is_some());
    let process_cleanup = cleanup_process_tree(&mut child, child_pid, child_reaped);
    stdout_cancel.store(true, Ordering::Release);
    stderr_cancel.store(true, Ordering::Release);
    let stdout_cleanup = join_capture_reader(stdout_reader);
    let stderr_cleanup = join_capture_reader(stderr_reader);

    let outcome = outcome?;
    process_cleanup?;
    stdout_cleanup?;
    stderr_cleanup?;

    let stdout = stdout_sink
        .bytes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let stderr = stderr_sink
        .bytes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();

    Ok(SupervisorResult {
        stdout,
        stderr,
        status: outcome.status,
        timed_out: outcome.timed_out,
        output_truncated: outcome.output_limited
            || stdout_sink.is_truncated()
            || stderr_sink.is_truncated(),
        duration_ms: start.elapsed().as_millis().min(u64::MAX as u128) as u64,
    })
}

fn cleanup_after_failure(
    primary: io::Error,
    process_cleanup: io::Result<()>,
    reader_cleanup: Option<io::Result<()>>,
) -> io::Error {
    let process_error = process_cleanup.err().map(|error| error.to_string());
    let reader_error = reader_cleanup
        .and_then(Result::err)
        .map(|error| error.to_string());
    let details = [process_error, reader_error]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if details.is_empty() {
        primary
    } else {
        io::Error::other(format!("{primary}; cleanup failed: {}", details.join("; ")))
    }
}

fn spawn_capture<R>(
    mut pipe: R,
    limit: usize,
) -> io::Result<(CaptureSink, Arc<AtomicBool>, JoinHandle<io::Result<()>>)>
where
    R: Read + Send + 'static,
{
    let sink = CaptureSink::new();
    let thread_sink = sink.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let thread_cancel = Arc::clone(&cancel);
    let reader = thread::Builder::new()
        .name("wordcraft-output-capture".to_string())
        .spawn(move || -> io::Result<()> {
            let mut chunk = [0_u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => return Ok(()),
                    Ok(read) => {
                        let mut bytes = thread_sink
                            .bytes
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        let remaining = limit.saturating_sub(bytes.len());
                        if read > remaining {
                            bytes.extend_from_slice(&chunk[..remaining]);
                            drop(bytes);
                            thread_sink.truncated.store(true, Ordering::Release);
                            return Ok(());
                        }
                        bytes.extend_from_slice(&chunk[..read]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if thread_cancel.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => return Err(error),
                }
            }
        })?;
    Ok((sink, cancel, reader))
}

#[cfg(unix)]
fn set_pipe_nonblocking(pipe: &impl std::os::fd::AsRawFd) -> io::Result<()> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};

    let flags = fcntl(pipe.as_raw_fd(), FcntlArg::F_GETFL).map_err(io::Error::other)?;
    let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(pipe.as_raw_fd(), FcntlArg::F_SETFL(flags))
        .map(|_| ())
        .map_err(io::Error::other)
}

#[cfg(not(unix))]
fn set_pipe_nonblocking(_pipe: &impl Read) -> io::Result<()> {
    Ok(())
}

fn join_capture_reader(reader: JoinHandle<io::Result<()>>) -> io::Result<()> {
    match reader.join() {
        Ok(result) => result,
        Err(_) => Err(io::Error::other("output capture thread panicked")),
    }
}

fn wait_with_timeout(
    child: &mut ManagedChild,
    timeout: Duration,
    stdout_sink: &CaptureSink,
    stderr_sink: &CaptureSink,
) -> io::Result<WaitOutcome> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(WaitOutcome {
                status: Some(status),
                timed_out: false,
                output_limited: false,
            });
        }
        if start.elapsed() >= timeout {
            return Ok(WaitOutcome {
                status: None,
                timed_out: true,
                output_limited: stdout_sink.is_truncated() || stderr_sink.is_truncated(),
            });
        }
        if stdout_sink.is_truncated() || stderr_sink.is_truncated() {
            return Ok(WaitOutcome {
                status: child.try_wait()?,
                timed_out: false,
                output_limited: true,
            });
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn terminate_process_group(
    child: &mut ManagedChild,
    _child_pid: u32,
    already_reaped: bool,
) -> io::Result<()> {
    let reaped = already_reaped || child.try_wait()?.is_some();
    #[cfg(target_os = "linux")]
    let group_kill = if reaped {
        Ok(())
    } else {
        match child {
            ManagedChild::Unix(child) => child.kill(),
        }
    };
    #[cfg(all(unix, not(target_os = "linux")))]
    let group_kill = {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;

        let pid = i32::try_from(_child_pid)
            .map(Pid::from_raw)
            .map_err(io::Error::other)?;
        match killpg(pid, Signal::SIGKILL) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(error) => Err(io::Error::other(error)),
        }
    };
    #[cfg(windows)]
    let group_kill = {
        match child {
            ManagedChild::Windows(child) => match child.kill() {
                Ok(()) => Ok(()),
                Err(error) if reaped && error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            },
        }
    };
    #[cfg(not(any(unix, windows)))]
    let group_kill = match child {
        ManagedChild::Other(child) => child.kill(),
    };

    let wait = if reaped {
        Ok(())
    } else {
        child.wait().map(|_| ())
    };

    match (group_kill, wait) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(group_error), Err(wait_error)) => Err(io::Error::other(format!(
            "kill process group: {group_error}; wait for interpreter: {wait_error}"
        ))),
    }
}

fn cleanup_process_tree(
    child: &mut ManagedChild,
    child_pid: u32,
    already_reaped: bool,
) -> io::Result<()> {
    let process_cleanup = terminate_process_group(child, child_pid, already_reaped);
    #[cfg(target_os = "linux")]
    let descendants_cleanup = kill_adopted_children();
    #[cfg(not(target_os = "linux"))]
    let descendants_cleanup: io::Result<()> = Ok(());

    match (process_cleanup, descendants_cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(io::Error::other(format!("process group: {error}"))),
        (Ok(()), Err(error)) => Err(io::Error::other(format!("adopted descendants: {error}"))),
        (Err(process_error), Err(descendants_error)) => Err(io::Error::other(format!(
            "process group cleanup: {process_error}; adopted child cleanup: {descendants_error}"
        ))),
    }
}

#[cfg(target_os = "linux")]
fn kill_adopted_children() -> io::Result<()> {
    use nix::errno::Errno;
    use nix::sys::signal::{Signal, kill};
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    use nix::unistd::Pid;

    let started = Instant::now();
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Err(Errno::ECHILD) => return Ok(()),
            Ok(WaitStatus::StillAlive) => {}
            Ok(_) => continue,
            Err(error) => return Err(io::Error::other(error)),
        }
        let children = direct_children()?;
        for child in children {
            match kill(Pid::from_raw(child), Signal::SIGKILL) {
                Ok(()) | Err(Errno::ESRCH) => {}
                Err(error) => return Err(io::Error::other(error)),
            }
        }
        if started.elapsed() >= CLEANUP_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "descendant processes did not exit after termination",
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(target_os = "linux")]
fn direct_children() -> io::Result<Vec<i32>> {
    let pid = std::process::id();
    let path = format!("/proc/{pid}/task/{pid}/children");
    let mut file = fs::File::open(path)?;
    let mut content = String::new();
    Read::by_ref(&mut file)
        .take(1024 * 1024)
        .read_to_string(&mut content)?;
    if content.len() >= 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "supervisor child list exceeds its limit",
        ));
    }
    let mut children = Vec::new();
    for token in content.split_whitespace() {
        if children.len() >= MAX_SUPERVISED_CHILDREN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "supervisor child count exceeds its limit",
            ));
        }
        let child = token
            .parse::<i32>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        children.push(child);
    }
    Ok(children)
}

fn write_result(path: &Path, result: SupervisorResult) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let mut flags = 0_u8;
    if result.timed_out {
        flags |= 1;
    }
    if result.output_truncated {
        flags |= 2;
    }
    let exit_code = result
        .status
        .and_then(|status| status.code())
        .unwrap_or(i32::MIN);
    file.write_all(b"WCK1")?;
    file.write_all(&[flags])?;
    file.write_all(&exit_code.to_le_bytes())?;
    file.write_all(&result.duration_ms.to_le_bytes())?;
    file.write_all(&(result.stdout.len() as u32).to_le_bytes())?;
    file.write_all(&(result.stderr.len() as u32).to_le_bytes())?;
    file.write_all(&result.stdout)?;
    file.write_all(&result.stderr)?;
    file.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_sink_caps_single_burst() -> io::Result<()> {
        let burst = vec![b'x'; MAX_CAPTURE_BYTES + 1024];
        let (sink, _, reader) = spawn_capture(io::Cursor::new(burst), MAX_CAPTURE_BYTES)?;

        reader
            .join()
            .map_err(|_| io::Error::other("capture reader thread panicked"))??;
        assert!(sink.is_truncated());
        assert_eq!(
            sink.bytes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            MAX_CAPTURE_BYTES
        );
        Ok(())
    }
}
