use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant};

fn binary_path() -> &'static str {
    env!("CARGO_BIN_EXE_wc-kernel")
}

fn supervisor_binary_path() -> &'static str {
    env!("CARGO_BIN_EXE_wc-kernel-supervisor")
}

#[test]
fn supervisor_helper_is_built_with_the_cli() {
    assert!(PathBuf::from(supervisor_binary_path()).is_file());
}

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(prefix: &str) -> Self {
        for attempt in 0..64 {
            let path =
                std::env::temp_dir().join(format!("{prefix}-{}-{attempt}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create temp dir {}: {e}", path.display()),
            }
        }
        panic!("could not create an exclusive temp dir");
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn python_has_matplotlib() -> bool {
    let Some(python) = wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python)
    else {
        return false;
    };
    let output = Command::new(python)
        .args([
            "-c",
            "import importlib.util, sys; sys.exit(42 if importlib.util.find_spec('matplotlib') is None else 0)",
        ])
        .output()
        .expect("probe matplotlib with the configured Python interpreter");
    match output.status.code() {
        Some(0) => true,
        Some(42) => false,
        code => panic!("matplotlib probe failed unexpectedly with {code:?}"),
    }
}

#[test]
fn cli_help_flag() {
    let output = Command::new(binary_path())
        .arg("--help")
        .output()
        .expect("failed to execute wc-kernel");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("USAGE:"));
    assert!(stdout.contains("--lang"));
    assert!(stdout.contains("--eval"));
    assert!(stdout.contains("--json"));
    assert!(stdout.contains("--out-plot"));
}

#[test]
fn cli_version_flag() {
    let output = Command::new(binary_path())
        .arg("--version")
        .output()
        .expect("failed to execute wc-kernel");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("wc-kernel 0.1.0"));
}

#[test]
fn cli_acceptance_r_eval_basic() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::R).is_none() {
        eprintln!("Skipping R test: Rscript not found");
        return;
    }

    let output = Command::new(binary_path())
        .args(["--lang", "r", "-e", "print(1+1)"])
        .output()
        .expect("failed to execute wc-kernel");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[1] 2"));
}

#[test]
fn cli_acceptance_r_eval_json() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::R).is_none() {
        eprintln!("Skipping R test: Rscript not found");
        return;
    }

    let output = Command::new(binary_path())
        .args(["--lang", "r", "-e", "print(1+1)", "--json"])
        .output()
        .expect("failed to execute wc-kernel");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("output must be valid JSON");

    assert_eq!(json["success"], true);
    assert_eq!(json["timed_out"], false);
    assert_eq!(json["exit_code"], 0);
    assert!(
        json["stdout"]
            .as_str()
            .expect("stdout string")
            .contains("[1] 2")
    );
}

#[test]
fn cli_python_eval_basic() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python).is_none() {
        eprintln!("Skipping Python test: python3 not found");
        return;
    }

    let output = Command::new(binary_path())
        .args(["--lang", "py", "-e", "print(10 + 25)"])
        .output()
        .expect("failed to execute wc-kernel");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "35");
}

#[test]
fn cli_python_eval_json() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python).is_none() {
        eprintln!("Skipping Python test: python3 not found");
        return;
    }

    let output = Command::new(binary_path())
        .args(["--lang", "py", "-e", "print('hello json')", "--json"])
        .output()
        .expect("failed to execute wc-kernel");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("output must be valid JSON");

    assert_eq!(json["success"], true);
    assert_eq!(json["timed_out"], false);
    assert_eq!(json["exit_code"], 0);
    assert!(
        json["stdout"]
            .as_str()
            .expect("stdout string")
            .contains("hello json")
    );
}

