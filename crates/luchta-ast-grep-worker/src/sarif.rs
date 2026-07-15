use luchta_worker::sarif::{build_sarif as build_shared_sarif, SarifFinding, SarifLevel};

use crate::lint::Finding;

pub fn build_sarif(findings: &[Finding]) -> Result<String, String> {
    let entries: Vec<SarifFinding> = findings
        .iter()
        .map(|finding| SarifFinding {
            rule_id: if finding.rule_id.is_empty() {
                "ast-grep-rule".to_owned()
            } else {
                finding.rule_id.clone()
            },
            level: map_level(&finding.severity),
            message: finding.message.clone(),
            uri: finding.relative_uri.clone(),
            start_line: finding.start_line,
            start_column: finding.start_column,
            end_line: Some(finding.end_line),
            end_column: Some(finding.end_column),
        })
        .collect();
    build_shared_sarif("ast-grep", &entries)
}

fn map_level(sev: &ast_grep_config::Severity) -> SarifLevel {
    match sev {
        ast_grep_config::Severity::Error => SarifLevel::Error,
        ast_grep_config::Severity::Warning => SarifLevel::Warning,
        ast_grep_config::Severity::Info => SarifLevel::Note,
        ast_grep_config::Severity::Hint => SarifLevel::Note,
        ast_grep_config::Severity::Off => SarifLevel::None,
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
            relative_uri: "packages/app/src/index.ts".to_owned(),
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
            "packages/app/src/index.ts"
        );
        assert_eq!(
            result["locations"][0]["physicalLocation"]["region"]["endColumn"],
            12
        );
    }
}
