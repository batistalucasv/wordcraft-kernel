#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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
    assert!(
        result
            .stdout
            .trim()
            .contains(expected_dir.to_str().expect("valid utf8"))
    );
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

fn pandas_available() -> bool {
    if !is_language_available(CodeLanguage::Python) {
        return false;
    }
    let request = ExecutionRequest::new(CodeLanguage::Python, "import pandas")
        .with_timeout(60_000)
        .with_plot_capture(false);
    execute(&request)
        .map(|result| result.success)
        .unwrap_or(false)
}

#[test]
fn python_dataframe_last_expression_is_captured() {
    if !pandas_available() {
        return;
    }

    let code = r##"
import pandas as pd
pd.DataFrame({"species": ["setosa", "virginica"], "abundance": [50, 12]})
"##;
    let result =
        execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success);
    assert_eq!(result.tables.len(), 1);
    let table = &result.tables[0];
    assert_eq!(table.headers, vec!["species", "abundance"]);
    assert_eq!(
        table.rows,
        vec![vec!["setosa", "50"], vec!["virginica", "12"]]
    );
}

#[test]
fn python_wc_table_helper_captures_explicit_dataframe() {
    if !pandas_available() {
        return;
    }

    let code = r##"
import pandas as pd
frame = pd.DataFrame({"site": ["A", "B"], "richness": [7, 9]})
wc_table(frame)
print("captured")
"##;
    let result =
        execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success);
    assert_eq!(result.tables.len(), 1);
    assert_eq!(result.tables[0].headers, vec!["site", "richness"]);
    assert_eq!(result.tables[0].rows, vec![vec!["A", "7"], vec!["B", "9"]]);
}

#[test]
fn python_index_is_kept_as_leading_column() {
    if !pandas_available() {
        return;
    }

    let code = r##"
import pandas as pd
pd.DataFrame({"site": ["A", "B"], "richness": [7, 9]}).set_index("site")
"##;
    let result =
        execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success);
    assert_eq!(result.tables.len(), 1);
    assert_eq!(result.tables[0].headers, vec!["", "richness"]);
    assert_eq!(result.tables[0].rows, vec![vec!["A", "7"], vec!["B", "9"]]);
}

#[test]
fn python_without_dataframe_reports_no_tables() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let result = execute(&ExecutionRequest::new(
        CodeLanguage::Python,
        "print('plain text only')",
    ))
    .expect("execution failed");

    assert!(result.success);
    assert!(result.tables.is_empty());
}

#[test]
fn python_cell_escaping_round_trips() {
    if !pandas_available() {
        return;
    }

    let code = r##"
import pandas as pd
pd.DataFrame({
    "label": ['quote " here', "back\\slash", "line\nbreak", "tab\there", "ação", None],
})
"##;
    let result =
        execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success);
    assert_eq!(
        result.tables[0].rows,
        vec![
            vec!["quote \" here"],
            vec!["back\\slash"],
            vec!["line\nbreak"],
            vec!["tab\there"],
            vec!["ação"],
            vec!["NA"],
        ]
    );
}

#[test]
fn r_dataframe_last_expression_is_captured() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r##"
data.frame(species = c("setosa", "virginica"), abundance = c(50L, 12L))
"##;
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code)).expect("execution failed");

    assert!(result.success);
    assert_eq!(result.tables.len(), 1);
    let table = &result.tables[0];
    assert_eq!(table.headers, vec!["species", "abundance"]);
    assert_eq!(
        table.rows,
        vec![vec!["setosa", "50"], vec!["virginica", "12"]]
    );
}

#[test]
fn r_wc_table_helper_captures_explicit_dataframe() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r##"
table <- data.frame(site = c("A", "B"), richness = c(7L, 9L))
wc_table(table)
cat("captured\n")
"##;
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code)).expect("execution failed");

    assert!(result.success);
    assert_eq!(result.tables.len(), 1);
    assert_eq!(result.tables[0].headers, vec!["site", "richness"]);
    assert_eq!(result.tables[0].rows, vec![vec!["A", "7"], vec!["B", "9"]]);
}