#[cfg(unix)]
fn assert_process_stopped(pid: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let running = Command::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !running {
            return;
        }
        if Instant::now() >= deadline {
            let _ = Command::new("kill")
                .args(["-KILL", pid])
                .stderr(Stdio::null())
                .status();
            panic!("interpreter {pid} survived cancellation");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn assert_signal_cleans_execution(signal: nix::sys::signal::Signal, prefix: &str) {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python).is_none() {
        return;
    }

    let fixture = TestDir::new(prefix);
    let working_dir = fixture.join("cwd");
    let scratch_dir = fixture.join("scratch");
    fs::create_dir(&working_dir).expect("create working directory");
    fs::create_dir(&scratch_dir).expect("create scratch directory");

    let code = "from pathlib import Path\nimport os, time\nPath('interpreter.pid').write_text(str(os.getpid()))\ntime.sleep(30)";
    let mut child = Command::new(binary_path())
        .args(["--lang", "py", "--eval", code, "--cwd"])
        .arg(&working_dir)
        .arg("--no-plot")
        .env("TMPDIR", &scratch_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn wc-kernel");

    let pid_file = working_dir.join("interpreter.pid");
    let startup_deadline = Instant::now() + Duration::from_secs(10);
    while !pid_file.is_file() {
        if let Some(status) = child.try_wait().expect("check wc-kernel status") {
            panic!("wc-kernel exited before starting user code: {status}");
        }
        assert!(
            Instant::now() < startup_deadline,
            "user code did not start before the deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let interpreter_pid = fs::read_to_string(&pid_file).expect("read interpreter pid");
    let interpreter_pid = interpreter_pid.trim().to_string();

    let pid = i32::try_from(child.id()).expect("wc-kernel PID fits in i32");
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal).expect("signal wc-kernel");

    let exit_deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("check wc-kernel status") {
            break status;
        }
        if Instant::now() >= exit_deadline {
            child.kill().expect("stop unresponsive wc-kernel");
            let _ = child.wait();
            panic!("wc-kernel did not exit after {signal:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    assert_eq!(status.code(), Some(130));
    assert_eq!(
        fs::read_dir(&scratch_dir)
            .expect("read scratch directory")
            .count(),
        0,
        "execution workspace was not cleaned up after {signal:?}"
    );
    assert_process_stopped(&interpreter_pid);
}

#[cfg(unix)]
#[test]
fn cli_ctrl_c_cleans_execution_workspace_and_supervisor() {
    assert_signal_cleans_execution(nix::sys::signal::Signal::SIGINT, "wc-kernel-interrupt");
}

#[cfg(unix)]
#[test]
fn cli_sigterm_cleans_execution_workspace_and_supervisor() {
    assert_signal_cleans_execution(nix::sys::signal::Signal::SIGTERM, "wc-kernel-terminate");
}

#[cfg(unix)]
#[test]
fn cli_ctrl_c_during_stdin_read_exits_without_running_code() {
    let fixture = TestDir::new("wc-kernel-stdin-interrupt");
    let scratch_dir = fixture.join("scratch");
    fs::create_dir(&scratch_dir).expect("create scratch directory");

    let mut child = Command::new(binary_path())
        .args(["--lang", "py", "--stdin"])
        .env("TMPDIR", &scratch_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn wc-kernel");

    // Keep the write end open, so the CLI stays blocked on stdin.
    let stdin = child.stdin.take().expect("stdin piped");
    std::thread::sleep(Duration::from_millis(300));

    let pid = i32::try_from(child.id()).expect("wc-kernel PID fits in i32");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGINT,
    )
    .expect("send SIGINT to wc-kernel");

    let exit_deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("check wc-kernel status") {
            break status;
        }
        if Instant::now() >= exit_deadline {
            child.kill().expect("stop unresponsive wc-kernel");
            let _ = child.wait();
            panic!("wc-kernel did not stop reading stdin after SIGINT");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(stdin);

    assert_eq!(status.code(), Some(130));
    assert_eq!(
        fs::read_dir(&scratch_dir)
            .expect("read scratch directory")
            .count(),
        0,
        "no workspace should be created for cancelled stdin input"
    );
}

#[cfg(unix)]
#[test]
fn cli_ctrl_c_interrupts_a_blocked_output_write() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python).is_none() {
        return;
    }

    let fixture = TestDir::new("wc-output-block");
    let scratch_dir = fixture.join("scratch");
    fs::create_dir(&scratch_dir).expect("create scratch directory");

    // 4 MiB exceeds the pipe capacity, so the CLI cannot finish writing it.
    let mut child = Command::new(binary_path())
        .args([
            "--lang",
            "py",
            "-e",
            "print('x' * (4 * 1024 * 1024))",
            "--no-plot",
        ])
        .env("TMPDIR", &scratch_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn wc-kernel");

    // Keep the read end open and never drain it; readability proves the CLI
    // has started writing into the pipe.
    use std::os::fd::AsFd;
    let stdout = child.stdout.take().expect("stdout piped");
    let poll_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut fds = [nix::poll::PollFd::new(
            stdout.as_fd(),
            nix::poll::PollFlags::POLLIN,
        )];
        match nix::poll::poll(&mut fds, 100u16) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(error) => panic!("poll wc-kernel stdout: {error}"),
        }
        if fds[0]
            .revents()
            .is_some_and(|flags| flags.contains(nix::poll::PollFlags::POLLIN))
        {
            break;
        }
        if Instant::now() >= poll_deadline {
            child.kill().expect("stop blocked wc-kernel");
            let _ = child.wait();
            drop(stdout);
            panic!("wc-kernel did not write to the blocked pipe");
        }
    }

    let pid = i32::try_from(child.id()).expect("wc-kernel PID fits in i32");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGINT,
    )
    .expect("send SIGINT to wc-kernel");

    let exit_deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("check wc-kernel status") {
            break status;
        }
        if Instant::now() >= exit_deadline {
            child.kill().expect("stop unresponsive wc-kernel");
            let _ = child.wait();
            drop(stdout);
            panic!("wc-kernel stayed blocked while writing output");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(stdout);

    assert_eq!(status.code(), Some(130));
    assert_eq!(
        fs::read_dir(&scratch_dir)
            .expect("read scratch directory")
            .count(),
        0,
        "execution workspace was not cleaned up after SIGINT"
    );
}

#[cfg(unix)]
#[test]
fn cli_closed_stdout_pipe_does_not_panic() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python).is_none() {
        return;
    }

    let mut child = Command::new(binary_path())
        .args(["--lang", "py", "-e", "print('x' * 65536)", "--no-plot"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wc-kernel");
    drop(child.stdout.take().expect("stdout piped"));

    let output = child.wait_with_output().expect("wait for wc-kernel");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "a closed output pipe must not fail the run: {stderr}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn cli_closed_stdout_pipe_keeps_the_script_exit_status() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python).is_none() {
        return;
    }

    let mut child = Command::new(binary_path())
        .args([
            "--lang",
            "py",
            "-e",
            "import sys\nprint('x' * 65536)\nsys.exit(3)",
            "--no-plot",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wc-kernel");
    drop(child.stdout.take().expect("stdout piped"));

    let output = child.wait_with_output().expect("wait for wc-kernel");
    assert_eq!(
        output.status.code(),
        Some(3),
        "a closed pipe must not replace the script exit status"
    );
}

#[cfg(unix)]
#[test]
fn cli_resolves_interpreter_before_changing_directory() {
    let Some(python) = wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python)
    else {
        return;
    };

    let fixture = TestDir::new("wc-relative-interpreter");
    let bin_dir = fixture.join("bin");
    let working_dir = fixture.join("cwd");
    fs::create_dir(&bin_dir).expect("create bin directory");
    fs::create_dir(&working_dir).expect("create working directory");
    std::os::unix::fs::symlink(
        fs::canonicalize(python).expect("canonical python"),
        bin_dir.join("python3"),
    )
    .expect("link python3 into the relative PATH entry");

    let output = Command::new(binary_path())
        .args([
            "--lang",
            "py",
            "-e",
            "print('relative path works')",
            "--no-plot",
            "--cwd",
        ])
        .arg(&working_dir)
        .current_dir(&fixture.path)
        .env("PATH", "bin")
        .output()
        .expect("execute wc-kernel");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "relative path works"
    );
}

#[test]
fn cli_stdin_pipe_execution() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python).is_none() {
        eprintln!("Skipping Python test: python3 not found");
        return;
    }

    let mut child = Command::new(binary_path())
        .args(["--lang", "py"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn child");

    {
        let stdin = child.stdin.as_mut().expect("stdin piped");
        stdin
            .write_all(b"x = 5\ny = 7\nprint(x * y)\n")
            .expect("write to stdin");
    }

    let output = child.wait_with_output().expect("wait with output");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "35");
}

#[test]
fn cli_file_execution_and_inference() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python).is_none() {
        eprintln!("Skipping Python test: python3 not found");
        return;
    }

    let temp_dir = TestDir::new("wc-file-inference");
    let script_file = temp_dir.join("test_script.py");
    fs::write(&script_file, "print('inferred python works')").expect("write test script");

    // Execute without --lang, inferring from .py extension
    let output = Command::new(binary_path())
        .args(["--file", script_file.to_str().expect("valid path")])
        .output()
        .expect("failed to execute wc-kernel");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "inferred python works");
}

