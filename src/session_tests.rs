#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::time::Duration;

use super::*;

fn manager_for(language: CodeLanguage) -> Option<SessionManager> {
    if !is_language_available(language) {
        return None;
    }
    Some(SessionManager::new().with_idle_timeout(Duration::from_secs(60)))
}

fn chunk(language: CodeLanguage, code: &str) -> ExecutionRequest {
    ExecutionRequest::new(language, code).with_timeout(10_000)
}

fn python_has_matplotlib() -> bool {
    if !is_language_available(CodeLanguage::Python) {
        return false;
    }
    let req = ExecutionRequest::new(
        CodeLanguage::Python,
        "import matplotlib; matplotlib.use('Agg'); import matplotlib.pyplot as plt",
    )
    .with_timeout(5_000);
    execute(&req).map(|res| res.success).unwrap_or(false)
}

fn r_has_png_device() -> bool {
    if !is_language_available(CodeLanguage::R) {
        return false;
    }
    let req = ExecutionRequest::new(
        CodeLanguage::R,
        "tf <- tempfile(fileext = '.png'); png(tf); dev.off(); unlink(tf)",
    )
    .with_timeout(5_000);
    execute(&req).map(|res| res.success).unwrap_or(false)
}

fn r_has_svg_device() -> bool {
    if !is_language_available(CodeLanguage::R) {
        return false;
    }
    let req = ExecutionRequest::new(
        CodeLanguage::R,
        "tf <- tempfile(fileext = '.svg'); grDevices::svg(tf); grDevices::dev.off(); unlink(tf)",
    )
    .with_timeout(5_000);
    execute(&req).map(|res| res.success).unwrap_or(false)
}

#[test]
fn python_session_keeps_variables_between_executions() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let first = chunk(CodeLanguage::Python, "total = 40 + 2");
    let result1 = manager
        .execute("py-state", &first)
        .expect("execute chunk 1");
    assert!(result1.success, "stderr: {}", result1.stderr);

    let second = chunk(CodeLanguage::Python, "print(f'total is {total}')");
    let result2 = manager
        .execute("py-state", &second)
        .expect("execute chunk 2");
    assert!(result2.success, "stderr: {}", result2.stderr);
    assert_eq!(result2.stdout.trim(), "total is 42");

    manager.close_session("py-state");
}

#[test]
fn r_session_keeps_variables_between_executions() {
    let Some(mut manager) = manager_for(CodeLanguage::R) else {
        return;
    };

    let first = chunk(CodeLanguage::R, "total <- 40 + 2");
    let result1 = manager.execute("r-state", &first).expect("execute chunk 1");
    assert!(result1.success, "stderr: {}", result1.stderr);

    let second = chunk(
        CodeLanguage::R,
        "cat(\"total is \", total, \"\\n\", sep = \"\")",
    );
    let result2 = manager
        .execute("r-state", &second)
        .expect("execute chunk 2");
    assert!(result2.success, "stderr: {}", result2.stderr);
    assert_eq!(result2.stdout.trim(), "total is 42");

    manager.close_session("r-state");
}

#[test]
fn sessions_do_not_share_state() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let first = chunk(CodeLanguage::Python, "x = 99");
    let r1 = manager.execute("session-a", &first).expect("execute chunk");
    assert!(r1.success);

    let probe = chunk(
        CodeLanguage::Python,
        "print('defined' if 'x' in globals() else 'missing')",
    );
    let r2 = manager.execute("session-b", &probe).expect("execute chunk");
    assert!(r2.success);
    assert_eq!(r2.stdout.trim(), "missing");

    manager.close_session("session-a");
    manager.close_session("session-b");
}

#[test]
fn restart_session_clears_memory() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let define = chunk(CodeLanguage::Python, "survives = 'no'");
    assert!(
        manager
            .execute("py-restart", &define)
            .expect("execute")
            .success
    );

    manager
        .restart_session("py-restart")
        .expect("restart session");

    let probe = chunk(
        CodeLanguage::Python,
        "print('survives' if 'survives' in globals() else 'cleared')",
    );
    let result = manager.execute("py-restart", &probe).expect("execute");
    assert!(result.success);
    assert_eq!(result.stdout.trim(), "cleared");

    manager.close_session("py-restart");
}

#[test]
fn restarting_an_unknown_session_is_a_no_op() {
    let mut manager = SessionManager::new();
    assert!(manager.restart_session("does-not-exist").is_ok());
    assert_eq!(manager.session_count(), 0);
}

#[test]
fn a_session_keeps_its_language() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let first = chunk(CodeLanguage::Python, "x = 1");
    assert!(
        manager
            .execute("lang-guard", &first)
            .expect("execute")
            .success
    );

    let r_chunk = chunk(CodeLanguage::R, "x <- 1");
    let err = manager
        .execute("lang-guard", &r_chunk)
        .expect_err("should reject language mismatch");

    match err {
        KernelError::SessionLanguageMismatch {
            session_id,
            existing,
            requested,
        } => {
            assert_eq!(session_id, "lang-guard");
            assert_eq!(existing, CodeLanguage::Python);
            assert_eq!(requested, CodeLanguage::R);
        }
        other => panic!("expected SessionLanguageMismatch, got {other:?}"),
    }

    manager.close_session("lang-guard");
}

