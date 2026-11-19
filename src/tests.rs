use super::*;

#[test]
fn code_language_parsing() {
    assert!(matches!("r".parse::<CodeLanguage>(), Ok(CodeLanguage::R)));
    assert!(matches!("R".parse::<CodeLanguage>(), Ok(CodeLanguage::R)));
    assert!(matches!(
        "python".parse::<CodeLanguage>(),
        Ok(CodeLanguage::Python)
    ));
    assert!(matches!(
        "py".parse::<CodeLanguage>(),
        Ok(CodeLanguage::Python)
    ));
    assert!(matches!(
        "Python".parse::<CodeLanguage>(),
        Ok(CodeLanguage::Python)
    ));
    assert!("javascript".parse::<CodeLanguage>().is_err());
}

#[test]
fn execution_request_defaults() {
    let request = ExecutionRequest::new(CodeLanguage::Python, "print('hello')");
    assert_eq!(request.language, CodeLanguage::Python);
    assert_eq!(request.timeout_ms, 30_000);
    assert!(request.capture_plots);
    assert_eq!(request.plot_format, PlotFormat::Png);
    assert_eq!(request.code, "print('hello')");
    assert!(request.supervisor_path.is_none());

    let customized = request
        .with_timeout(5_000)
        .with_plot_capture(false)
        .with_plot_format(PlotFormat::Svg)
        .with_supervisor_path("/tmp/wc-kernel-supervisor")
        .with_working_dir("/tmp");
    assert_eq!(customized.timeout_ms, 5_000);
    assert!(!customized.capture_plots);
    assert_eq!(customized.plot_format, PlotFormat::Svg);
    assert_eq!(customized.working_dir, Some(PathBuf::from("/tmp")));
    assert_eq!(
        customized.supervisor_path,
        Some(PathBuf::from("/tmp/wc-kernel-supervisor"))
    );
}

#[test]
fn execution_request_deserialization_defaults_plot_options() -> Result<(), serde_json::Error> {
    let omitted =
        r#"{"language":"python","code":"print('hello')","timeout_ms":1000,"capture_plots":true}"#;
    let request: ExecutionRequest = serde_json::from_str(omitted)?;
    assert_eq!(request.plot_format, PlotFormat::Png);
    assert!(!request.prefer_svg);
    assert!(request.supervisor_path.is_none());

    let explicit_format = r#"{"language":"python","code":"","timeout_ms":1000,"capture_plots":false,"plot_format":"svg"}"#;
    let request: ExecutionRequest = serde_json::from_str(explicit_format)?;
    assert_eq!(request.plot_format, PlotFormat::Svg);
    assert!(!request.prefer_svg);

    let explicit_preference = r#"{"language":"python","code":"","timeout_ms":1000,"capture_plots":false,"prefer_svg":true}"#;
    let request: ExecutionRequest = serde_json::from_str(explicit_preference)?;
    assert_eq!(request.plot_format, PlotFormat::Png);
    assert!(request.prefer_svg);
    assert_eq!(request.effective_plot_format(), PlotFormat::Svg);
    Ok(())
}

#[test]
fn effective_plot_format_combinations() {
    let base = ExecutionRequest::new(CodeLanguage::Python, "");
    assert_eq!(base.effective_plot_format(), PlotFormat::Png);
    assert_eq!(
        base.clone().with_prefer_svg(true).effective_plot_format(),
        PlotFormat::Svg
    );
    assert_eq!(
        base.clone()
            .with_plot_format(PlotFormat::Both)
            .with_prefer_svg(true)
            .effective_plot_format(),
        PlotFormat::Both
    );
    assert_eq!(
        base.clone()
            .with_prefer_svg(true)
            .with_plot_format(PlotFormat::Svg)
            .with_prefer_svg(false)
            .effective_plot_format(),
        PlotFormat::Svg
    );
    assert_eq!(
        base.with_plot_format(PlotFormat::Png)
            .with_prefer_svg(true)
            .effective_plot_format(),
        PlotFormat::Svg
    );
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
