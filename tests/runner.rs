use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::process::Stdio;

use wordcraft_kernel::{
    CodeLanguage, ExecutionRequest, ExecutionResult, KernelError, PlotFormat,
    execute as kernel_execute, is_language_available, which_binary,
};

const MAX_CAPTURE_BYTES: usize = 10 * 1024 * 1024;

fn execute(request: &ExecutionRequest) -> Result<ExecutionResult, KernelError> {
    let request = if request.supervisor_path.is_some() {
        request.clone()
    } else {
        request
            .clone()
            .with_supervisor_path(env!("CARGO_BIN_EXE_wc-kernel-supervisor"))
    };
    kernel_execute(&request)
}

struct TempTestDir {
    path: PathBuf,
}

impl TempTestDir {
    fn create(prefix: &str) -> Self {
        for attempt in 0..64 {
            let path =
                std::env::temp_dir().join(format!("{prefix}-{}-{attempt}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create test dir {}: {e}", path.display()),
            }
        }
        panic!("could not create an exclusive test directory");
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn relative_path(from: &Path, to: &Path) -> Option<PathBuf> {
    let from_components: Vec<_> = from.components().collect();
    let to_components: Vec<_> = to.components().collect();
    let common = from_components
        .iter()
        .zip(&to_components)
        .take_while(|(left, right)| left == right)
        .count();
    if common == 0 {
        return None;
    }

    let mut relative = PathBuf::new();
    for _ in &from_components[common..] {
        relative.push("..");
    }
    for component in &to_components[common..] {
        match component {
            Component::Prefix(_) | Component::RootDir => return None,
            _ => relative.push(component.as_os_str()),
        }
    }
    Some(relative)
}

#[cfg(target_os = "linux")]
fn assert_process_is_gone(pid: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => return,
            // A zombie no longer runs; the environment may not reap orphans.
            Ok(stat) if stat.split_whitespace().nth(2) == Some("Z") => return,
            Ok(_) => {}
        }
        if std::time::Instant::now() >= deadline {
            let _ = Command::new("kill")
                .args(["-KILL", pid])
                .stderr(Stdio::null())
                .status();
            panic!("descendant {pid} outlived the supervisor cleanup");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn python_basic_execution() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let req = ExecutionRequest::new(CodeLanguage::Python, "print('Hello from Python!')");
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert_eq!(result.exit_code, Some(0));
    assert!(result.stdout.contains("Hello from Python!"));
    assert!(!result.timed_out);
    assert!(!result.output_truncated);
    assert!(result.plot_png.is_none());
    assert!(result.plot_svg.is_none());
}

#[test]
fn python_working_dir_execution() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let temp_dir = std::env::temp_dir();
    let req = ExecutionRequest::new(
        CodeLanguage::Python,
        "import os\nprint(os.path.realpath(os.getcwd()))",
    )
    .with_working_dir(&temp_dir);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    let expected_dir = fs::canonicalize(&temp_dir).expect("canonical temp dir");
    let printed_dir =
        fs::canonicalize(PathBuf::from(result.stdout.trim())).expect("canonical printed dir");
    assert_eq!(printed_dir, expected_dir);
}

#[test]
fn python_syntax_error_handled_safely() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let req = ExecutionRequest::new(CodeLanguage::Python, "def broken(: syntax error");
    let result = execute(&req).expect("execution failed");

    assert!(!result.success);
    assert_ne!(result.exit_code, Some(0));
    assert!(result.stderr.contains("SyntaxError"));
    assert!(!result.timed_out);
}

#[test]
fn python_timeout_kills_runaway_process() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    // Runaway infinite loop with 300ms timeout
    let req = ExecutionRequest::new(
        CodeLanguage::Python,
        "import time\nwhile True:\n    time.sleep(0.05)",
    )
    .with_timeout(300);
    let result = execute(&req).expect("execution failed");

    assert!(!result.success);
    assert!(result.timed_out);
}

#[test]
fn python_timeout_preserves_partial_stdout() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let req = ExecutionRequest::new(
        CodeLanguage::Python,
        "import time\nprint('before timeout', flush=True)\ntime.sleep(5)",
    )
    .with_plot_capture(false)
    .with_timeout(300);
    let result = execute(&req).expect("execution failed");

    assert!(result.timed_out);
    assert!(result.stdout.contains("before timeout"));
}