#[test]
fn idle_sessions_are_reaped() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };
    manager.set_idle_timeout(Duration::from_millis(50));

    let first = chunk(CodeLanguage::Python, "x = 1");
    assert!(manager.execute("idle-py", &first).expect("execute").success);
    assert_eq!(manager.session_count(), 1);

    std::thread::sleep(Duration::from_millis(100));

    let reaped = manager.reap_idle_sessions();
    assert_eq!(reaped, 1);
    assert_eq!(manager.session_count(), 0);
    assert!(!manager.contains("idle-py"));
}

#[test]
fn idle_session_reaped_on_subsequent_execute_request() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };
    manager.set_idle_timeout(Duration::from_millis(50));

    let first = chunk(CodeLanguage::Python, "cached_var = 123");
    assert!(
        manager
            .execute("idle-expiry-target", &first)
            .expect("execute")
            .success
    );

    std::thread::sleep(Duration::from_millis(100));

    // Calling execute on the exact same session_id after timeout should reap the expired daemon
    // and start a fresh one, meaning cached_var is no longer defined.
    let second = chunk(
        CodeLanguage::Python,
        "print('found' if 'cached_var' in globals() else 'expired')",
    );
    let result = manager
        .execute("idle-expiry-target", &second)
        .expect("execute");
    assert!(result.success);
    assert_eq!(result.stdout.trim(), "expired");

    manager.close_session("idle-expiry-target");
}

#[test]
fn a_zero_idle_timeout_disables_reaping() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };
    manager.set_idle_timeout(Duration::ZERO);

    let first = chunk(CodeLanguage::Python, "x = 1");
    assert!(manager.execute("forever", &first).expect("execute").success);

    std::thread::sleep(Duration::from_millis(50));

    assert_eq!(manager.reap_idle_sessions(), 0);
    assert_eq!(manager.session_count(), 1);

    manager.close_session("forever");
}

#[test]
fn session_timeout_kills_the_daemon() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let warm = ExecutionRequest::new(CodeLanguage::Python, "print('warm')");
    let res_warm = manager.execute("py-timeout", &warm).expect("warm");
    assert!(res_warm.success);
    assert_eq!(res_warm.stdout.trim(), "warm");

    let hang = ExecutionRequest::new(
        CodeLanguage::Python,
        "import sys, time\nsys.stdout.write('started\\n')\nsys.stdout.flush()\ntime.sleep(10)",
    )
    .with_timeout(300);
    let result = manager
        .execute("py-timeout", &hang)
        .expect("execution result");
    assert!(result.timed_out);
    assert!(!result.success);
    assert!(
        result.stdout.contains("started"),
        "stdout should capture output before timeout: {:?}",
        result.stdout
    );
    assert!(!manager.contains("py-timeout"));

    let next = chunk(CodeLanguage::Python, "print('recovered')");
    let r2 = manager.execute("py-timeout", &next).expect("execute next");
    assert!(r2.success);
    assert_eq!(r2.stdout.trim(), "recovered");

    manager.close_session("py-timeout");
}

#[test]
fn a_failed_chunk_leaves_the_session_running() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    // Step 1: Assign a marker before raising an error
    let define = chunk(CodeLanguage::Python, "marker = 9876");
    let r1 = manager
        .execute("py-error", &define)
        .expect("execute chunk 1");
    assert!(r1.success);

    // Step 2: Failed chunk raises an error
    let bad = chunk(CodeLanguage::Python, "raise RuntimeError('boom')");
    let r2 = manager.execute("py-error", &bad).expect("execute chunk 2");
    assert!(!r2.success);
    assert!(
        r2.stderr.contains("RuntimeError"),
        "stderr should mention the exception: {}",
        r2.stderr
    );

    // Step 3: Verify the session was NOT killed and pre-existing variables persist
    let next = chunk(CodeLanguage::Python, "print(f'marker is {marker}')");
    let r3 = manager.execute("py-error", &next).expect("execute chunk 3");
    assert!(r3.success, "stderr: {}", r3.stderr);
    assert_eq!(r3.stdout.trim(), "marker is 9876");

    manager.close_session("py-error");
}

#[test]
fn r_session_reports_errors_without_dying() {
    let Some(mut manager) = manager_for(CodeLanguage::R) else {
        return;
    };

    // Step 1: Assign a marker before raising an error
    let define = chunk(CodeLanguage::R, "marker <- 54321");
    let r1 = manager
        .execute("r-error", &define)
        .expect("execute chunk 1");
    assert!(r1.success);

    // Step 2: Failed chunk stops with an error
    let bad = chunk(CodeLanguage::R, "stop(\"deliberate failure\")");
    let r2 = manager.execute("r-error", &bad).expect("execute chunk 2");
    assert!(!r2.success);
    assert!(
        r2.stderr.contains("deliberate failure"),
        "stderr should mention the error: {}",
        r2.stderr
    );

    // Step 3: Verify variables assigned before the error are still accessible
    let probe = chunk(
        CodeLanguage::R,
        "cat(\"marker is \", marker, \"\\n\", sep = \"\")",
    );
    let r3 = manager.execute("r-error", &probe).expect("execute chunk 3");
    assert!(r3.success, "stderr: {}", r3.stderr);
    assert_eq!(r3.stdout.trim(), "marker is 54321");

    manager.close_session("r-error");
}

