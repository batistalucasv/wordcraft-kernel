use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python).is_none() {
        return false;
    }
    Command::new(binary_path())
        .args(["--lang", "py", "-e", "import matplotlib"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
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

    let temp_dir = TestDir::new("wc-test");
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

    let temp_dir = TestDir::new("wc-plot-test");
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

    let temp_dir = TestDir::new("wc-svg-test");
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
fn cli_rejects_conflicting_plot_format_and_extension() {
    let temp_dir = TestDir::new("wc-format-conflict");
    let plot_file = temp_dir.join("figure.svg");

    let output = Command::new(binary_path())
        .args([
            "--lang",
            "py",
            "-e",
            "print(1)",
            "--plot-format",
            "png",
            "--out-plot",
            plot_file.to_str().expect("valid path"),
        ])
        .output()
        .expect("failed to execute wc-kernel");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("conflicting --plot-format"));
    assert!(!plot_file.exists(), "conflicting export must not write");
}

#[test]
fn cli_out_plot_svg_does_not_fall_back_to_png() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python).is_none() {
        eprintln!("Skipping Python test: python3 not found");
        return;
    }

    let temp_dir = TestDir::new("wc-svg-no-fallback");
    let plot_file = temp_dir.join("figure.svg");

    let output = Command::new(binary_path())
        .args([
            "--lang",
            "py",
            "-e",
            "print('no plot here')",
            "--out-plot",
            plot_file.to_str().expect("valid path"),
        ])
        .output()
        .expect("failed to execute wc-kernel");

    assert!(output.status.success());
    assert!(
        !plot_file.exists(),
        "SVG target must not receive PNG bytes when no SVG was captured"
    );
}

#[test]
fn cli_large_stdout_does_not_deadlock() {
    if wordcraft_kernel::which_binary(wordcraft_kernel::CodeLanguage::Python).is_none() {
        eprintln!("Skipping Python test: python3 not found");
        return;
    }

    let output = Command::new(binary_path())
        .args(["--lang", "py", "-e", "print('x' * 1_000_000)"])
        .output()
        .expect("failed to execute wc-kernel");

    assert!(output.status.success());
    assert!(
        output.stdout.len() >= 1_000_000,
        "large stdout must be drained without truncation"
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
