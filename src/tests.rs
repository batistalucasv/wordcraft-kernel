use super::*;

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
        .with_plot_format(PlotFormat::Svg);
    assert_eq!(customized.timeout_ms, 5_000);
    assert!(!customized.capture_plots);
    assert_eq!(customized.plot_format, PlotFormat::Svg);
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
    assert!(result.plot_png.is_none());
    assert!(result.plot_svg.is_none());
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
    let png_bytes = result
        .plot_png
        .expect("Expected plot PNG bytes to be captured");
    assert!(result.plot_svg.is_none());
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
    let svg = result
        .plot_svg
        .expect("Expected plot SVG string to be captured");
    assert!(svg.contains("<svg"), "SVG output must contain '<svg'");
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
    let svg = result.plot_svg.expect("Expected SVG string");
    assert!(svg.contains("<svg"));
}

#[test]
fn r_grid_plot_capture() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let has_ggplot2 = Command::new(CodeLanguage::R.binary_name())
        .args([
            "--vanilla",
            "-e",
            "quit(status = if (requireNamespace('ggplot2', quietly = TRUE)) 0L else 1L)",
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !has_ggplot2 {
        return;
    }

    let code = r#"
library(ggplot2)
print(
  ggplot(mtcars, aes(x = wt, y = mpg)) +
    geom_point() +
    theme_minimal()
)
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert!(
        result.plot_png.is_some(),
        "grid-based plots (ggplot2, lattice) must be captured too"
    );
}

#[test]
fn execution_directory_is_removed_after_execution() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    // The snippet leaves an extra file behind, as the ingestion template does.
    let code = "import os\nprint(os.getcwd())\nopen('extra-output.csv', 'w').write('x')\n";
    let result =
        execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    let directory = result.stdout.trim();
    assert!(
        directory.contains("wordcraft-chunk-"),
        "unexpected working directory: {directory}"
    );
    assert!(
        !std::path::Path::new(directory).exists(),
        "temporary directory was left behind: {directory}"
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
    assert_eq!(req.plot_format, PlotFormat::Svg);
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
    let png_bytes = result
        .plot_png
        .expect("expected the Python plot to be captured");
    assert!(png_bytes.starts_with(&[0x89, b'P', b'N', b'G']));
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
    let svg = result.plot_svg.expect("Expected SVG plot output");
    assert!(svg.contains("<svg") && svg.contains("</svg>"));
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
    let svg = result.plot_svg.expect("Expected SVG in Both mode");
    assert!(svg.contains("<svg"));
}