#[test]
fn session_separates_stdout_and_stderr() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let code = r#"
import sys
print("standard output line")
sys.stderr.write("standard error line\n")
"#;
    let result = manager
        .execute("streams", &chunk(CodeLanguage::Python, code))
        .expect("execute");
    assert!(result.success);
    assert_eq!(result.stdout.trim(), "standard output line");
    assert_eq!(result.stderr.trim(), "standard error line");

    manager.close_session("streams");
}

#[test]
fn active_sessions_report_language_and_process() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let c = chunk(CodeLanguage::Python, "x = 1");
    assert!(manager.execute("active-a", &c).expect("execute").success);
    assert!(manager.execute("active-b", &c).expect("execute").success);

    let active = manager.active_sessions();
    assert_eq!(active.len(), 2);
    assert_eq!(active[0].session_id, "active-a");
    assert_eq!(active[0].language, CodeLanguage::Python);
    assert!(active[0].pid.is_some());
    assert_eq!(active[1].session_id, "active-b");

    manager.close_session("active-a");
    manager.close_session("active-b");
}

#[test]
fn exited_interpreter_is_not_reported_as_active() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let c = chunk(CodeLanguage::Python, "x = 1");
    assert!(manager.execute("exited-test", &c).expect("execute").success);
    assert_eq!(manager.session_count(), 1);

    // Kill the underlying process directly
    manager.kill_session_process_for_test("exited-test");

    assert_eq!(manager.active_sessions().len(), 0);
    assert_eq!(manager.session_count(), 0);
    assert!(!manager.contains("exited-test"));
}

#[test]
fn close_session_frees_the_process() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let c = chunk(CodeLanguage::Python, "x = 1");
    assert!(
        manager
            .execute("close-target", &c)
            .expect("execute")
            .success
    );
    assert!(manager.contains("close-target"));

    assert!(manager.close_session("close-target"));
    assert!(!manager.contains("close-target"));
    assert_eq!(manager.session_count(), 0);

    assert!(!manager.close_session("close-target"));
}

#[test]
fn a_chunk_changes_the_working_directory_for_later_chunks() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    struct TempDirGuard(PathBuf);
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    let pid = std::process::id();
    let rand_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let sub = std::env::temp_dir().join(format!("wc-test-cwd-{pid}-{rand_id}"));
    fs::create_dir_all(&sub).expect("create dir");
    let _guard = TempDirGuard(sub.clone());

    let move_req =
        chunk(CodeLanguage::Python, "import os; print(os.getcwd())").with_working_dir(&sub);
    let r1 = manager
        .execute("py-cwd", &move_req)
        .expect("execute move chunk");
    assert!(r1.success, "stderr: {}", r1.stderr);

    let stay_req = chunk(CodeLanguage::Python, "import os; print(os.getcwd())");
    let r2 = manager
        .execute("py-cwd", &stay_req)
        .expect("execute stay chunk");
    assert!(r2.success, "stderr: {}", r2.stderr);

    let actual_path = PathBuf::from(r2.stdout.trim());
    let expected_canon = sub.canonicalize().unwrap_or_else(|_| sub.clone());
    let actual_canon = actual_path.canonicalize().unwrap_or(actual_path);

    let normalize = |p: &Path| {
        let s = p.to_string_lossy();
        s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
    };
    assert_eq!(normalize(&actual_canon), normalize(&expected_canon));

    manager.close_session("py-cwd");
}

#[test]
fn session_preserved_on_invalid_unsent_request() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let init = chunk(CodeLanguage::Python, "my_val = 'important_data'");
    assert!(
        manager
            .execute("preserve-test", &init)
            .expect("execute")
            .success
    );

    // Request with invalid working directory containing line break
    let bad_req = chunk(CodeLanguage::Python, "print('should not run')")
        .with_working_dir(PathBuf::from("dir\nwith\nnewline"));

    let err = manager.execute("preserve-test", &bad_req);
    assert!(err.is_err());

    // The healthy session must still be present with its variables intact
    let probe = chunk(CodeLanguage::Python, "print(my_val)");
    let res = manager
        .execute("preserve-test", &probe)
        .expect("execute probe");
    assert!(res.success);
    assert_eq!(res.stdout.trim(), "important_data");

    manager.close_session("preserve-test");
}

#[test]
fn session_temp_dir_has_restricted_permissions() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let c = chunk(CodeLanguage::Python, "x = 1");
    assert!(manager.execute("perm-test", &c).expect("execute").success);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let temp_dir = manager
            .temp_dir_for_test("perm-test")
            .expect("session temp dir");
        let metadata = fs::metadata(temp_dir).expect("metadata");
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "session directory must have 0700 permissions, got 0{:o}",
            mode
        );
    }

    manager.close_session("perm-test");
}

#[test]
fn code_with_sentinel_lines_does_not_break_protocol() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let tricky_code = r#"
msg = """
RUN 1 1 png 0 0
__WC_END_1__
END 1
"""
print("received:", len(msg) > 0)
"#;
    let req = chunk(CodeLanguage::Python, tricky_code);
    let res = manager
        .execute("sentinel-test", &req)
        .expect("execute tricky code");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.stdout.trim(), "received: True");

    manager.close_session("sentinel-test");
}

#[test]
fn python_subprocess_output_does_not_corrupt_protocol() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    // Subprocess writes stdout without a trailing newline
    let code = r#"