#[test]
fn cli_out_plot_png_saved() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::R).is_none() {
        eprintln!("Skipping R test: Rscript not found");
        return;
    }

    let temp_dir = TestDir::new("wc-plot");
    let plot_file = temp_dir.join("generated_plot.png");

    let output = Command::new(binary_path())
        .args([
            "--lang",
            "r",
            "-e",
            "plot(1:10, 1:10, main='CLI Plot Test')",
            "--out-plot",
            plot_file.to_str().expect("valid path"),
        ])
        .output()
        .expect("failed to execute wc-kernel");

    assert!(output.status.success());
    assert!(plot_file.is_file(), "plot file should have been written");
    let bytes = fs::read(&plot_file).expect("read plot file");
    assert!(
        bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
        "file must be a valid PNG"
    );
}

#[test]
fn cli_out_plot_svg_saved() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::R).is_none() {
        eprintln!("Skipping R test: Rscript not found");
        return;
    }

    let temp_dir = TestDir::new("wc-svg");
    let plot_file = temp_dir.join("generated_plot.svg");

    let output = Command::new(binary_path())
        .args([
            "--lang",
            "r",
            "-e",
            "plot(1:10, 1:10, main='CLI SVG Test')",
            "--out-plot",
            plot_file.to_str().expect("valid path"),
        ])
        .output()
        .expect("failed to execute wc-kernel");

    assert!(output.status.success());
    assert!(
        plot_file.is_file(),
        "svg plot file should have been written"
    );
    let content = fs::read_to_string(&plot_file).expect("read svg plot file");
    assert!(
        content.contains("<svg") && content.contains("</svg>"),
        "file must be a valid SVG"
    );
}