#[test]
fn python_output_budget_limits_capture() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let code = "import sys\nsys.stdout.write('x' * (12 * 1024 * 1024))\nsys.stdout.flush()";
    let req = ExecutionRequest::new(CodeLanguage::Python, code).with_plot_capture(false);
    let result = execute(&req).expect("execution failed");

    assert!(result.output_truncated);
    assert!(result.stdout.len() <= MAX_CAPTURE_BYTES);
}

#[test]
fn python_output_survives_inherited_pipe_holder() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    // The grandchild inherits the capture handle and outlives the interpreter.
    let code = r#"
import subprocess, sys
print("ready", flush=True)
subprocess.Popen([sys.executable, "-c", "import time; time.sleep(5)"])
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code).with_plot_capture(false);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert!(result.stdout.contains("ready"));
}

#[cfg(target_os = "linux")]
#[test]
fn python_supervisor_kills_descendant_that_escapes_process_group() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let code = r#"
import subprocess, sys
child = subprocess.Popen(
    [sys.executable, "-c", "import time; time.sleep(30)"],
    start_new_session=True,
)
print(child.pid, flush=True)
"#;
    let started = std::time::Instant::now();
    let req = ExecutionRequest::new(CodeLanguage::Python, code).with_plot_capture(false);
    let result = execute(&req).expect("execution failed");

    let child_pid = result
        .stdout
        .trim()
        .parse::<u32>()
        .expect("print child pid");
    let child_pid = child_pid.to_string();
    let alive = Command::new("kill")
        .args(["-0", child_pid.as_str()])
        .stderr(Stdio::null())
        .status()
        .expect("check escaped test child");
    if alive.success() {
        let _ = Command::new("kill")
            .args(["-KILL", child_pid.as_str()])
            .stderr(Stdio::null())
            .status();
    }
    assert!(result.success);
    assert!(!alive.success(), "escaped descendant remained alive");
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
}

#[cfg(target_os = "linux")]
#[test]
fn forced_supervisor_termination_kills_its_process_group() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    use std::os::unix::fs::PermissionsExt;

    let temp_dir = TempTestDir::create("wc-supervisor-stall");
    let helper = temp_dir.path().join("stalled-supervisor");
    fs::write(
        &helper,
        "#!/bin/sh\ntrap '' TERM\nsleep 60 &\nchild=$!\necho \"$child\" > child.pid\n: > child.ready\nsleep 60\nwait\n",
    )
    .expect("write stalled helper");
    let mut permissions = fs::metadata(&helper)
        .expect("read helper metadata")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&helper, permissions).expect("make helper executable");

    let request = ExecutionRequest::new(CodeLanguage::Python, "")
        .with_supervisor_path(&helper)
        .with_working_dir(temp_dir.path())
        .with_timeout(50)
        .with_plot_capture(false);
    assert!(execute(&request).is_err());

    assert!(
        temp_dir.path().join("child.ready").is_file(),
        "fake supervisor did not start its descendant"
    );
    let child_pid = fs::read_to_string(temp_dir.path().join("child.pid"))
        .expect("read helper child pid")
        .trim()
        .to_string();
    assert_process_is_gone(&child_pid);
}

#[cfg(target_os = "linux")]
#[test]
fn cancellation_lets_the_supervisor_clean_up_before_the_hard_fallback() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    use std::os::unix::fs::PermissionsExt;

    let temp_dir = TempTestDir::create("wc-supervisor-cancel");
    let helper = temp_dir.path().join("cancelling-supervisor");
    fs::write(
        &helper,
        "#!/bin/sh\ntrap 'echo term > term.received; kill -KILL \"$child\"; wait \"$child\" 2>/dev/null; exit 0' TERM\nsleep 60 &\nchild=$!\necho \"$child\" > child.pid\nwhile :; do sleep 0.05; done\n",
    )
    .expect("write cancelling helper");
    let mut permissions = fs::metadata(&helper)
        .expect("read helper metadata")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&helper, permissions).expect("make helper executable");

    let request = ExecutionRequest::new(CodeLanguage::Python, "")
        .with_supervisor_path(&helper)
        .with_working_dir(temp_dir.path())
        .with_timeout(50)
        .with_plot_capture(false);
    let started = std::time::Instant::now();
    assert!(execute(&request).is_err());

    // The supervisor deadline is the request timeout plus five seconds, and a
    // cooperative cancellation must finish before the four-second grace that
    // leads to the hard fallback.
    assert!(started.elapsed() < std::time::Duration::from_secs(7));
    assert!(
        temp_dir.path().join("term.received").is_file(),
        "the supervisor was not asked to cancel its interpreter"
    );
    let child_pid = fs::read_to_string(temp_dir.path().join("child.pid"))
        .expect("read helper child pid")
        .trim()
        .to_string();
    assert_process_is_gone(&child_pid);
}