#[test]
fn r_anova_table_keeps_term_names() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r##"
set.seed(1)
response <- rnorm(12)
group <- rep(c("control", "treated"), each = 6)
anova(lm(response ~ group))
"##;
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code)).expect("execution failed");

    assert!(result.success);
    assert_eq!(result.tables.len(), 1);
    let table = &result.tables[0];
    assert_eq!(table.headers.first().map(String::as_str), Some(""));
    assert_eq!(table.headers.get(1).map(String::as_str), Some("Df"));
    assert_eq!(table.rows.len(), 2);
    assert_eq!(table.rows[0][0], "group");
    assert_eq!(table.rows[1][0], "Residuals");
    assert!(
        table
            .rows
            .iter()
            .all(|row| row.len() == table.headers.len())
    );
}

#[test]
fn r_cell_escaping_round_trips() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r##"
data.frame(label = c("quote \" here", "back\\slash", "line\nbreak", "tab\there", "ação", NA_character_))
"##;
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code)).expect("execution failed");

    assert!(result.success);
    assert_eq!(
        result.tables[0].rows,
        vec![
            vec!["quote \" here"],
            vec!["back\\slash"],
            vec!["line\nbreak"],
            vec!["tab\there"],
            vec!["ação"],
            vec!["NA"],
        ]
    );
}

#[test]
fn r_without_dataframe_reports_no_tables() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let result = execute(&ExecutionRequest::new(
        CodeLanguage::R,
        "cat('plain text only\n')",
    ))
    .expect("execution failed");

    assert!(result.success);
    assert!(result.tables.is_empty());
}

#[test]
fn script_written_table_payload_is_not_trusted() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    // The working directory is the scratch directory, so the script can overwrite
    // the payload the wrapper reads back. Non-string cells must not deserialize.
    let code = r##"
with open("__wordcraft_tables__.json", "w", encoding="utf-8") as handle:
    handle.write('{"tables":[{"headers":["a"],"rows":[[1,2]]}]}')
print("done")
"##;
    let result =
        execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success);
    assert!(result.tables.is_empty());
}

#[test]
fn table_payload_is_clamped_and_rectangular() {
    let unique_id = format!(
        "wordcraft-tables-unit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let dir = std::env::temp_dir().join(unique_id);
    create_scratch_dir(&dir).expect("create scratch dir");
    let _guard = TempDirGuard(&dir);
    let path = dir.join("tables.json");

    let long_cell = "x".repeat(MAX_CELL_CHARS + 10);
    let payload = format!(
        r#"{{"tables":[{{"headers":["a","b"],"rows":[["only-a"],["1","2","3"],["{long_cell}"]]}}]}}"#
    );
    fs::write(&path, payload).expect("write payload");

    let tables = read_tables(&path);
    assert_eq!(tables.len(), 1);
    let table = &tables[0];
    assert_eq!(table.headers, vec!["a", "b"]);
    assert_eq!(table.rows.len(), 3);
    assert_eq!(table.rows[0], vec!["only-a", ""]);
    assert_eq!(table.rows[1], vec!["1", "2"]);
    assert_eq!(table.rows[2][0].chars().count(), MAX_CELL_CHARS);
    assert_eq!(table.rows[2][1], "");
}

#[test]
fn malformed_table_payload_is_ignored() {
    let unique_id = format!(
        "wordcraft-tables-bad-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let dir = std::env::temp_dir().join(unique_id);
    create_scratch_dir(&dir).expect("create scratch dir");
    let _guard = TempDirGuard(&dir);
    let path = dir.join("tables.json");
    fs::write(&path, "not json at all").expect("write payload");

    assert!(read_tables(&path).is_empty());
}

#[test]
fn execution_result_deserializes_without_tables_field() {
    let json = r#"{"stdout":"","stderr":"","exit_code":0,"success":true,"timed_out":false,"plot_png":null,"duration_ms":0}"#;
    let result: ExecutionResult = serde_json::from_str(json).expect("deserialize result");
    assert!(result.tables.is_empty());
}

#[test]
fn r_missing_factor_is_captured_as_na() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r#"
data.frame(group = factor(c("A", NA)))
"#;
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code)).expect("execution failed");
    assert!(result.success);
    assert_eq!(result.tables.len(), 1);
    assert_eq!(result.tables[0].rows, vec![vec!["A"], vec!["NA"]]);
}

