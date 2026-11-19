use super::*;

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

#[test]
fn code_language_parsing() {
    assert_eq!("r".parse::<CodeLanguage>().unwrap(), CodeLanguage::R);
    assert_eq!("R".parse::<CodeLanguage>().unwrap(), CodeLanguage::R);
    assert_eq!(
        "python".parse::<CodeLanguage>().unwrap(),
        CodeLanguage::Python
    );
    assert_eq!("py".parse::<CodeLanguage>().unwrap(), CodeLanguage::Python);
    assert_eq!(
        "Python".parse::<CodeLanguage>().unwrap(),
        CodeLanguage::Python
    );
    assert!("javascript".parse::<CodeLanguage>().is_err());
}

#[test]
fn execution_request_defaults() {
    let req = ExecutionRequest::new(CodeLanguage::Python, "print('hello')");
    assert_eq!(req.language, CodeLanguage::Python);
    assert_eq!(req.timeout_ms, 30_000);
    assert!(req.capture_plots);
    assert_eq!(req.plot_format, PlotFormat::Png);
    assert_eq!(req.code, "print('hello')");

    let customized = req
        .with_timeout(5_000)
        .with_plot_capture(false)
        .with_plot_format(PlotFormat::Svg)
        .with_working_dir("/tmp");
    assert_eq!(customized.timeout_ms, 5_000);
    assert!(!customized.capture_plots);
    assert_eq!(customized.plot_format, PlotFormat::Svg);
    assert_eq!(customized.working_dir, Some(PathBuf::from("/tmp")));
}

#[test]
fn execution_request_deserialization_defaults_plot_options() {
    let omitted =
        r#"{"language":"python","code":"print('hello')","timeout_ms":1000,"capture_plots":true}"#;
    let request: ExecutionRequest = serde_json::from_str(omitted).expect("deserialize request");
    assert_eq!(request.plot_format, PlotFormat::Png);
    assert!(!request.prefer_svg);

    let explicit_format = r#"{"language":"python","code":"","timeout_ms":1000,"capture_plots":false,"plot_format":"svg"}"#;
    let request: ExecutionRequest =
        serde_json::from_str(explicit_format).expect("deserialize explicit format");
    assert_eq!(request.plot_format, PlotFormat::Svg);
    assert!(!request.prefer_svg);

    let explicit_preference = r#"{"language":"python","code":"","timeout_ms":1000,"capture_plots":false,"prefer_svg":true}"#;
    let request: ExecutionRequest =
        serde_json::from_str(explicit_preference).expect("deserialize explicit preference");
    assert_eq!(request.plot_format, PlotFormat::Png);
    assert!(request.prefer_svg);
    assert_eq!(request.effective_plot_format(), PlotFormat::Svg);
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
fn capture_sink_caps_single_burst() {
    let burst = vec![b'x'; MAX_CAPTURE_BYTES + 1024];
    let (sink, _, reader) = spawn_capture(std::io::Cursor::new(burst), MAX_CAPTURE_BYTES)
        .expect("start capture reader");

    reader
        .join()
        .expect("capture reader must not panic")
        .expect("capture reader must succeed");

    assert!(sink.is_truncated());
    assert_eq!(sink.text().len(), MAX_CAPTURE_BYTES);
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

#[cfg(unix)]
#[test]
fn python_output_reader_stops_when_descendant_escapes_process_group() {
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
    let status = Command::new("kill")
        .args(["-KILL", child_pid.as_str()])
        .status()
        .expect("kill escaped test child");
    assert!(status.success());
    assert!(result.success);
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
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
    if !is_language_available(CodeLanguage::Python) {
        return false;
    }
    let req = ExecutionRequest::new(CodeLanguage::Python, "import matplotlib");
    execute(&req).map(|r| r.success).unwrap_or(false)
}

#[test]
fn execution_request_prefer_svg() {
    let req = ExecutionRequest::new(CodeLanguage::R, "plot(1:5)").with_prefer_svg(true);
    assert!(req.prefer_svg);
    assert_eq!(req.plot_format, PlotFormat::Png);
    assert_eq!(req.effective_plot_format(), PlotFormat::Svg);
}

#[test]
fn effective_plot_format_combinations() {
    let base = ExecutionRequest::new(CodeLanguage::Python, "");

    assert_eq!(base.effective_plot_format(), PlotFormat::Png);

    let svg_by_preference = base.clone().with_prefer_svg(true);
    assert_eq!(svg_by_preference.effective_plot_format(), PlotFormat::Svg);

    let both_with_preference = base
        .clone()
        .with_plot_format(PlotFormat::Both)
        .with_prefer_svg(true);
    assert_eq!(
        both_with_preference.effective_plot_format(),
        PlotFormat::Both
    );

    let explicit_svg = base
        .clone()
        .with_prefer_svg(true)
        .with_plot_format(PlotFormat::Svg)
        .with_prefer_svg(false);
    assert_eq!(explicit_svg.effective_plot_format(), PlotFormat::Svg);

    let png_with_preference = base
        .clone()
        .with_plot_format(PlotFormat::Png)
        .with_prefer_svg(true);
    assert_eq!(png_with_preference.effective_plot_format(), PlotFormat::Svg);
}

#[test]
fn svg_completion_filter() {
    let body = "x".repeat(80);

    assert!(is_complete_svg(&format!("<svg>{body}</svg>")));
    assert!(is_complete_svg(&format!("<svg>{body}</svg>\n")));
    assert!(!is_complete_svg(&format!("<svg>{body}")));
    assert!(!is_complete_svg(&format!("<svg>{body}</svg")));
    assert!(!is_complete_svg("<svg></svg>"));
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