#[cfg(unix)]
#[test]
fn supervisor_diagnostics_do_not_wait_for_inherited_stderr() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let temp_dir = TempTestDir::create("wc-supervisor-stderr");
    let helper = temp_dir.path().join("helper.sh");
    fs::write(&helper, "#!/bin/sh\nsleep 3 &\nexit 0\n").expect("write helper");

    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(&helper)
        .expect("read helper metadata")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&helper, permissions).expect("make helper executable");

    let request = ExecutionRequest::new(CodeLanguage::Python, "")
        .with_supervisor_path(&helper)
        .with_plot_capture(false);
    let started = std::time::Instant::now();
    match execute(&request) {
        Err(KernelError::Io(message)) => assert!(
            message.contains("read supervisor result"),
            "expected a missing supervisor result, got: {message}"
        ),
        other => panic!("expected a missing supervisor result, got: {other:?}"),
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "reading supervisor diagnostics waited for the inherited stderr handle"
    );
}

#[cfg(windows)]
#[test]
fn python_supervisor_kills_descendant_in_new_process_group() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let code = r#"
import subprocess, sys
child = subprocess.Popen(
    [sys.executable, "-c", "import time; time.sleep(30)"],
    creationflags=subprocess.CREATE_NEW_PROCESS_GROUP,
)
print(child.pid, flush=True)
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code).with_plot_capture(false);
    let result = execute(&req).expect("execution failed");
    let child_pid = result.stdout.trim().to_string();
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {child_pid}"), "/NH"])
        .output()
        .expect("check escaped test child");
    assert!(output.status.success(), "tasklist failed");
    let listed = String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next();
        fields.next() == Some(child_pid.as_str())
    });
    if listed {
        let _ = Command::new("taskkill")
            .args(["/PID", &child_pid, "/T", "/F"])
            .status();
    }
    assert!(result.success);
    assert!(!listed, "descendant remained alive");
}

#[cfg(all(unix, not(target_os = "linux")))]
#[test]
fn python_supervisor_kills_same_group_child_after_interpreter_exits() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let code = r#"
import subprocess, sys
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
print(child.pid, flush=True)
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code).with_plot_capture(false);
    let result = execute(&req).expect("execution failed");
    let child_pid = result.stdout.trim().to_string();

    assert!(result.success);
    assert_descendant_stopped(&child_pid);
}

/// Pass when the descendant is gone or a zombie; the environment may not reap
/// orphans, and a zombie no longer runs.
#[cfg(all(unix, not(target_os = "linux")))]
fn assert_descendant_stopped(pid: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let state = Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .expect("probe descendant state");
        let state = String::from_utf8_lossy(&state.stdout);
        let state = state.trim();
        if state.is_empty() || state.starts_with('Z') {
            return;
        }
        if std::time::Instant::now() >= deadline {
            let _ = Command::new("kill")
                .args(["-KILL", pid])
                .stderr(Stdio::null())
                .status();
            panic!("same-group descendant {pid} outlived the interpreter");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn python_path_is_used_for_module_lookup() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let helper_dir = TempTestDir::create("wc-python-path");
    fs::write(helper_dir.path().join("wc_helper.py"), "VALUE = 42\n").expect("write helper module");

    let req = ExecutionRequest::new(
        CodeLanguage::Python,
        "import wc_helper\nprint(wc_helper.VALUE)",
    )
    .with_python_path(helper_dir.path());
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert_eq!(result.stdout.trim(), "42");
}

#[test]
fn python_path_cannot_shadow_wrapper_imports() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let helper_dir = TempTestDir::create("wc-python-shadow");
    fs::write(helper_dir.path().join("helper_module.py"), "VALUE = 42\n")
        .expect("write helper module");
    fs::write(
        helper_dir.path().join("ast.py"),
        "raise RuntimeError('shadowed ast imported')\n",
    )
    .expect("write ast shadow");
    fs::write(
        helper_dir.path().join("json.py"),
        "raise RuntimeError('shadowed json imported')\n",
    )
    .expect("write json shadow");

    let req = ExecutionRequest::new(
        CodeLanguage::Python,
        "import helper_module\nprint(helper_module.VALUE)",
    )
    .with_python_path(helper_dir.path());
    let result = execute(&req).expect("execution failed");

    assert!(result.success, "{}", result.stderr);
    assert_eq!(result.stdout.trim(), "42");
}