#[test]
fn cli_defaults_to_current_directory() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python).is_none() {
        eprintln!("Skipping Python test: python3 not found");
        return;
    }

    let output = Command::new(binary_path())
        .args([
            "--lang",
            "py",
            "-e",
            "import os; print(os.path.realpath(os.getcwd()))",
        ])
        .output()
        .expect("failed to execute wc-kernel");

    assert!(output.status.success());
    let printed = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let printed = fs::canonicalize(printed).expect("canonical printed dir");
    let expected = fs::canonicalize(std::env::current_dir().expect("current dir"))
        .expect("canonical current dir");
    assert_eq!(printed, expected);
}

#[test]
fn cli_file_execution_resolves_sibling_imports() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python).is_none() {
        eprintln!("Skipping Python test: python3 not found");
        return;
    }

    let temp_dir = TestDir::new("wc-sibling-import");
    fs::write(temp_dir.join("helper_module.py"), "VALUE = 42\n").expect("write helper module");
    let script_file = temp_dir.join("main_script.py");
    fs::write(
        &script_file,
        "import helper_module\nprint(helper_module.VALUE)\n",
    )
    .expect("write main script");

    let output = Command::new(binary_path())
        .args([
            "--file",
            script_file.to_str().expect("valid path"),
            "--no-plot",
        ])
        .output()
        .expect("failed to execute wc-kernel");

    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[cfg(unix)]
