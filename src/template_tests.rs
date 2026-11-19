use std::collections::BTreeSet;
use std::io::Write;
use std::process::{Command, Stdio};

use crate::templates::{self, Template, TemplateCategory, TemplateParam};
use crate::{
    CodeLanguage, ExecutionRequest, KernelError, execute, is_language_available, which_binary,
};

const NO_PARAMS: &[TemplateParam] = &[];
const SELF_REFERENTIAL_PARAMS: &[TemplateParam] = &[TemplateParam {
    key: "x",
    label: "X",
    default: "{{title}}",
    description: "Default points at another placeholder on purpose.",
}];

fn fixture(id: &'static str, code: &'static str, params: &'static [TemplateParam]) -> Template {
    Template {
        id,
        title: "Fixture",
        description: "Test fixture.",
        language: CodeLanguage::Python,
        category: TemplateCategory::Diversity,
        requires: &[],
        params,
        code,
    }
}

/// Collect every `{{key}}` in `code`, without resolving anything.
fn placeholders(code: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = code;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            break;
        };
        found.push(after[..end].trim());
        rest = &after[end + 2..];
    }
    found
}

#[test]
fn library_metadata_is_complete_and_consistent() {
    assert!(
        !templates::all().is_empty(),
        "the library must not be empty"
    );

    let mut ids = BTreeSet::new();
    for template in templates::all() {
        assert!(
            ids.insert(template.id),
            "duplicate template id: {}",
            template.id
        );
        assert!(
            !template.title.trim().is_empty(),
            "{}: empty title",
            template.id
        );
        assert!(
            !template.description.trim().is_empty(),
            "{}: empty description",
            template.id
        );
        assert!(
            template.code.starts_with('#'),
            "{}: source must open with a comment explaining its dependencies",
            template.id
        );
        assert!(
            template.code.len() > 100,
            "{}: source is too short to be a useful snippet",
            template.id
        );
        let prefix = match template.language {
            CodeLanguage::R => "r.",
            CodeLanguage::Python => "python.",
        };
        assert!(
            template.id.starts_with(prefix),
            "{}: id does not start with its language prefix '{prefix}'",
            template.id
        );
        for package in template.requires {
            assert!(
                !package.is_empty()
                    && package
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_'),
                "{}: suspicious package name '{package}'",
                template.id
            );
        }

        let mut keys = BTreeSet::new();
        for param in template.params {
            assert!(
                !param.key.is_empty(),
                "{}: parameter with an empty key",
                template.id
            );
            assert!(
                keys.insert(param.key),
                "{}: duplicate parameter '{}'",
                template.id,
                param.key
            );
            assert!(
                !param.label.trim().is_empty(),
                "{}: parameter '{}' has no label",
                template.id,
                param.key
            );
            assert!(
                !param.description.trim().is_empty(),
                "{}: parameter '{}' has no description",
                template.id,
                param.key
            );
            assert!(
                !param.default.is_empty(),
                "{}: parameter '{}' has no default, so the template would not render",
                template.id,
                param.key
            );
            assert!(
                param
                    .key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{}: parameter key '{}' is not a simple identifier",
                template.id,
                param.key
            );
        }

        let used: BTreeSet<&str> = placeholders(template.code).into_iter().collect();
        assert_eq!(
            used, keys,
            "{}: placeholders in the source and declared parameters differ",
            template.id
        );
        assert_eq!(
            templates::find(template.id).map(|found| found.id),
            Some(template.id),
            "{}: find() cannot locate its own id",
            template.id
        );
    }

    for category in templates::categories() {
        assert!(
            templates::by_category(*category).next().is_some(),
            "category '{category}' has no templates"
        );
    }
    for language in [CodeLanguage::R, CodeLanguage::Python] {
        assert!(
            templates::by_language(language).next().is_some(),
            "no templates for {language}"
        );
    }

    assert_eq!(
        templates::find("R.SHANNON-SIMPSON").map(|found| found.id),
        Some("r.shannon-simpson"),
        "find() must ignore ASCII case"
    );
    assert!(templates::find("does.not.exist").is_none());
}

#[test]
fn every_template_renders_with_its_defaults() {
    for template in templates::all() {
        let rendered = template.render_default().expect("default render");
        assert!(
            !rendered.contains("{{"),
            "{}: rendered source still contains a placeholder",
            template.id
        );
        assert_eq!(
            rendered,
            template.render_default().expect("second render"),
            "{}: rendering is not deterministic",
            template.id
        );

        let request = template.execution_request().expect("execution request");
        assert_eq!(request.language, template.language);
        assert_eq!(request.code, rendered);
    }
}