#[test]
fn python_invalid_utf8_output_respects_the_byte_limit() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let code =
        "import sys\nsys.stdout.buffer.write(b'\\xff' * (9 * 1024 * 1024))\nsys.stdout.flush()";
    let result =
        execute(&ExecutionRequest::new(CodeLanguage::Python, code).with_plot_capture(false))
            .expect("execution failed");

    assert!(result.output_truncated);
    assert!(
        result.stdout.len() <= MAX_CAPTURE_BYTES,
        "decoded stdout exceeded the limit: {} bytes",
        result.stdout.len()
    );
}

#[test]
fn python_path_supports_future_imports() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let req = ExecutionRequest::new(
        CodeLanguage::Python,
        "from __future__ import annotations\nvalue: int = 42\nprint(value)",
    )
    .with_python_path(std::env::temp_dir());
    let result = execute(&req).expect("execution failed");

    assert!(result.success, "{}", result.stderr);
    assert_eq!(result.stdout.trim(), "42");
}

#[cfg(unix)]
#[test]
fn python_path_accepts_path_separator_in_directory_name() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let helper_dir = TempTestDir::create("wc-python:path");
    fs::write(helper_dir.path().join("wc_helper.py"), "VALUE = 42\n").expect("write helper module");
    let req = ExecutionRequest::new(
        CodeLanguage::Python,
        "import wc_helper\nprint(wc_helper.VALUE)",
    )
    .with_python_path(helper_dir.path());
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert_eq!(result.stdout.trim(), "42");
}

#[test]
fn relative_supervisor_path_is_resolved_before_changing_directory() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let current_dir = fs::canonicalize(std::env::current_dir().expect("current directory"))
        .expect("canonical current directory");
    let helper_path = fs::canonicalize(env!("CARGO_BIN_EXE_wc-kernel-supervisor"))
        .expect("canonical supervisor helper path");
    let relative_helper =
        relative_path(&current_dir, &helper_path).expect("helper must share the current volume");
    let working_dir = TempTestDir::create("wc-supervisor-cwd");
    let request = ExecutionRequest::new(CodeLanguage::Python, "print('ready')")
        .with_supervisor_path(relative_helper)
        .with_working_dir(working_dir.path())
        .with_plot_capture(false);
    let result = execute(&request).expect("execution failed");

    assert!(result.success);
    assert_eq!(result.stdout.trim(), "ready");
}

#[test]
fn r_basic_execution() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let req = ExecutionRequest::new(CodeLanguage::R, "cat('Hello from R!\n')");
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert_eq!(result.exit_code, Some(0));
    assert!(result.stdout.contains("Hello from R!"));
    assert!(!result.timed_out);
    assert!(result.plot_png.is_none());
    assert!(result.plot_svg.is_none());
}

#[test]
fn r_syntax_error_handled_safely() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let req = ExecutionRequest::new(CodeLanguage::R, "this is not valid R syntax %%%");
    let result = execute(&req).expect("execution failed");

    assert!(!result.success);
    assert!(!result.timed_out);
}

#[test]
fn r_timeout_kills_runaway_process() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let req =
        ExecutionRequest::new(CodeLanguage::R, "repeat { Sys.sleep(0.05) }").with_timeout(300);
    let result = execute(&req).expect("execution failed");

    assert!(!result.success);
    assert!(result.timed_out);
}

#[test]
fn r_plot_png_capture() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r#"
x <- 1:10
y <- x^2
plot(x, y, main = "Test Curve")
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code).with_plot_format(PlotFormat::Png);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert!(
        result.plot_png.is_some(),
        "Expected plot PNG bytes to be captured"
    );
    assert!(result.plot_svg.is_none());
    let png_bytes = result.plot_png.unwrap();
    // Valid PNG header: 0x89 'P' 'N' 'G'
    assert!(png_bytes.starts_with(&[0x89, b'P', b'N', b'G']));
}

#[test]
fn r_plot_svg_capture() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r#"
x <- 1:10
y <- x^2
plot(x, y, main = "Test SVG Curve")
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code).with_plot_format(PlotFormat::Svg);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert!(result.plot_png.is_none());
    assert!(
        result.plot_svg.is_some(),
        "Expected plot SVG string to be captured"
    );
    let svg = result.plot_svg.unwrap();
    assert!(svg.contains("<svg"), "SVG output must contain '<svg'");
}

