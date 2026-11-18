use std::collections::BTreeSet;
use std::io::Write;
use std::process::{Command, Stdio};

use crate::templates::{self, Template, TemplateCategory, TemplateParam};
use crate::{CodeLanguage, KernelError, execute, is_language_available, which_binary};

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

/// Parse a rendered script without executing it. `None` means the interpreter
/// is not installed, so the check is skipped rather than failed.
fn syntax_is_valid(template: &Template, source: &str) -> Option<bool> {
    let binary = which_binary(template.language)?;
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

    let mut child = command.spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(source.as_bytes()).ok()?;
    }
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        eprintln!(
            "{}: interpreter rejected the rendered source:\n{}",
            template.id,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Some(output.status.success())
}

#[test]
fn rendered_scripts_parse_in_their_interpreter() {
    for template in templates::all() {
        let source = template.render_default().expect("default render");
        match syntax_is_valid(template, &source) {
            Some(true) => {}
            Some(false) => panic!("{}: rendered source does not parse", template.id),
            None => eprintln!(
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
        if !is_language_available(template.language) {
            eprintln!(
                "skipping execution of {}: {} not found",
                template.id,
                template.language.binary_name()
            );
            continue;
        }
        let missing: Vec<&str> = template
            .requires
            .iter()
            .copied()
            .filter(|package| !package_available(template.language, package))
            .collect();
        if !missing.is_empty() {
            eprintln!(
                "skipping execution of {}: missing package(s) {}",
                template.id,
                missing.join(", ")
            );
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