import subprocess, sys
subprocess.run([sys.executable, "-c", "import sys; sys.stdout.write('sub_output_no_newline')"])
"#;
    let req = chunk(CodeLanguage::Python, code);
    let res = manager
        .execute("subproc-test", &req)
        .expect("execute subprocess");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.stdout.trim(), "sub_output_no_newline");

    manager.close_session("subproc-test");
}

#[test]
fn python_session_captures_plots_between_chunks() {
    if !python_has_matplotlib() {
        return;
    }
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let draw = ExecutionRequest::new(
        CodeLanguage::Python,
        "import matplotlib.pyplot as plt\nplt.figure()\nplt.plot([1, 2, 3], [4, 5, 6])",
    )
    .with_plot_capture(true)
    .with_plot_format(PlotFormat::Png)
    .with_timeout(10_000);

    let result = manager.execute("py-plot", &draw).expect("execute draw");
    assert!(result.success, "stderr: {}", result.stderr);

    assert!(result.plot_png.is_some(), "plot should be captured");
    let bytes = result.plot_png.expect("plot png");
    assert!(bytes.len() > 64);
    assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]));

    manager.close_session("py-plot");
}

#[test]
fn python_text_chunk_does_not_reemit_old_figure() {
    if !python_has_matplotlib() {
        return;
    }
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    // Chunk 1: Create a plot
    let draw = ExecutionRequest::new(
        CodeLanguage::Python,
        "import matplotlib.pyplot as plt\nplt.figure()\nplt.plot([1, 2], [3, 4])",
    )
    .with_plot_capture(true)
    .with_plot_format(PlotFormat::Png)
    .with_timeout(10_000);

    let r1 = manager
        .execute("py-no-reemit", &draw)
        .expect("execute draw");
    assert!(r1.success);
    assert!(r1.plot_png.is_some(), "chunk 1 should emit plot");

    // Chunk 2: Pure text chunk, should NOT re-emit the figure from chunk 1
    let text = ExecutionRequest::new(CodeLanguage::Python, "print('pure text here')")
        .with_plot_capture(true)
        .with_plot_format(PlotFormat::Png)
        .with_timeout(10_000);

    let r2 = manager
        .execute("py-no-reemit", &text)
        .expect("execute text");
    assert!(r2.success);
    assert_eq!(r2.stdout.trim(), "pure text here");
    assert!(
        r2.plot_png.is_none(),
        "text chunk must not re-emit figure from earlier chunk"
    );

    manager.close_session("py-no-reemit");
}

#[test]
fn python_session_captures_png_and_svg() {
    if !python_has_matplotlib() {
        return;
    }
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let draw = ExecutionRequest::new(
        CodeLanguage::Python,
        "import matplotlib.pyplot as plt\nplt.figure()\nplt.plot([0, 1], [0, 1])",
    )
    .with_plot_capture(true)
    .with_plot_format(PlotFormat::Both)
    .with_timeout(10_000);

    let result = manager.execute("py-dual", &draw).expect("execute draw");
    assert!(result.success, "stderr: {}", result.stderr);

    assert!(result.plot_png.is_some());
    assert!(result.plot_svg.is_some());
    let svg = result.plot_svg.expect("plot svg");
    assert!(svg.contains("<svg"));
    assert!(svg.trim_end().ends_with("</svg>"));

    manager.close_session("py-dual");
}

#[test]
fn r_session_captures_a_plot() {
    if !r_has_png_device() {
        return;
    }
    let Some(mut manager) = manager_for(CodeLanguage::R) else {
        return;
    };

    let draw = ExecutionRequest::new(CodeLanguage::R, "plot(1:5, 1:5)")
        .with_plot_capture(true)
        .with_plot_format(PlotFormat::Png)
        .with_timeout(10_000);

    let result = manager.execute("r-plot", &draw).expect("execute draw");
    assert!(result.success, "stderr: {}", result.stderr);

    assert!(result.plot_png.is_some(), "plot should be captured");
    let bytes = result.plot_png.expect("plot png");
    assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]));

    manager.close_session("r-plot");
}

#[test]
fn r_dual_plot_captures_both_png_and_svg_and_closes_devices() {
    if !r_has_png_device() || !r_has_svg_device() {
        return;
    }
    let Some(mut manager) = manager_for(CodeLanguage::R) else {
        return;
    };

    let draw = ExecutionRequest::new(CodeLanguage::R, "plot(1:10, (1:10)^2)")
        .with_plot_capture(true)
        .with_plot_format(PlotFormat::Both)
        .with_timeout(10_000);

    let result = manager.execute("r-dual", &draw).expect("execute dual draw");
    assert!(result.success, "stderr: {}", result.stderr);
    assert!(result.plot_png.is_some());
    assert!(result.plot_svg.is_some());
    let svg = result.plot_svg.expect("plot svg");
    assert!(svg.contains("<svg"));
    assert!(svg.trim_end().ends_with("</svg>"));

    // Verify devices were cleanly closed by running a probe chunk with capture disabled
    let probe = chunk(
        CodeLanguage::R,
        "cat('open devs:', length(dev.list()), '\\n')",
    )
    .with_plot_capture(false);
    let r_probe = manager.execute("r-dual", &probe).expect("execute probe");
    assert!(r_probe.success);
    assert_eq!(r_probe.stdout.trim(), "open devs: 0");

    manager.close_session("r-dual");
}

