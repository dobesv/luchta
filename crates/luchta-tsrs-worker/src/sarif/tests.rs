use std::collections::BTreeMap;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::Arc;

use luchta_worker::sarif::SarifFinding;
use serde_json::{json, Value};
use tsrs_ast::{
    new_ad_hoc_message, new_compiler_diagnostic, new_external_diagnostic, DiagnosticExt,
};
use tsrs_compiler::{
    free_unshared_program, get_diagnostics_of_any_program, new_cached_fs_compiler_host,
    new_program, Context, Program, ProgramOptions,
};
use tsrs_core::{TextRange, P};
use tsrs_diagnostics::Category;
use tsrs_tsoptions::{get_parsed_command_line_of_config_file, ParseConfigHost};
use tsrs_vfs::{bundled, vfstest, FS};

use super::{
    build_sarif, convert_diagnostics, repo_relative_sarif_uri, MIME_TYPE, REPORT_FILENAME,
};

const REPO: &str = "/repo";
const PACKAGE: &str = "/repo/packages/pkg";
const SOURCE: &str = "/repo/packages/pkg/src/index.ts";
const CONFIG: &str = "/repo/packages/pkg/tsconfig.json";
const STRICT_CONFIG: &str =
    r#"{"compilerOptions":{"strict":true,"noEmit":true},"files":["src/index.ts"]}"#;

struct ConfigHost(Arc<dyn FS>);

impl ParseConfigHost for ConfigHost {
    fn fs(&self) -> &dyn FS {
        self.0.as_ref()
    }

    fn get_current_directory(&self) -> &str {
        PACKAGE
    }
}

fn fixture_host(source: &str, config: &str) -> ConfigHost {
    let files = BTreeMap::from([
        (CONFIG.to_owned(), config.to_owned()),
        (SOURCE.to_owned(), source.to_owned()),
    ]);
    ConfigHost(Arc::new(bundled::wrap_fs(vfstest::from_map(files, true))))
}

fn fixture_program(host: ConfigHost) -> &'static Program {
    let (config, errors) = get_parsed_command_line_of_config_file(CONFIG, None, None, &host, None);
    assert!(errors.is_empty(), "config read errors: {errors:?}");
    let config = config.expect("parsed config");
    assert!(
        config.errors.is_empty(),
        "config errors: {:?}",
        config.errors
    );
    let host = new_cached_fs_compiler_host(PACKAGE, host.0, &bundled::lib_path(), None, None, None);
    let mut options = ProgramOptions::new(P::new(config), host);
    options.checker_regions = true;
    options.checker_diagnostic_regions = true;
    options.regions = Some(Arc::default());
    new_program(options)
}