#[test]
fn cli_accepts_non_utf8_file_paths() {
    use std::os::unix::ffi::OsStringExt;

    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python).is_none() {
        return;
    }

    let temp_dir = TestDir::new("wc-non-utf8-file");
    let script_dir = temp_dir
        .path
        .join(std::ffi::OsString::from_vec(b"dir-\xff".to_vec()));
    fs::create_dir(&script_dir).expect("create non-UTF-8 directory");
    fs::write(script_dir.join("helper.py"), "VALUE = 42\n").expect("write helper module");
    let script = script_dir.join("script.py");
    fs::write(&script, "import helper\nprint(helper.VALUE)\n").expect("write script");

    let output = Command::new(binary_path())
        .args(["--no-plot", "--file"])
        .arg(&script)
        .output()
        .expect("run CLI");

    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
fn cli_export_error_still_emits_json() {
    if !python_has_matplotlib() {
        eprintln!("Skipping plot export test: matplotlib not found");
        return;
    }

    let temp_dir = TestDir::new("wc-export-error");
    let blocker = temp_dir.join("blocker");
    fs::write(&blocker, "not a directory").expect("write blocker file");
    let plot_file = blocker.join("plot.png");

    let output = Command::new(binary_path())
        .args([
            "--lang",
            "py",
            "-e",
            "import matplotlib.pyplot as plt\nplt.plot([1, 2, 3])",
            "--json",
            "--out-plot",
            plot_file.to_str().expect("valid path"),
        ])
        .output()
        .expect("failed to execute wc-kernel");

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("result JSON must be emitted before the export error");
    assert_eq!(json["success"], true);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("failed to create directory"));
}

#[test]
fn cli_reports_truncated_output_in_json() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python).is_none() {
        eprintln!("Skipping Python test: python3 not found");
        return;
    }

    let output = Command::new(binary_path())
        .args([
            "--lang",
            "py",
            "-e",
            "import sys; sys.stdout.write('x' * (12 * 1024 * 1024))",
            "--json",
            "--no-plot",
        ])
        .output()
        .expect("failed to execute wc-kernel");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON result");
    assert_eq!(json["output_truncated"], true);
    assert!(json["stdout"].as_str().expect("stdout string").len() <= 10 * 1024 * 1024);
}

#[test]
fn cli_timeout_enforced() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python).is_none() {
        eprintln!("Skipping Python test: python3 not found");
        return;
    }

    let output = Command::new(binary_path())
        .args([
            "--lang",
            "py",
            "-e",
            "import time\nwhile True:\n    time.sleep(0.05)",
            "--timeout",
            "300",
        ])
        .output()
        .expect("failed to execute wc-kernel");

    assert!(!output.status.success());
    // Unix timeout exit code is typically 124
    let code = output.status.code().expect("exit code");
    assert_eq!(code, 124);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("timed out after 300 ms"));
}

#[test]
fn cli_syntax_error_returns_nonzero_and_json() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python).is_none() {
        eprintln!("Skipping Python test: python3 not found");
        return;
    }

    let output = Command::new(binary_path())
        .args(["--lang", "py", "-e", "def broken(: syntax error", "--json"])
        .output()
        .expect("failed to execute wc-kernel");

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("output must still be valid JSON");

    assert_eq!(json["success"], false);
    assert_ne!(json["exit_code"], 0);
    assert!(
        json["stderr"]
            .as_str()
            .expect("stderr string")
            .contains("SyntaxError")
    );
}

#[test]
fn cli_invalid_arguments_handled() {
    let output = Command::new(binary_path())
        .args(["--nonexistent-option"])
        .output()
        .expect("failed to execute wc-kernel");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unrecognized option"));
}

#[test]
fn cli_missing_language_handled() {
    let output = Command::new(binary_path())
        .args(["-e", "print(1)"])
        .output()
        .expect("failed to execute wc-kernel");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing required argument --lang"));
}