#[test]
fn species_accumulation_template_computes_both_curves() {
    let template = templates::find("r.species-accumulation").expect("built-in template");
    let rendered = template.render_default().expect("default render");

    // The template promises accumulation and rarefaction, so both must run.
    assert!(
        rendered.contains(r#"method = "random""#),
        "the accumulation curve is missing"
    );
    assert!(
        rendered.contains(r#"method = "rarefaction""#),
        "the rarefaction curve is missing"
    );
    assert!(
        rendered.contains("lines(rarefaction"),
        "the rarefaction curve is not drawn"
    );
}

#[test]
fn overrides_replace_only_the_named_placeholder() {
    let template = templates::find("r.shannon-simpson").expect("built-in template");
    let rendered = template
        .render(&[("title", "Three sites <test>")])
        .expect("render");

    assert!(rendered.contains(r#"main = "Three sites <test>""#));
    assert!(!rendered.contains("{{title}}"));
    assert!(
        rendered.contains("data.frame("),
        "other parameters must keep their defaults"
    );

    let duplicated = template
        .render(&[("title", "first"), ("title", "second")])
        .expect("render");
    assert!(duplicated.contains("first"));
    assert!(
        !duplicated.contains("second"),
        "the first value for a repeated key wins"
    );
}

#[test]
fn unknown_overrides_are_rejected() {
    let template = templates::find("python.data-ingest").expect("built-in template");
    match template.render(&[("title", "x")]) {
        Err(KernelError::UnknownTemplateParameter {
            template,
            parameter,
        }) => {
            assert_eq!(template, "python.data-ingest");
            assert_eq!(parameter, "title");
        }
        other => panic!("expected UnknownTemplateParameter, got {other:?}"),
    }
}

#[test]
fn malformed_sources_return_errors() {
    let unterminated = fixture("test.unterminated", "print('{{oops')", NO_PARAMS);
    match unterminated.render_default() {
        Err(KernelError::UnterminatedPlaceholder { template }) => {
            assert_eq!(template, "test.unterminated");
        }
        other => panic!("expected UnterminatedPlaceholder, got {other:?}"),
    }

    let undefined = fixture("test.undefined", "print('{{missing}}')", NO_PARAMS);
    match undefined.render_default() {
        Err(KernelError::UndefinedPlaceholder {
            template,
            placeholder,
        }) => {
            assert_eq!(template, "test.undefined");
            assert_eq!(placeholder, "missing");
        }
        other => panic!("expected UndefinedPlaceholder, got {other:?}"),
    }
}

#[test]
fn values_are_never_rescanned() {
    let template = fixture("test.non-recursive", "a {{x}} b", SELF_REFERENTIAL_PARAMS);
    assert_eq!(
        template.render_default().expect("render"),
        "a {{title}} b",
        "a substituted value must not be parsed again"
    );
}

#[test]
fn hostile_parameters_never_panic() {
    let hostile = [
        "",
        "{{",
        "}}",
        "{{title}}",
        "'; rm -rf / #",
        "\0\"\\",
        "🌿",
        "0.95\nlibrary(evil)",
    ];
    let long_value = "x".repeat(10_000);

    for template in templates::all() {
        for param in template.params {
            for value in hostile
                .iter()
                .copied()
                .chain(std::iter::once(long_value.as_str()))
            {
                let rendered = template
                    .render(&[(param.key, value)])
                    .expect("replacing a declared parameter cannot fail");
                assert!(
                    rendered.contains(value),
                    "{}: value for '{}' was not inserted verbatim",
                    template.id,
                    param.key
                );
                if value.contains("{{") || value.contains("}}") {
                    continue;
                }
                for other in template.params {
                    if other.key == param.key {
                        continue;
                    }
                    let placeholder = format!("{{{{{}}}}}", other.key);
                    assert!(
                        !rendered.contains(&placeholder),
                        "{}: '{}' stayed unresolved after changing '{}'",
                        template.id,
                        other.key,
                        param.key
                    );
                }
            }
        }
    }
}

/// Result of parsing a rendered script without executing it.
enum SyntaxCheck {
    /// The interpreter is not installed, so the check is skipped.
    Skipped,
    /// The interpreter parsed the source.
    Valid,
    /// The interpreter could not parse it, or the subprocess failed.
    Invalid(String),
}

/// Parse a rendered script without executing it.
fn syntax_is_valid(template: &Template, source: &str) -> SyntaxCheck {
    let Some(binary) = which_binary(template.language) else {
        return SyntaxCheck::Skipped;
    };
    let mut command = Command::new(binary);
    match template.language {
        CodeLanguage::R => {
            command.args([
                "--vanilla",
                "-e",
                "invisible(parse(text = readLines('stdin', warn = FALSE)))",
            ]);
        }
        CodeLanguage::Python => {
            command.args(["-c", "import ast, sys; ast.parse(sys.stdin.read())"]);
        }
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return SyntaxCheck::Invalid(format!("spawn failed: {error}")),
    };
    if let Some(mut stdin) = child.stdin.take()
        && let Err(error) = stdin.write_all(source.as_bytes())
    {
        return SyntaxCheck::Invalid(format!("writing to stdin failed: {error}"));
    }
    match child.wait_with_output() {
        Ok(output) if output.status.success() => SyntaxCheck::Valid,
        Ok(output) => SyntaxCheck::Invalid(String::from_utf8_lossy(&output.stderr).into_owned()),
        Err(error) => SyntaxCheck::Invalid(format!("waiting for the interpreter failed: {error}")),
    }
}

#[test]
fn rendered_scripts_parse_in_their_interpreter() {
    for template in templates::all() {
        let source = template.render_default().expect("default render");
        match syntax_is_valid(template, &source) {
            SyntaxCheck::Valid => {}
            SyntaxCheck::Invalid(reason) => {
                panic!("{}: syntax check failed:\n{reason}", template.id);
            }
            SyntaxCheck::Skipped => eprintln!(
                "skipping syntax check for {}: {} not found",
                template.id,
                template.language.binary_name()
            ),
        }
    }
}

/// Whether an interpreter can import a declared third-party package.
fn package_available(language: CodeLanguage, package: &str) -> bool {
    let valid = !package.is_empty()
        && package
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_');
    if !valid {
        return false;
    }
    let Some(binary) = which_binary(language) else {
        return false;
    };
    let script = match language {
        CodeLanguage::R => format!(
            "quit(status = if (requireNamespace(\"{package}\", quietly = TRUE)) 0L else 1L)"
        ),
        CodeLanguage::Python => format!(
            "import importlib.util, sys; sys.exit(0 if importlib.util.find_spec(\"{package}\") else 1)"
        ),
    };
    let status = match language {
        CodeLanguage::R => Command::new(binary)
            .args(["--vanilla", "-e", script.as_str()])
            .status(),
        CodeLanguage::Python => Command::new(binary).args(["-c", script.as_str()]).status(),
    };
    status.map(|status| status.success()).unwrap_or(false)
}

/// Whether the interpreter and every declared package are available.
/// Prints the reason and returns false when the check must be skipped.
fn dependencies_available(template: &Template) -> bool {
    if !is_language_available(template.language) {
        eprintln!(
            "skipping {}: {} not found",
            template.id,
            template.language.binary_name()
        );
        return false;
    }
    let missing: Vec<&str> = template
        .requires
        .iter()
        .copied()
        .filter(|package| !package_available(template.language, package))
        .collect();
    if !missing.is_empty() {
        eprintln!(
            "skipping {}: missing package(s) {}",
            template.id,
            missing.join(", ")
        );
        return false;
    }
    true
}

#[test]
fn templates_execute_when_their_packages_are_installed() {
    const PLOTTING: &[&str] = &[
        "r.shannon-simpson",
        "r.species-accumulation",
        "r.lm-confidence",
        "r.ttest-anova",
        "python.correlation-heatmap",
    ];

    for template in templates::all() {
        if !dependencies_available(template) {
            continue;
        }

        let request = template
            .execution_request()
            .expect("default render")
            .with_timeout(120_000);
        let result = execute(&request).expect("the interpreter is available");
        assert!(
            result.success,
            "{} failed (exit {:?}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
            template.id, result.exit_code, result.stdout, result.stderr
        );
        assert!(
            !result.stdout.trim().is_empty(),
            "{}: snippet printed nothing",
            template.id
        );
        if PLOTTING.contains(&template.id) {
            assert!(
                result.plot_png.is_some(),
                "{}: expected the plot to be captured",
                template.id
            );
        }
    }
}

#[test]
fn correlation_template_supports_its_documented_methods() {
    let template = templates::find("python.correlation-heatmap").expect("built-in template");
    if !dependencies_available(template) {
        return;
    }

    for method in ["pearson", "spearman", "kendall"] {
        let script = template
            .render(&[("method", method)])
            .expect("documented method must render");
        let request = ExecutionRequest::new(template.language, script).with_timeout(120_000);
        let result = execute(&request).expect("the interpreter is available");
        assert!(
            result.success,
            "method {method} failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
            result.stdout, result.stderr
        );
        assert!(
            result.plot_png.is_some(),
            "method {method}: expected the heat map to be captured"
        );
    }
}