fn with_program(
    host: ConfigHost,
    collect: impl FnOnce(&'static Program) -> Vec<SarifFinding>,
) -> Vec<SarifFinding> {
    let program = fixture_program(host);
    let result = catch_unwind(AssertUnwindSafe(|| collect(program)));
    // SAFETY: the callback returns only owned findings. No diagnostic, source
    // pointer or checker escapes, including when the callback panics.
    unsafe { free_unshared_program(program) };
    match result {
        Ok(findings) => findings,
        Err(panic) => resume_unwind(panic),
    }
}

fn compile_findings(host: ConfigHost) -> Vec<SarifFinding> {
    with_program(host, |program| {
        let diagnostics = get_diagnostics_of_any_program(
            &Context::default(),
            program,
            None,
            false,
            &mut |ctx, file| program.get_bind_diagnostics(ctx, file),
            &mut |ctx, file| program.get_semantic_diagnostics(ctx, file),
        );
        convert_diagnostics(REPO, &diagnostics)
    })
}

fn report(findings: &[SarifFinding]) -> Value {
    serde_json::from_str(&build_sarif(findings).expect("SARIF")).expect("JSON")
}

// Go TestCompilePackageTypeError, including the #260 repo-relative URI check.
#[test]
fn compile_package_type_error_sarif() {
    luchta_test_support::require_nextest();
    let findings = compile_findings(fixture_host(
        "// heading\r\nexport const answer: number = \"not a number\";\r\n",
        STRICT_CONFIG,
    ));
    // This report is deliberately built after free_unshared_program.
    let json = report(&findings);
    assert_eq!(json["version"], "2.1.0");
    assert_eq!(
        json["$schema"],
        "https://json.schemastore.org/sarif-2.1.0.json"
    );
    assert_eq!(json["runs"][0]["results"].as_array().unwrap().len(), 1);
    let result = &json["runs"][0]["results"][0];
    assert_eq!(result["ruleId"], "TS2322");
    assert_eq!(result["level"], "error");
    assert!(!result["message"]["text"].as_str().unwrap().is_empty());
    assert_eq!(
        result["locations"],
        json!([{
            "physicalLocation": {
                "artifactLocation": {"uri": "packages/pkg/src/index.ts"},
                "region": {"startLine": 2, "startColumn": 14, "endLine": 2, "endColumn": 20}
            }
        }])
    );
}

#[test]
fn sarif_real_diagnostic_uses_utf16_columns() {
    luchta_test_support::require_nextest();
    let findings = compile_findings(fixture_host(
        "\r\n/* 🦀 */ export const answer: number = \"not a number\";\r\n",
        STRICT_CONFIG,
    ));
    assert_eq!(findings.len(), 1);
    let json = report(&findings);
    let region = &json["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"];
    assert_eq!(
        region,
        &json!({"startLine": 2, "startColumn": 23, "endLine": 2, "endColumn": 29})
    );
}

#[test]
fn sarif_real_message_chain_is_indented() {
    luchta_test_support::require_nextest();
    let findings = compile_findings(fixture_host(
        "interface Outer { nested: { value: number } }\n\
     const actual = { nested: { value: 'bad' } };\n\
     const expected: Outer = actual;\n",
        STRICT_CONFIG,
    ));
    assert_eq!(findings.len(), 1);
    let message = &findings[0].message;
    assert!(message.contains("\n  "), "first chain level: {message}");
    assert!(message.contains("\n    "), "second chain level: {message}");
}

#[test]
fn sarif_message_chain_flattens_arguments_and_siblings_but_not_related_info() {
    luchta_test_support::require_nextest();
    let leaf = new_compiler_diagnostic(new_ad_hoc_message("Leaf {0}"), &[&"argument"]);
    let child = new_compiler_diagnostic(new_ad_hoc_message("Child"), &[]).add_message_chain(leaf);
    let sibling = new_compiler_diagnostic(new_ad_hoc_message("Sibling"), &[]);
    let related = new_compiler_diagnostic(new_ad_hoc_message("Related location"), &[]);
    let diagnostic = new_compiler_diagnostic(new_ad_hoc_message("Root"), &[])
        .set_message_chain(&[child, sibling])
        .add_related_info(related);
    assert_eq!(diagnostic.related_information().len(), 1);
    let findings = convert_diagnostics(REPO, &[diagnostic]);
    let json = report(&findings);
    let result = &json["runs"][0]["results"][0];
    assert_eq!(
        result["message"]["text"],
        "Root\n  Child\n    Leaf argument\n  Sibling"
    );
    // Go's FlattenDiagnosticMessage uses MessageChain, not RelatedInformation.
    assert!(result.get("relatedLocations").is_none());
    assert!(!json.to_string().contains("Related location"));
}

#[test]
fn sarif_real_global_diagnostics_have_no_location() {
    luchta_test_support::require_nextest();
    let findings = compile_findings(fixture_host(
        "export const answer = 42;",
        r#"{"compilerOptions":{"noLib":true,"noEmit":true},"files":["src/index.ts"]}"#,
    ));
    assert!(!findings.is_empty());
    let json = report(&findings);
    for result in json["runs"][0]["results"].as_array().unwrap() {
        assert_eq!(result["ruleId"], "TS2318");
        assert!(result.get("locations").is_none());
        assert_eq!(result["level"], "error");
    }
}

#[test]
fn sarif_config_diagnostics_keep_the_config_location() {
    luchta_test_support::require_nextest();
    let host = fixture_host(
        "export {};",
        r#"{"compilerOptions":{"strict":"yes"},"files":["src/index.ts"]}"#,
    );
    let (config, errors) = get_parsed_command_line_of_config_file(CONFIG, None, None, &host, None);
    assert!(errors.is_empty());
    let config = config.expect("config");
    assert!(!config.errors.is_empty());
    let json = report(&convert_diagnostics(REPO, &config.errors));
    let result = &json["runs"][0]["results"][0];
    assert_eq!(result["ruleId"], "TS5024");
    assert_eq!(
        result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
        "packages/pkg/tsconfig.json"
    );
}

#[test]
fn sarif_categories_and_code_zero_match_go() {
    luchta_test_support::require_nextest();
    for (category, level) in [
        (Category::Error, "error"),
        (Category::Warning, "warning"),
        (Category::Suggestion, "note"),
        (Category::Message, "note"),
    ] {
        let diagnostic = new_external_diagnostic(
            None,
            TextRange::synthetic(),
            "",
            category,
            0,
            "Global diagnostic",
        );
        let json = report(&convert_diagnostics(REPO, &[diagnostic]));
        let result = &json["runs"][0]["results"][0];
        assert_eq!(result["level"], level);
        assert_eq!(result["message"]["text"], "Global diagnostic");
        assert!(result.get("ruleId").is_none());
        assert!(result.get("locations").is_none());
    }
}

#[test]
fn sarif_multiline_and_zero_width_spans() {
    luchta_test_support::require_nextest();
    let findings = with_program(
        fixture_host("// 🦀\nexport {};\n", STRICT_CONFIG),
        |program| {
            let file = program.get_source_file(SOURCE).expect("source file");
            let multiline = new_external_diagnostic(
                Some(file),
                TextRange::new(3, 14),
                "",
                Category::Error,
                1,
                "Multiline",
            );
            let eof = file.text().len() as u32;
            let zero_width = new_external_diagnostic(
                Some(file),
                TextRange::new(eof, eof),
                "",
                Category::Warning,
                2,
                "End of file",
            );
            convert_diagnostics(REPO, &[multiline, zero_width])
        },
    );
    let json = report(&findings);
    let results = json["runs"][0]["results"].as_array().unwrap();
    assert_eq!(
        results[0]["locations"][0]["physicalLocation"]["region"],
        json!({"startLine": 1, "startColumn": 4, "endLine": 2, "endColumn": 7})
    );
    assert_eq!(
        results[1]["locations"][0]["physicalLocation"]["region"],
        json!({"startLine": 3, "startColumn": 1, "endLine": 3, "endColumn": 1})
    );
}

#[test]
fn sarif_empty_report_has_go_metadata_and_no_results() {
    let json = report(&[]);
    assert_eq!(REPORT_FILENAME, "tsc.sarif");
    assert_eq!(MIME_TYPE, "application/sarif+json");
    assert_eq!(json["runs"][0]["results"], json!([]));
    assert_eq!(
        json["runs"][0]["tool"]["driver"],
        json!({
            "name": "luchta-tsrs-worker",
            "informationUri": "https://github.com/dobesv/luchta"
        })
    );
}

// Go TestRepoRelativeSARIFURI, all four table entries.
#[test]
fn repo_relative_sarif_uri_go_cases() {
    let base = tempfile::tempdir().unwrap();
    let repo = base.path().join("repo");
    let outside = base.path().join("other/src/foo.ts");
    let repo = repo.to_str().unwrap();
    let cases = [
        (
            format!("{repo}/packages/app/src/foo.ts"),
            "packages/app/src/foo.ts".to_owned(),
        ),
        (
            outside.to_str().unwrap().to_owned(),
            outside.to_str().unwrap().replace('\\', "/"),
        ),
        (repo.to_owned(), ".".to_owned()),
        (
            format!("{repo}/..config/foo.ts"),
            "..config/foo.ts".to_owned(),
        ),
    ];
    for (path, want) in cases {
        assert_eq!(repo_relative_sarif_uri(repo, &path), want, "{path}");
    }
}

#[test]
fn repo_relative_sarif_uri_normalizes_without_resolving_symlinks() {
    for (root, file, want) in [
        ("/repo/", "/repo/./packages/../src/foo.ts", "src/foo.ts"),
        ("/repo", "/repo-other/src/foo.ts", "/repo-other/src/foo.ts"),
        ("/repo", "/repo/../foo.ts", "/repo/../foo.ts"),
        ("/repo", "/", "/"),
        ("/repo", "relative.ts", "relative.ts"),
        ("/", "/src/foo.ts", "src/foo.ts"),
    ] {
        assert_eq!(
            repo_relative_sarif_uri(root, file),
            want,
            "{root} -> {file}"
        );
    }
}

#[test]
fn repo_relative_sarif_uri_windows_paths() {
    for (root, file, want) in [
        (r"C:\repo", r"C:\repo\src\foo.ts", "src/foo.ts"),
        (r"C:\repo", r"D:\repo\src\foo.ts", "D:/repo/src/foo.ts"),
        (r"C:\repo", r"C:\other\foo.ts", "C:/other/foo.ts"),
        (
            r"\\server\share\repo",
            r"\\server\share\repo\src\foo.ts",
            "src/foo.ts",
        ),
    ] {
        assert_eq!(
            repo_relative_sarif_uri(root, file),
            want,
            "{root} -> {file}"
        );
    }
}