#[test]
fn r_visible_non_dataframe_is_printed_to_stdout() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r#"
1 + 1
cat("done\n")
"#;
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code)).expect("execution failed");
    assert!(result.success);
    assert!(result.stdout.contains("[1] 2"));
    assert!(result.stdout.contains("done"));
}

#[test]
fn python_user_json_import_does_not_shadow_table_serialization() {
    if !pandas_available() {
        return;
    }

    let code = r#"
import pandas as pd
df = pd.DataFrame({"x": [1, 2]})
wc_table(df)
json = {"shadow": True}
print("ok")
"#;
    let result =
        execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");
    assert!(result.success);
    assert_eq!(result.tables.len(), 1);
    assert_eq!(result.tables[0].rows, vec![vec!["1"], vec!["2"]]);
}

#[test]
fn python_preserves_datetime_and_float_precision() {
    if !pandas_available() {
        return;
    }

    let code = r#"
import pandas as pd
pd.DataFrame({
    "dt": [pd.Timestamp("2023-01-01 12:00:00")],
    "flt": [0.1234567890123],
})
"#;
    let result =
        execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");
    assert!(result.success);
    assert_eq!(result.tables.len(), 1);
    assert_eq!(result.tables[0].rows[0][0], "2023-01-01 12:00:00");
    assert_eq!(result.tables[0].rows[0][1], "0.1234567890123");
}

#[test]
fn python_string_index_is_kept_as_leading_column() {
    if !pandas_available() {
        return;
    }

    let code = r#"
import pandas as pd
pd.DataFrame({"value": [7, 9]}, index=["0", "1"])
"#;
    let result =
        execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");
    assert!(result.success);
    assert_eq!(result.tables.len(), 1);
    assert_eq!(result.tables[0].headers, vec!["", "value"]);
    assert_eq!(result.tables[0].rows, vec![vec!["0", "7"], vec!["1", "9"]]);
}

#[test]
fn python_final_wc_table_call_does_not_capture_twice() {
    if !pandas_available() {
        return;
    }

    let code = r#"
import pandas as pd
df = pd.DataFrame({"x": [1, 2]})
wc_table(df)
"#;
    let result =
        execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");
    assert!(result.success);
    assert_eq!(result.tables.len(), 1);
}

#[test]
fn python_table_rows_clamped_to_limit() {
    if !pandas_available() {
        return;
    }

    let code = r#"
import pandas as pd
pd.DataFrame({"x": range(10050)})
"#;
    let result =
        execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");
    assert!(result.success);
    assert_eq!(result.tables.len(), 1);
    assert_eq!(result.tables[0].rows.len(), 10_000);
}

#[test]
fn r_table_rows_clamped_to_limit() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r#"
data.frame(x = 1:10050)
"#;
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code)).expect("execution failed");
    assert!(result.success);
    assert_eq!(result.tables.len(), 1);
    assert_eq!(result.tables[0].rows.len(), 10_000);
}

#[test]
fn r_driver_state_is_isolated_from_user_assignments() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r#"
df0 <- data.frame(first = 1L)
wc_table(df0)
.wc_capture <- NULL
.wc_tables <- "corrupted"
.wc_tables_file <- "/tmp/evil"
rm(list = ls(all.names = TRUE))
df1 <- data.frame(second = 2L)
df1
"#;
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code)).expect("execution failed");
    assert!(result.success);
    assert_eq!(result.tables.len(), 2);
    assert_eq!(result.tables[0].headers, vec!["first"]);
    assert_eq!(result.tables[0].rows, vec![vec!["1"]]);
    assert_eq!(result.tables[1].headers, vec!["second"]);
    assert_eq!(result.tables[1].rows, vec![vec!["2"]]);
}

