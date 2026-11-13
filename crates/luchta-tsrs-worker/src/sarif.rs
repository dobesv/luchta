//! Convert compiler diagnostics while their program is alive; serialize owned
//! findings after freeing the program. Internal errors belong on stderr, not here.

use luchta_worker::sarif::{build_sarif_with_information_uri, SarifFinding, SarifLevel};
use tsrs_ast::{Diagnostic, SourceFile};
use tsrs_compiler::diagnosticwriter::write_flattened_diagnostic_message;
use tsrs_core::{position_to_line_and_byte_offset, tspath, utf16_len, P};
use tsrs_diagnostics::Category;

pub const REPORT_FILENAME: &str = "tsc.sarif";
pub const MIME_TYPE: &str = "application/sarif+json";

/// Copy diagnostics before freeing their program or diagnostic arena. Returned
/// findings contain no tsrs pointers and can be accumulated across configs.
pub fn convert_diagnostics(repo_root: &str, diagnostics: &[P<Diagnostic>]) -> Vec<SarifFinding> {
    diagnostics
        .iter()
        .map(|&diagnostic| convert_diagnostic(repo_root, diagnostic))
        .collect()
}

/// Serialize collected compiler diagnostics, never internal worker errors.
pub fn build_sarif(findings: &[SarifFinding]) -> Result<String, String> {
    build_sarif_with_information_uri(
        "luchta-tsrs-worker",
        Some("https://github.com/dobesv/luchta"),
        findings,
    )
}

fn convert_diagnostic(repo_root: &str, diagnostic: P<Diagnostic>) -> SarifFinding {
    let mut message = Vec::new();
    // Use tsc's own formatter: localized arguments, depth-first message chains,
    // two spaces per level. Go does not include separate related_information.
    write_flattened_diagnostic_message(&mut message, diagnostic, "\n");
    let mut finding = SarifFinding {
        rule_id: match diagnostic.code() {
            0 => None,
            code => Some(format!("TS{code}")),
        },
        level: map_level(diagnostic.category()),
        message: String::from_utf8(message).expect("tsrs diagnostic text is UTF-8"),
        uri: None,
        start_line: 1,
        start_column: 1,
        end_line: None,
        end_column: None,
    };
    if let Some(file) = diagnostic.file() {
        let start = diagnostic.pos().unwrap_or_default();
        let (start_line, start_column) = position(file, start);
        let (end_line, end_column) = position(file, diagnostic.end().unwrap_or(start));
        finding.uri = Some(repo_relative_sarif_uri(repo_root, file.file_name()));
        finding.start_line = start_line;
        finding.start_column = start_column;
        finding.end_line = Some(end_line);
        finding.end_column = Some(end_column);
    }
    finding
}

fn map_level(category: Category) -> SarifLevel {
    match category {
        Category::Error => SarifLevel::Error,
        Category::Warning => SarifLevel::Warning,
        Category::Suggestion | Category::Message => SarifLevel::Note,
    }
}

fn position(file: P<SourceFile>, offset: u32) -> (usize, usize) {
    let (line, byte_column) = position_to_line_and_byte_offset(offset, file.ecma_line_map());
    let line_start = (offset - byte_column) as usize;
    let column = utf16_len(&file.text()[line_start..offset as usize]);
    (line + 1, column as usize + 1)
}

fn repo_relative_sarif_uri(repo_root: &str, file_name: &str) -> String {
    let fallback = file_name.replace('\\', "/");
    let root = tspath::normalize_path(repo_root);
    let file = tspath::normalize_path(file_name);
    // tsrs requires matching absolute/relative roots; Go's filepath.Rel returns
    // an error for this case. Neither path is resolved through the filesystem.
    if tspath::is_rooted_disk_path(&root) != tspath::is_rooted_disk_path(&file) {
        return fallback;
    }
    let relative = tspath::get_relative_path_from_directory(
        &root,
        &file,
        &tspath::ComparePathsOptions {
            use_case_sensitive_file_names: !cfg!(windows),
            current_directory: String::new(),
        },
    );
    let escapes_repo = relative.split('/').next() == Some("..");
    if tspath::is_rooted_disk_path(&relative) || escapes_repo {
        fallback
    } else if relative.is_empty() {
        ".".to_owned()
    } else {
        relative
    }
}

#[cfg(test)]
mod tests;
