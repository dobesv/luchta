use serde::Serialize;

const SARIF_VERSION: &str = "2.1.0";
const SARIF_SCHEMA: &str = "https://json.schemastore.org/sarif-2.1.0.json";

/// SARIF result severity levels used by luchta's diagnostic workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SarifLevel {
    Error,
    Warning,
    Note,
    None,
}

impl SarifLevel {
    fn as_str(self) -> &'static str {
        match self {
            SarifLevel::Error => "error",
            SarifLevel::Warning => "warning",
            SarifLevel::Note => "note",
            SarifLevel::None => "none",
        }
    }
}

/// One diagnostic to render as a SARIF result. `end_line`/`end_column` are
/// optional; when `None` they are omitted from the region object.
#[derive(Debug, Clone)]
pub struct SarifFinding {
    pub rule_id: String,
    pub level: SarifLevel,
    pub message: String,
    pub uri: String,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: Option<usize>,
    pub end_column: Option<usize>,
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_column: Option<usize>,
}

/// Serialize `findings` as a SARIF 2.1.0 log with a single run whose driver
/// name is `tool_name`.
pub fn build_sarif(tool_name: &str, findings: &[SarifFinding]) -> Result<String, String> {
    let results = findings
        .iter()
        .map(|finding| SarifResult {
            rule_id: finding.rule_id.clone(),
            level: finding.level.as_str(),
            message: SarifMessage {
                text: finding.message.clone(),
            },
            locations: vec![SarifLocation {
                physical_location: SarifPhysicalLocation {
                    artifact_location: SarifArtifactLocation {
                        uri: finding.uri.clone(),
                    },
                    region: SarifRegion {
                        start_line: finding.start_line,
                        start_column: finding.start_column,
                        end_line: finding.end_line,
                        end_column: finding.end_column,
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
                driver: SarifDriver { name: tool_name },
            },
            results,
        }],
    })
    .map_err(|error| format!("failed to serialize SARIF: {error}"))
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::{build_sarif, SarifFinding, SarifLevel};

    fn finding(end: Option<(usize, usize)>) -> SarifFinding {
        SarifFinding {
            rule_id: "no-console".to_owned(),
            level: SarifLevel::Error,
            message: "no console".to_owned(),
            uri: "packages/app/src/index.ts".to_owned(),
            start_line: 1,
            start_column: 1,
            end_line: end.map(|(line, _)| line),
            end_column: end.map(|(_, col)| col),
        }
    }

    #[test]
    fn empty_findings_produce_valid_sarif_with_driver_name() {
        let sarif = build_sarif("oxlint", &[]).expect("sarif");
        let json: Value = serde_json::from_str(&sarif).expect("json");
        assert_eq!(json["version"], "2.1.0");
        assert_eq!(json["runs"][0]["tool"]["driver"]["name"], "oxlint");
        assert_eq!(json["runs"][0]["results"], Value::Array(vec![]));
    }

    #[test]
    fn finding_uri_and_level_are_rendered() {
        let sarif = build_sarif("ast-grep", &[finding(Some((1, 12)))]).expect("sarif");
        let json: Value = serde_json::from_str(&sarif).expect("json");
        let result = &json["runs"][0]["results"][0];
        assert_eq!(result["ruleId"], "no-console");
        assert_eq!(result["level"], "error");
        assert_eq!(
            result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            "packages/app/src/index.ts"
        );
        let region = &result["locations"][0]["physicalLocation"]["region"];
        assert_eq!(region["endLine"], 1);
        assert_eq!(region["endColumn"], 12);
    }

    #[test]
    fn end_positions_are_omitted_when_absent() {
        let sarif = build_sarif("oxlint", &[finding(None)]).expect("sarif");
        let json: Value = serde_json::from_str(&sarif).expect("json");
        let region = &json["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"];
        assert!(region.get("endLine").is_none());
        assert!(region.get("endColumn").is_none());
        assert_eq!(region["startLine"], 1);
    }
}