#[test]
fn r_plot_prefer_svg_deserialized_request() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let json = r#"{"language":"r","code":"plot(1:5)","timeout_ms":30000,"capture_plots":true,"prefer_svg":true}"#;
    let req: ExecutionRequest = serde_json::from_str(json).expect("deserialize request");
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert!(result.plot_png.is_none());
    assert!(
        result.plot_svg.is_some(),
        "Expected SVG capture from prefer_svg"
    );
    assert!(result.plot_svg.unwrap().contains("<svg"));
}

#[test]
fn r_plot_both_capture() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r#"
x <- 1:10
y <- x^2
plot(x, y, main = "Test Both Curve")
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code).with_plot_format(PlotFormat::Both);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert!(result.plot_png.is_some(), "Expected PNG bytes");
    assert!(result.plot_svg.is_some(), "Expected SVG string");
    assert!(result.plot_svg.unwrap().contains("<svg"));
}

#[test]
fn r_plot_both_capture_device_closed_by_user() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r#"
plot(1:5)
dev.off()
pdf(tempfile(fileext = ".pdf"))
plot(5:1)
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code).with_plot_format(PlotFormat::Both);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert!(result.plot_png.is_some(), "Expected completed PNG bytes");
    assert!(
        result.plot_svg.is_none(),
        "Must not copy the other active device as the SVG"
    );
}

#[test]
fn r_plot_both_capture_device_replaced_with_same_type_by_user() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r#"
plot(1:5)
dev.off()
png(tempfile(fileext = ".png"))
plot(5:1)
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code).with_plot_format(PlotFormat::Both);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert!(result.plot_png.is_some(), "Expected completed PNG bytes");
    assert!(
        result.plot_svg.is_none(),
        "Must not copy the replacement same-type device as the SVG"
    );
}

fn python_has_matplotlib() -> bool {
    let Some(python) = which_binary(CodeLanguage::Python) else {
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
fn python_plot_png_capture() {
    if !python_has_matplotlib() {
        return;
    }

    let code = r#"
import matplotlib.pyplot as plt
plt.figure()
plt.plot([1, 2, 3], [10, 20, 30])
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code).with_plot_format(PlotFormat::Png);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert!(result.plot_png.is_some(), "Expected PNG plot output");
    assert!(
        result
            .plot_png
            .unwrap()
            .starts_with(&[0x89, b'P', b'N', b'G'])
    );
    assert!(result.plot_svg.is_none());
}

#[test]
fn python_plot_svg_capture() {
    if !python_has_matplotlib() {
        return;
    }

    let code = r#"
import matplotlib.pyplot as plt
plt.figure()
plt.plot([1, 2, 3], [10, 20, 30])
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code).with_plot_format(PlotFormat::Svg);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert!(result.plot_png.is_none());
    assert!(result.plot_svg.is_some(), "Expected SVG plot output");
    let svg = result.plot_svg.unwrap();
    assert!(svg.contains("<svg") && svg.contains("</svg>"));
}

#[test]
fn python_plot_prefer_svg_deserialized_request() {
    if !python_has_matplotlib() {
        return;
    }

    let code = r#"
import matplotlib.pyplot as plt
plt.figure()
plt.plot([1, 2, 3], [10, 20, 30])
"#;
    let req: ExecutionRequest = serde_json::from_value(serde_json::json!({
        "language": "python",
        "code": code,
        "timeout_ms": 30_000,
        "capture_plots": true,
        "prefer_svg": true,
    }))
    .expect("deserialize request");
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert!(result.plot_png.is_none());
    assert!(
        result.plot_svg.is_some(),
        "Expected SVG capture from prefer_svg"
    );
    assert!(result.plot_svg.unwrap().contains("<svg"));
}

#[test]
fn python_plot_both_capture() {
    if !python_has_matplotlib() {
        return;
    }

    let code = r#"
import matplotlib.pyplot as plt
plt.figure()
plt.plot([1, 2, 3], [10, 20, 30])
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code).with_plot_format(PlotFormat::Both);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert!(result.plot_png.is_some(), "Expected PNG in Both mode");
    assert!(result.plot_svg.is_some(), "Expected SVG in Both mode");
    assert!(result.plot_svg.unwrap().contains("<svg"));
}
