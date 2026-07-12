use serde::Serialize;

use crate::lint::Finding;

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
    end_line: usize,
    end_column: usize,
}

pub fn build_sarif(findings: &[Finding]) -> Result<String, String> {
    let results = findings
        .iter()
        .map(|finding| SarifResult {
            rule_id: if finding.rule_id.is_empty() {
                "ast-grep-rule".to_owned()
            } else {
                finding.rule_id.clone()
            },
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
                driver: SarifDriver { name: "ast-grep" },
            },
            results,
        }],
    })
    .map_err(|error| format!("failed to serialize SARIF: {error}"))
}

fn map_level(sev: &ast_grep_config::Severity) -> &'static str {
    match sev {
        ast_grep_config::Severity::Error => "error",
        ast_grep_config::Severity::Warning => "warning",
        ast_grep_config::Severity::Info => "note",
        ast_grep_config::Severity::Hint => "note",
        ast_grep_config::Severity::Off => "none",
    }
}

#[cfg(test)]
mod tests {
    use ast_grep_config::Severity;
    use serde_json::Value;

    use crate::lint::Finding;

    use super::build_sarif;

    #[test]
    fn empty_findings_produces_valid_sarif() {
        let sarif = build_sarif(&[]).expect("sarif");
        let json: Value = serde_json::from_str(&sarif).expect("json");
        assert_eq!(json["version"], "2.1.0");
        assert_eq!(json["runs"][0]["tool"]["driver"]["name"], "ast-grep");
        assert_eq!(json["runs"][0]["results"], Value::Array(vec![]));
    }

    #[test]
    fn single_finding_produces_expected_shape() {
        let sarif = build_sarif(&[Finding {
            rule_id: "no-console-log".to_owned(),
            severity: Severity::Error,
            message: "No console.log allowed".to_owned(),
            relative_uri: "src/index.ts".to_owned(),
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 12,
        }])
        .expect("sarif");
        let json: Value = serde_json::from_str(&sarif).expect("json");
        let result = &json["runs"][0]["results"][0];
        assert_eq!(result["ruleId"], "no-console-log");
        assert_eq!(result["level"], "error");
        assert_eq!(result["message"]["text"], "No console.log allowed");
        assert_eq!(
            result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            "src/index.ts"
        );
        assert_eq!(
            result["locations"][0]["physicalLocation"]["region"]["endColumn"],
            12
        );
    }
}