#[test]
fn r_global_env_variable_collision_does_not_affect_driver() {
    let Some(mut manager) = manager_for(CodeLanguage::R) else {
        return;
    };

    // User code creates variables named status, seq, out_path in .GlobalEnv
    let code = r#"
status <- "ready"
seq <- 999
out_path <- "/tmp/forbidden"
cat("user status is ", status, "\n", sep = "")
"#;
    let req = chunk(CodeLanguage::R, code);
    let res = manager
        .execute("r-collision", &req)
        .expect("execute collision");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.stdout.trim(), "user status is ready");

    manager.close_session("r-collision");
}

#[test]
fn python_classes_can_be_pickled_across_chunks() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let define_class = r#"
import pickle

class Person:
    def __init__(self, name):
        self.name = name

p = Person("Ada")
data = pickle.dumps(p)
"#;
    let res1 = manager
        .execute("py-pickle", &chunk(CodeLanguage::Python, define_class))
        .expect("define class");
    assert!(res1.success, "stderr: {}", res1.stderr);

    let unpickle_code = r#"
p2 = pickle.loads(data)
print(f"unpickled {p2.name}")
"#;
    let res2 = manager
        .execute("py-pickle", &chunk(CodeLanguage::Python, unpickle_code))
        .expect("unpickle");
    assert!(res2.success, "stderr: {}", res2.stderr);
    assert_eq!(res2.stdout.trim(), "unpickled Ada");

    manager.close_session("py-pickle");
}

#[test]
fn python_in_place_figure_modification_is_captured() {
    if !python_has_matplotlib() {
        return;
    }
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    // First chunk creates a plot
    let code1 = r#"
import matplotlib.pyplot as plt
fig, ax = plt.subplots()
line, = ax.plot([1, 2, 3], [1, 2, 3])
"#;
    let req1 = ExecutionRequest::new(CodeLanguage::Python, code1).with_plot_capture(true);
    let res1 = manager.execute("py-inplace", &req1).expect("chunk 1");
    assert!(res1.success, "stderr: {}", res1.stderr);
    assert!(res1.plot_png.is_some(), "initial figure must be captured");

    // Second chunk modifies the existing line artist in-place without calling plt.figure or plot
    let code2 = r#"
line.set_ydata([3, 2, 1])
fig.canvas.draw()
"#;
    let req2 = ExecutionRequest::new(CodeLanguage::Python, code2).with_plot_capture(true);
    let res2 = manager.execute("py-inplace", &req2).expect("chunk 2");
    assert!(res2.success, "stderr: {}", res2.stderr);
    assert!(
        res2.plot_png.is_some(),
        "modified figure should be re-emitted"
    );

    // Third chunk does NOT modify anything
    let code3 = "x = 42\nprint(x)";
    let req3 = ExecutionRequest::new(CodeLanguage::Python, code3).with_plot_capture(true);
    let res3 = manager.execute("py-inplace", &req3).expect("chunk 3");
    assert!(res3.success, "stderr: {}", res3.stderr);
    assert!(
        res3.plot_png.is_none(),
        "unmodified figure should not be re-emitted"
    );

    manager.close_session("py-inplace");
}

#[test]
fn python_plot_callbacks_write_to_chunk_output() {
    if !python_has_matplotlib() {
        return;
    }
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let code = r#"
import sys
import matplotlib.pyplot as plt
fig, ax = plt.subplots()
ax.plot([1, 2], [3, 4])
def on_draw(event):
    print("plot callback stdout")
    print("plot callback stderr", file=sys.stderr)
fig.canvas.mpl_connect("draw_event", on_draw)
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code)
        .with_plot_capture(true)
        .with_plot_format(PlotFormat::Png);
    let result = manager
        .execute("py-plot-callback", &req)
        .expect("execute plot");

    assert!(result.success, "stderr: {}", result.stderr);
    assert!(result.plot_png.is_some(), "plot should be captured");
    assert!(result.stdout.contains("plot callback stdout"));
    assert!(result.stderr.contains("plot callback stderr"));

    manager.close_session("py-plot-callback");
}

#[test]
fn r_driver_resilient_to_function_shadowing_and_env_wiping() {
    let Some(mut manager) = manager_for(CodeLanguage::R) else {
        return;
    };

    let code1 = r#"
cat <- function(...) stop("sabotaged cat")
writeLines <- function(...) stop("sabotaged writeLines")
sink <- function(...) stop("sabotaged sink")
.wc_driver <- "wrecked"
base::print("step 1 complete")
"#;
    let res1 = manager
        .execute("r-shadow", &chunk(CodeLanguage::R, code1))
        .expect("step 1");
    assert!(res1.success, "stderr: {}", res1.stderr);
    assert!(res1.stdout.contains("step 1 complete"));

    let code2 = r#"
rm(list = base::ls(all.names = TRUE))
base::print("still alive")
"#;
    let res2 = manager
        .execute("r-shadow", &chunk(CodeLanguage::R, code2))
        .expect("step 2");
    assert!(res2.success, "stderr: {}", res2.stderr);
    assert!(res2.stdout.contains("still alive"));

    manager.close_session("r-shadow");
}

