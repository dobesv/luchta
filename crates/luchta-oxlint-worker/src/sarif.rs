#![cfg(feature = "oxc")]

use luchta_worker::sarif::{build_sarif as build_shared_sarif, SarifFinding, SarifLevel};

use crate::lint::WrappedDiagnostic;

pub fn build_sarif(findings: &[WrappedDiagnostic]) -> Result<String, String> {
    let entries: Vec<SarifFinding> = findings
        .iter()
        .map(|finding| SarifFinding {
            rule_id: finding
                .rule_id
                .clone()
                .unwrap_or_else(|| "oxlint-diagnostic".to_owned()),
            level: map_level(&finding.severity),
            message: finding.message.clone(),
            uri: finding.relative_uri.clone(),
            start_line: finding.start_line,
            start_column: finding.start_column,
            end_line: None,
            end_column: None,
        })
        .collect();
    build_shared_sarif("oxlint", &entries)
}

fn map_level(severity: &oxc_diagnostics::Severity) -> SarifLevel {
    match severity {
        oxc_diagnostics::Severity::Error => SarifLevel::Error,
        oxc_diagnostics::Severity::Warning => SarifLevel::Warning,
        oxc_diagnostics::Severity::Advice => SarifLevel::Note,
    }
}