#[test]
fn r_control_characters_including_form_feed_round_trip() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r#"
data.frame(label = c("page1\fpage2", "bell\bhere", "vert\vtab"))
"#;
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code)).expect("execution failed");
    assert!(result.success);
    assert_eq!(result.tables.len(), 1);
    assert_eq!(
        result.tables[0].rows,
        vec![
            vec!["page1\x0cpage2"],
            vec!["bell\x08here"],
            vec!["vert\x0btab"],
        ]
    );
}

#[cfg(unix)]
#[test]
fn test_scratch_dir_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let unique_id = format!("wordcraft_test_perm_{}", std::process::id());
    let temp_dir = std::env::temp_dir().join(&unique_id);
    let res = create_scratch_dir(&temp_dir);
    assert!(res.is_ok());
    let guard = TempDirGuard(&temp_dir);
    let meta = fs::metadata(&temp_dir).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    drop(guard);
    assert!(!temp_dir.exists());
}

#[test]
fn test_substitute_markers_single_pass() {
    let template = "START __TABLES__ MIDDLE __PLOT_SETUP__ END";
    let replacements = [
        ("__TABLES__", "tbl_path_with___PLOT_SETUP___inside"),
        ("__PLOT_SETUP__", "setup_code"),
    ];
    let result = substitute_markers(template, &replacements);
    assert_eq!(
        result,
        "START tbl_path_with___PLOT_SETUP___inside MIDDLE setup_code END"
    );
}

#[test]
fn test_python_table_lone_surrogates() {
    if !pandas_available() {
        return;
    }
    let code = r#"
import pandas as pd
df = pd.DataFrame({"col": ["hello \ud800 world", "valid"]}, dtype=object)
wc_table(df)
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.tables.len(), 1);
    assert_eq!(res.tables[0].rows.len(), 2);
    assert!(
        res.tables[0].rows[0][0].contains('\u{fffd}') || res.tables[0].rows[0][0].contains('?')
    );
    assert_eq!(res.tables[0].rows[1][0], "valid");
}

#[test]
fn test_python_table_payload_budget_preserves_earlier_table() {
    if !pandas_available() {
        return;
    }
    let code = r#"
import pandas as pd
__wc_budget__ = 50000
df1 = pd.DataFrame({"small": [1, 2, 3]})
wc_table(df1)
df2 = pd.DataFrame({"big1": ["x" * 4000] * 50, "big2": ["y" * 4000] * 50})
wc_table(df2)
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.tables.len(), 2);
    assert_eq!(res.tables[0].headers, vec!["small"]);
    assert_eq!(res.tables[0].rows.len(), 3);
    assert!(res.tables[1].rows.len() < 50);
    assert!(!res.tables[1].rows.is_empty());
}

#[test]
fn test_r_table_payload_budget_preserves_earlier_table() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let code = r#"
.wc_budget <- 20000L
df1 <- data.frame(small = c(1, 2, 3))
wc_table(df1)
df2 <- data.frame(big1 = rep(paste(rep("x", 1000), collapse=""), 50))
wc_table(df2)
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.tables.len(), 2);
    assert_eq!(res.tables[0].headers, vec!["small"]);
    assert_eq!(res.tables[0].rows.len(), 3);
    assert!(res.tables[1].rows.len() < 50);
    assert!(!res.tables[1].rows.is_empty());
}

#[test]
fn test_r_user_file_override_does_not_break_table_writer() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let code = r#"
file <- function(...) stop("user file overridden")
df <- data.frame(x = 1L)
wc_table(df)
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.tables.len(), 1);
    assert_eq!(res.tables[0].rows, vec![vec!["1"]]);
}