#[cfg(unix)]
#[test]
fn r_subprocess_output_without_newline_does_not_corrupt_protocol() {
    let Some(mut manager) = manager_for(CodeLanguage::R) else {
        return;
    };

    let code = r#"
cat("before-subprocess\n")
system("printf 'partial-line-output'")
cat("\nafter-subprocess\n")
"#;
    let res = manager
        .execute("r-subp", &chunk(CodeLanguage::R, code))
        .expect("exec subp");
    assert!(res.success, "stderr: {}", res.stderr);
    assert!(
        res.stdout.contains("partial-line-output"),
        "stdout should contain partial-line-output, got: {:?}",
        res.stdout
    );
    let before = res.stdout.find("before-subprocess").expect("before output");
    let subprocess = res
        .stdout
        .find("partial-line-output")
        .expect("subprocess output");
    let after = res.stdout.find("after-subprocess").expect("after output");
    assert!(
        before < subprocess && subprocess < after,
        "stdout order was: {:?}",
        res.stdout
    );

    let binary = manager
        .execute(
            "r-subp",
            &chunk(CodeLanguage::R, r#"system("printf '\\377'")"#),
        )
        .expect("exec non-UTF-8 subp");
    assert!(binary.success, "stderr: {}", binary.stderr);
    assert!(binary.stdout.contains('\u{fffd}'));

    // Follow-up chunk confirms session is healthy and responsive
    let res2 = manager
        .execute("r-subp", &chunk(CodeLanguage::R, "cat('ok\n')"))
        .expect("exec follow-up");
    assert!(res2.success, "stderr: {}", res2.stderr);
    assert_eq!(res2.stdout.trim(), "ok");

    manager.close_session("r-subp");
}

#[test]
fn r_user_closing_plot_device_and_opening_pdf_is_handled_safely() {
    let Some(mut manager) = manager_for(CodeLanguage::R) else {
        return;
    };
    if !r_has_png_device() {
        return;
    }

    let code = r#"
plot(1:5)
dev.off()
pdf(tempfile(fileext = ".pdf"))
saved_dev <- as.integer(dev.cur())
plot(5:1)
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code).with_plot_capture(true);
    let res = manager.execute("r-devoff-pdf", &req).expect("exec");
    assert!(res.success, "stderr: {}", res.stderr);
    assert!(
        res.plot_png.is_some(),
        "first chunk must exercise driver's PNG capture device"
    );

    // Follow-up chunk verifies the replacement device was kept open and intact
    let check_code = r#"
is_open <- saved_dev %in% dev.list()
invisible(dev.off(saved_dev))
cat("device still open: ", is_open, "\n", sep = "")
"#;
    let req2 = ExecutionRequest::new(CodeLanguage::R, check_code).with_plot_capture(false);
    let res2 = manager
        .execute("r-devoff-pdf", &req2)
        .expect("exec follow-up");
    assert!(res2.success, "stderr: {}", res2.stderr);
    assert_eq!(res2.stdout.trim(), "device still open: TRUE");

    manager.close_session("r-devoff-pdf");
}

#[test]
fn r_session_handles_unicode_utf8_code_cleanly() {
    let Some(mut manager) = manager_for(CodeLanguage::R) else {
        return;
    };

    let code = r#"
msg <- "Olá mundo! Acentuação e emojis: 🌍 café"
cat(msg, "\n", sep = "")
"#;
    let res = manager
        .execute("r-utf8", &chunk(CodeLanguage::R, code))
        .expect("exec utf8");
    assert!(res.success, "stderr: {}", res.stderr);
    assert!(
        res.stdout
            .contains("Olá mundo! Acentuação e emojis: 🌍 café")
    );

    manager.close_session("r-utf8");
}

#[cfg(unix)]
#[test]
fn session_rejects_non_utf8_working_directory_gracefully() {
    use std::os::unix::ffi::OsStringExt;
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let non_utf8_name = std::ffi::OsString::from_vec(b"/tmp/wck-test-\xff\xfe".to_vec());
    let non_utf8_path = std::path::PathBuf::from(non_utf8_name);

    let mut req = chunk(CodeLanguage::Python, "print('hello')");
    req.working_dir = Some(non_utf8_path);

    let res = manager.execute("py-utf8", &req);
    assert!(res.is_err());
    let err_str = res.expect_err("should reject").to_string();
    assert!(
        err_str.contains("valid UTF-8"),
        "error should state valid UTF-8 requirement: {}",
        err_str
    );
}

#[test]
fn r_session_recovers_after_close_all_connections() {
    let Some(mut manager) = manager_for(CodeLanguage::R) else {
        return;
    };

    let code1 = "val <- 42\ncloseAllConnections()\n";
    let res1 = manager
        .execute("r-close-all", &chunk(CodeLanguage::R, code1))
        .expect("chunk 1");
    assert!(res1.success, "stderr: {}", res1.stderr);

    let code2 = "val * 2\n";
    let res2 = manager
        .execute("r-close-all", &chunk(CodeLanguage::R, code2))
        .expect("chunk 2");
    assert!(res2.success, "stderr: {}", res2.stderr);
    assert!(
        res2.stdout.contains("84"),
        "stdout should contain visible evaluation of 84: {:?}",
        res2.stdout
    );

    manager.close_session("r-close-all");
}

