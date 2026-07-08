#![cfg(feature = "oxc")]

use serde::Serialize;

use crate::lint::WrappedDiagnostic;

const SARIF_VERSION: &str = "2.1.0";
const SARIF_SCHEMA: &str = "https://json.schemastore.org/sarif-2.1.0.json";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifLog<'a> {
    version: &'a str,
    #[serde(rename = "$schema")]
    schema: &'a str,
    runs: Vec<SarifRun<'a>>,
}

#[derive(Serialize)]
struct SarifRun<'a> {
    tool: SarifTool<'a>,
    results: Vec<SarifResult>,
}

#[derive(Serialize)]
struct SarifTool<'a> {
    driver: SarifDriver<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifDriver<'a> {
    name: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifResult {
    rule_id: String,
    level: &'static str,
    message: SarifMessage,
    locations: Vec<SarifLocation>,
}

#[derive(Serialize)]
struct SarifMessage {
    text: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifLocation {
    physical_location: SarifPhysicalLocation,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifPhysicalLocation {
    artifact_location: SarifArtifactLocation,
    region: SarifRegion,
}

#[derive(Serialize)]
struct SarifArtifactLocation {
    uri: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SarifRegion {
    start_line: usize,
    start_column: usize,
}

pub fn build_sarif(findings: &[WrappedDiagnostic]) -> Result<String, String> {
    let results = findings
        .iter()
        .map(|finding| SarifResult {
            rule_id: finding
                .rule_id
                .clone()
                .unwrap_or_else(|| "oxlint-diagnostic".to_owned()),
            level: map_level(&finding.severity),
            message: SarifMessage {
                text: finding.message.clone(),
            },
            locations: vec![SarifLocation {
                physical_location: SarifPhysicalLocation {
                    artifact_location: SarifArtifactLocation {
                        uri: finding.relative_uri.clone(),
                    },
                    region: SarifRegion {
                        start_line: finding.start_line,
                        start_column: finding.start_column,
                    },
                },
            }],
        })
        .collect();

    serde_json::to_string_pretty(&SarifLog {
        version: SARIF_VERSION,
        schema: SARIF_SCHEMA,
        runs: vec![SarifRun {
            tool: SarifTool {
                driver: SarifDriver { name: "oxlint" },
            },
            results,
        }],
    })
    .map_err(|error| format!("failed to serialize SARIF: {error}"))
}

fn map_level(severity: &oxc_diagnostics::Severity) -> &'static str {
    match severity {
        oxc_diagnostics::Severity::Error => "error",
        oxc_diagnostics::Severity::Warning => "warning",
        oxc_diagnostics::Severity::Advice => "note",
    }
}