#[test]
fn test_r_subclass_row_selection_does_not_drop_rows() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let code = r#"
df <- data.frame(a = 1:5, b = 6:10)
class(df) <- c("custom_table", "data.frame")
`[.custom_table` <- function(x, i, ...) {
    as.data.frame(x)[i, , drop = FALSE]
}
wc_table(df)
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.tables.len(), 1);
    assert_eq!(res.tables[0].rows.len(), 5);
}

#[test]
fn test_r_duplicate_headers_preserved() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let code = r#"
data.frame(x = 1L, x = 2L, check.names = FALSE)
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.tables.len(), 1);
    assert_eq!(res.tables[0].headers, vec!["x", "x"]);
    assert_eq!(res.tables[0].rows, vec![vec!["1", "2"]]);
}

#[test]
fn test_r_matrix_capture() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let code = r#"
matrix(1:4, nrow = 2)
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.tables.len(), 1);
    assert_eq!(res.tables[0].headers, vec!["V1", "V2"]);
    assert_eq!(res.tables[0].rows, vec![vec!["1", "3"], vec!["2", "4"]]);
    assert!(
        res.stdout.is_empty(),
        "expected empty stdout, got: {}",
        res.stdout
    );
}

#[test]
fn test_r_matrix_with_dimnames_and_wc_table() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let code = r#"
m <- matrix(1:4, nrow = 2, dimnames = list(c("r1", "r2"), c("c1", "c2")))
wc_table(m)
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.tables.len(), 1);
    assert_eq!(res.tables[0].headers, vec!["", "c1", "c2"]);
    assert_eq!(
        res.tables[0].rows,
        vec![vec!["r1", "1", "3"], vec!["r2", "2", "4"]]
    );
}

#[test]
fn test_r_eval_override_and_last_value() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let code = r#"
eval <- function(...) stop("user eval overridden")
x <- 42L
cat("Last value is: ", .Last.value, "\n", sep = "")
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert!(res.stdout.contains("Last value is: 42"));
}

#[test]
fn test_r_compact_row_names_not_expanded() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let code = r#"
df <- data.frame(val = 1:50)
df
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.tables.len(), 1);
    assert_eq!(res.tables[0].headers, vec!["val"]);
    assert_eq!(res.tables[0].rows.len(), 50);
}

#[test]
fn test_python_scalar_check_for_isna() {
    if !pandas_available() {
        return;
    }
    let code = r#"
import pandas as pd
import numpy as np

df = pd.DataFrame({"v": [np.array([np.nan])]})
df
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.tables.len(), 1);
    assert_eq!(res.tables[0].headers, vec!["v"]);
    assert_ne!(res.tables[0].rows[0][0], "NA");
    assert!(res.tables[0].rows[0][0].contains("nan"));
}

#[test]
fn test_python_float_index_retained_as_labels() {
    if !pandas_available() {
        return;
    }
    let code = r#"
import pandas as pd
df = pd.DataFrame({"x": [10, 20]}, index=[0.0, 1.0])
df
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.tables.len(), 1);
    assert_eq!(res.tables[0].headers, vec!["", "x"]);
    assert_eq!(res.tables[0].rows[0], vec!["0.0", "10"]);
    assert_eq!(res.tables[0].rows[1], vec!["1.0", "20"]);
}

#[test]
fn test_scratch_path_arguments_passed_safely() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let temp_root = std::env::temp_dir().join("wck test path with spaces and 'quotes'");
    let _ = fs::create_dir_all(&temp_root);
    struct CleanGuard<'a>(&'a Path);
    impl Drop for CleanGuard<'_> {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0);
        }
    }
    let _guard = CleanGuard(&temp_root);

    let code = r#"
df <- data.frame(status = "ok")
cat("path safety check ok\n")
df
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code).with_working_dir(&temp_root);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert!(res.stdout.contains("path safety check ok"));
    assert_eq!(res.tables.len(), 1);
    assert_eq!(res.tables[0].headers, vec!["status"]);
    assert_eq!(res.tables[0].rows, vec![vec!["ok"]]);
}