#[test]
fn session_manager_enforces_max_sessions_limit() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };
    manager.set_max_sessions(2);

    let req1 = chunk(CodeLanguage::Python, "x = 10");
    assert!(manager.execute("s1", &req1).expect("s1").success);

    let req2 = chunk(CodeLanguage::Python, "y = 20");
    assert!(manager.execute("s2", &req2).expect("s2").success);

    // Third new session should be rejected because limit of 2 is reached
    let req3 = chunk(CodeLanguage::Python, "z = 30");
    let err = manager.execute("s3", &req3).expect_err("should reject s3");
    match err {
        KernelError::SessionCapacityExceeded { max_sessions } => {
            assert_eq!(max_sessions, 2);
        }
        other => panic!("expected SessionCapacityExceeded, got {other:?}"),
    }

    // Existing session s1 can still be executed without creating a new session
    let req1_check = chunk(CodeLanguage::Python, "print(x)");
    let res1 = manager.execute("s1", &req1_check).expect("s1 check");
    assert_eq!(res1.stdout.trim(), "10");

    manager.close_session("s1");
    manager.close_session("s2");
}

#[test]
fn large_output_sets_truncated_flag_and_bounds_memory() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    // Print > 10 MiB of text
    let req = ExecutionRequest::new(
        CodeLanguage::Python,
        "import sys\nsys.stdout.write('X' * (11 * 1024 * 1024))\nsys.stdout.flush()",
    );
    let res = manager.execute("large-out", &req).expect("large out");
    assert!(res.success, "stderr: {}", res.stderr);
    assert!(res.output_truncated, "expected output_truncated to be true");
    assert!(
        res.stdout.len() <= crate::session::MAX_STREAM_BYTES,
        "stdout size {} should be <= MAX_STREAM_BYTES {}",
        res.stdout.len(),
        crate::session::MAX_STREAM_BYTES
    );

    let invalid_utf8 = ExecutionRequest::new(
        CodeLanguage::Python,
        "import sys\nsys.stdout.buffer.write(b'\\xff' * (5 * 1024 * 1024))",
    );
    let invalid_result = manager
        .execute("large-out", &invalid_utf8)
        .expect("invalid UTF-8 output");
    assert!(invalid_result.success, "stderr: {}", invalid_result.stderr);
    assert!(
        invalid_result.output_truncated,
        "lossy UTF-8 expansion must be reported as truncation"
    );
    assert!(invalid_result.stdout.len() <= crate::session::MAX_STREAM_BYTES);

    // Follow-up execution should work cleanly
    let req2 = chunk(CodeLanguage::Python, "print('clean')");
    let res2 = manager.execute("large-out", &req2).expect("clean");
    assert!(res2.success);
    assert_eq!(res2.stdout.trim(), "clean");

    manager.close_session("large-out");
}

#[test]
fn python_stable_stream_supports_buffer_and_fileno() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let code = r#"
import sys, subprocess
sys.stdout.buffer.write(b"binary stdout\n")
sys.stderr.buffer.write(b"binary stderr\n")
assert sys.stdout.fileno() == 1
assert sys.stderr.fileno() == 2
subprocess.run([sys.executable, "-c", "import sys; sys.stdout.write('from subprocess\\n')"], stdout=sys.stdout)
"#;
    let res = manager
        .execute("py-stream-apis", &chunk(CodeLanguage::Python, code))
        .expect("exec");
    assert!(res.success, "stderr: {}", res.stderr);
    assert!(res.stdout.contains("binary stdout"));
    assert!(res.stdout.contains("from subprocess"));
    assert!(res.stderr.contains("binary stderr"));

    manager.close_session("py-stream-apis");
}

#[test]
fn python_capture_disabled_chunk_does_not_leak_stale_plot() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };
    if !python_has_matplotlib() {
        return;
    }

    // Chunk 1: draw plot with capture disabled
    let req1 = ExecutionRequest::new(
        CodeLanguage::Python,
        "import matplotlib.pyplot as plt\nplt.plot([1, 2, 3])",
    )
    .with_plot_capture(false);
    let res1 = manager.execute("py-stale-fig", &req1).expect("chunk 1");
    assert!(res1.success);
    assert!(res1.plot_png.is_none(), "chunk 1 had capture disabled");

    // Chunk 2: print text with capture enabled (should NOT emit chunk 1's figure)
    let req2 =
        ExecutionRequest::new(CodeLanguage::Python, "print('just text')").with_plot_capture(true);
    let res2 = manager.execute("py-stale-fig", &req2).expect("chunk 2");
    assert!(res2.success);
    assert_eq!(res2.stdout.trim(), "just text");
    assert!(
        res2.plot_png.is_none(),
        "chunk 2 only printed text and should not re-emit chunk 1 figure"
    );

    // Chunk 3: genuine edit to existing figure with capture enabled
    let req3 = ExecutionRequest::new(
        CodeLanguage::Python,
        "plt.title('Updated Title')\nplt.plot([4, 5, 6])",
    )
    .with_plot_capture(true);
    let res3 = manager.execute("py-stale-fig", &req3).expect("chunk 3");
    assert!(res3.success);
    assert!(
        res3.plot_png.is_some(),
        "chunk 3 genuinely edited figure and must be captured"
    );

    manager.close_session("py-stale-fig");
}

#[test]
fn python_session_handles_overwritten_sys_stderr_during_error() {
    let Some(mut manager) = manager_for(CodeLanguage::Python) else {
        return;
    };

    let req1 = chunk(
        CodeLanguage::Python,
        "import sys\nx = 42\nsys.stderr = None\nraise RuntimeError('stderr was None')",
    );
    let res1 = manager
        .execute("py-bad-stderr", &req1)
        .expect("exec bad stderr");
    assert!(!res1.success);
    assert!(res1.stderr.contains("stderr was None"));

    // Session survived and retains state
    let req2 = chunk(CodeLanguage::Python, "print('x is', x)");
    let res2 = manager
        .execute("py-bad-stderr", &req2)
        .expect("exec recovered");
    assert!(res2.success);
    assert_eq!(res2.stdout.trim(), "x is 42");

    manager.close_session("py-bad-stderr");
}

#[cfg(unix)]
#[test]
fn r_session_system2_cat_does_not_hang() {
    let Some(mut manager) = manager_for(CodeLanguage::R) else {
        return;
    };

    // system2 with default stdin gets EOF immediately
    let req1 = chunk(
        CodeLanguage::R,
        "res <- system2('cat', stdout = TRUE)\ncat('length:', length(res), '\n')",
    );
    let res1 = manager.execute("r-system2", &req1).expect("system2 cat");
    assert!(res1.success, "stderr: {}", res1.stderr);
    assert!(res1.stdout.contains("length: 0"));

    // system2 with explicit input works
    let req2 = chunk(
        CodeLanguage::R,
        "res <- system2('cat', input = 'hello-system2', stdout = TRUE)\ncat(res, '\n')",
    );
    let res2 = manager
        .execute("r-system2", &req2)
        .expect("system2 cat with input");
    assert!(res2.success, "stderr: {}", res2.stderr);
    assert!(res2.stdout.contains("hello-system2"));

    manager.close_session("r-system2");
}

#[test]
fn r_session_dispatches_s3_print_from_globalenv() {
    let Some(mut manager) = manager_for(CodeLanguage::R) else {
        return;
    };

    let req1 = chunk(
        CodeLanguage::R,
        "print.custom_type <- function(x, ...) cat('CUSTOM S3 PRINT METHOD\\n')\nx <- structure(list(), class = 'custom_type')",
    );
    let res1 = manager.execute("r-s3", &req1).expect("define s3 method");
    assert!(res1.success);

    // Visible expression of class custom_type should invoke print.custom_type
    let req2 = chunk(CodeLanguage::R, "x");
    let res2 = manager.execute("r-s3", &req2).expect("print s3 object");
    assert!(res2.success);
    assert!(
        res2.stdout.contains("CUSTOM S3 PRINT METHOD"),
        "stdout was: {:?}",
        res2.stdout
    );

    manager.close_session("r-s3");
}

#[test]
fn r_automatic_print_uses_base_generic_when_global_print_is_shadowed() {
    let Some(mut manager) = manager_for(CodeLanguage::R) else {
        return;
    };

    let define_shadow = chunk(
        CodeLanguage::R,
        "print <- function(x, required) stop('shadowed print called')",
    );
    let defined = manager
        .execute("r-print-shadow", &define_shadow)
        .expect("define shadowed print");
    assert!(defined.success, "stderr: {}", defined.stderr);

    let visible = manager
        .execute("r-print-shadow", &chunk(CodeLanguage::R, "42"))
        .expect("automatically print visible value");
    assert!(visible.success, "stderr: {}", visible.stderr);
    assert!(
        visible.stdout.contains("42"),
        "stdout was: {:?}",
        visible.stdout
    );

    manager.close_session("r-print-shadow");
}

#[test]
fn r_session_reports_error_even_after_close_all_connections() {
    let Some(mut manager) = manager_for(CodeLanguage::R) else {
        return;
    };

    let req1 = chunk(
        CodeLanguage::R,
        "val <- 99\ncloseAllConnections()\nstop('boom after close connections')",
    );
    let res1 = manager
        .execute("r-close-conn-err", &req1)
        .expect("exec error");
    assert!(!res1.success);
    assert!(res1.stderr.contains("boom after close connections"));

    // Session remains intact and state was preserved
    let req2 = chunk(CodeLanguage::R, "cat('val:', val, '\n')");
    let res2 = manager
        .execute("r-close-conn-err", &req2)
        .expect("exec next");
    assert!(res2.success);
    assert_eq!(res2.stdout.trim(), "val: 99");

    manager.close_session("r-close-conn-err");
}

#[test]
fn r_session_symbol_print_does_not_fail() {
    let Some(mut manager) = manager_for(CodeLanguage::R) else {
        return;
    };

    let req = chunk(CodeLanguage::R, "quote(not_defined)");
    let res = manager.execute("r-symbol", &req).expect("exec symbol");
    assert!(res.success, "stderr: {}", res.stderr);
    assert!(res.stdout.contains("not_defined"));

    manager.close_session("r-symbol");
}

#[test]
fn session_unrepresentable_timeout_is_rejected() {
    let timeout = Duration::from_millis(u64::MAX);
    if Instant::now().checked_add(timeout).is_none() {
        let Some(mut manager) = manager_for(CodeLanguage::Python) else {
            return;
        };

        let req = ExecutionRequest::new(CodeLanguage::Python, "x = 1").with_timeout(u64::MAX);
        let err = manager
            .execute("py-unrep", &req)
            .expect_err("should reject overflow");
        assert!(matches!(err, KernelError::SessionProtocol(_)));
    }
}