#[test]
fn r_command_args_are_empty_and_tables_captured() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r#"
args <- commandArgs(trailingOnly = TRUE)
if (length(args) != 0L) {
    stop(paste("Expected 0 args, got", length(args)))
}
data.frame(status = "empty_args_ok")
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.tables.len(), 1);
    assert_eq!(res.tables[0].headers, vec!["status"]);
    assert_eq!(res.tables[0].rows, vec![vec!["empty_args_ok"]]);
}

#[test]
fn r_compact_row_names_explicit_sequence_not_treated_as_labels() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r#"
df <- data.frame(val = 1:4)
row.names(df) <- 1:4
df
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.tables.len(), 1);
    assert_eq!(res.tables[0].headers, vec!["val"]);
    assert_eq!(
        res.tables[0].rows,
        vec![vec!["1"], vec!["2"], vec!["3"], vec!["4"]]
    );
}

#[test]
fn r_matrix_large_slice_bounds_before_copy() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r#"
mat <- matrix(1:30000, nrow = 100, ncol = 300)
colnames(mat) <- paste0("c", 1:300)
rownames(mat) <- paste0("r", 1:100)
mat
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.tables.len(), 1);
    let table = &res.tables[0];
    assert_eq!(table.headers.len(), 256);
    assert_eq!(table.headers[0], "");
    assert_eq!(table.headers[1], "c1");
    assert_eq!(table.headers[255], "c255");
    assert_eq!(table.rows.len(), 100);
    assert_eq!(table.rows[0].len(), 256);
    assert_eq!(table.rows[0][0], "r1");
}

#[test]
fn python_file_dunder_points_to_user_script() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let code = r#"
import sys
from pathlib import Path
file_name = Path(__file__).name
argv_name = Path(sys.argv[0]).name
assert file_name.startswith("user."), f"file is {file_name}"
assert argv_name.startswith("user."), f"argv is {argv_name}"
print("file_dunder_ok")
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert!(res.stdout.contains("file_dunder_ok"));
}

#[test]
fn python_dataframe_index_only_no_data_columns_captured() {
    if !pandas_available() {
        return;
    }

    let code = r#"
import pandas as pd
pd.DataFrame(index=["site-a", "site-b"])
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.tables.len(), 1);
    assert_eq!(res.tables[0].headers, vec![""]);
    assert_eq!(res.tables[0].rows, vec![vec!["site-a"], vec!["site-b"]]);
}

#[test]
fn python_dataframe_256_columns_with_index_limited_to_256_slots() {
    if !pandas_available() {
        return;
    }

    let code = r#"
import pandas as pd
pd.DataFrame({f"c{i}": [1] for i in range(256)}, index=["row1"])
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.tables.len(), 1);
    let table = &res.tables[0];
    assert_eq!(table.headers.len(), 256);
    assert_eq!(table.headers[0], "");
    assert_eq!(table.headers[1], "c0");
    assert_eq!(table.headers[255], "c254");
    assert_eq!(table.rows.len(), 1);
    assert_eq!(table.rows[0].len(), 256);
    assert_eq!(table.rows[0][0], "row1");
}

#[test]
fn r_empty_dataframe_declined_by_capture_is_printed_to_stdout() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = "data.frame()";
    let req = ExecutionRequest::new(CodeLanguage::R, code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert!(
        res.tables.is_empty(),
        "empty data frame must not be stored in tables"
    );
    assert!(
        res.stdout.contains("data frame") || res.stdout.contains("0"),
        "declined empty data frame must be printed to stdout: {}",
        res.stdout
    );
}

#[test]
fn r_tables_beyond_capacity_are_printed_to_stdout() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let mut code = String::new();
    for i in 1..=17 {
        code.push_str(&format!("data.frame(tbl_id = {i})\n"));
    }
    let req = ExecutionRequest::new(CodeLanguage::R, &code);
    let res = execute(&req).expect("execution should succeed");
    assert!(res.success, "stderr: {}", res.stderr);
    assert_eq!(res.tables.len(), 16, "exactly 16 tables should be captured");
    assert!(
        res.stdout.contains("tbl_id") && res.stdout.contains("17"),
        "the 17th declined table must be printed to stdout: {}",
        res.stdout
    );
}

#[test]
fn table_payload_concurrent_growth_beyond_limit_is_rejected() {
    let unique_id = format!(
        "wordcraft-tables-grow-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let dir = std::env::temp_dir().join(unique_id);
    create_scratch_dir(&dir).expect("create scratch dir");
    let _guard = TempDirGuard(&dir);
    let path = dir.join("tables.json");

    // Write a small payload that passes the metadata size check.
    let small_payload = br#"{"tables":[{"headers":["h"],"rows":[["val"]]}]}"#;
    fs::write(&path, small_payload).expect("write small payload");

    // Open the file handle while it is within the 64 MiB limit (simulating TOCTOU).
    let file = fs::File::open(&path).expect("open file for reading");

    // Concurrently grow the file beyond the MAX_TABLES_FILE_BYTES limit.
    // set_len expands the file metadata/content instantly without disk thrashing.
    let grow_file = fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open for writing");
    grow_file
        .set_len(MAX_TABLES_FILE_BYTES + 1024)
        .expect("expand file past limit");

    // Re-reading from the concurrently expanded file handle must reject the oversized payload
    // without reading beyond MAX_TABLES_FILE_BYTES + 1 bytes.
    let result = read_tables_from_reader(file, MAX_TABLES_FILE_BYTES, MAX_TOTAL_CELLS);
    assert!(
        result.is_empty(),
        "concurrently grown file exceeding byte limit must be rejected"
    );

    // Also verify that read_tables on the path directly rejects it.
    let path_result = read_tables(&path);
    assert!(
        path_result.is_empty(),
        "oversized file must be rejected by read_tables"
    );
}

#[test]
fn table_payload_ragged_zip_bomb_memory_amplification_is_rejected() {
    let unique_id = format!(
        "wordcraft-tables-zipbomb-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let dir = std::env::temp_dir().join(unique_id);
    create_scratch_dir(&dir).expect("create scratch dir");
    let _guard = TempDirGuard(&dir);
    let path = dir.join("tables.json");

    // Construct a small JSON payload (< 10 KB) declaring 256 headers and 1,000 empty rows ([]).
    // This projects to 256,000 cells (> MAX_TOTAL_CELLS = 250,000).
    // Without the projection check, padding would allocate 256,000 String slots.
    let headers: Vec<String> = (0..256).map(|i| format!("\"c{i}\"")).collect();
    let empty_rows: Vec<&str> = vec!["[]"; 1_000];
    let payload = format!(
        r#"{{"tables":[{{"headers":[{}],"rows":[{}]}}]}}"#,
        headers.join(","),
        empty_rows.join(",")
    );

    fs::write(&path, payload).expect("write zip bomb payload");

    let tables = read_tables(&path);
    assert!(
        tables.is_empty(),
        "payload exceeding aggregate cell projection budget must be rejected before padding"
    );
}

#[test]
fn read_tables_from_reader_rejects_payload_with_extra_byte_beyond_limit() {
    use std::io::Cursor;
    // Simulate reader that returns max_bytes + 1 bytes.
    let small_limit = 100;
    let mut data = Vec::new();
    data.extend_from_slice(b"{\"tables\":[]}");
    data.resize(small_limit + 1, b' ');
    let reader = Cursor::new(data);
    let result = read_tables_from_reader(reader, small_limit as u64, MAX_TOTAL_CELLS);
    assert!(
        result.is_empty(),
        "payload with extra byte beyond limit must be rejected"
    );
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
        directory.contains("wordcraft_"),
        "unexpected working directory: {directory}"
    );
    assert!(
        !std::path::Path::new(directory).exists(),
        "temporary directory was left behind: {directory}"
    );
}
