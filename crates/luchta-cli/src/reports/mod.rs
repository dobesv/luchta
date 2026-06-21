//! Report MIME dispatch and typed parsers for native CLI renderers.
//!
//! To add a new MIME type: 1) add a variant/entry here keyed on exact MIME
//! string, 2) implement its renderer in `format.rs`, 3) document it in
//! README/AGENTS worker-protocol MIME list. Dispatch on MIME only, never
//! filename/extension.

pub mod ctrf;

use serde_json::Error as JsonError;

pub use ctrf::Ctrf;

const SARIF_MIME: &str = "application/sarif+json";
const CTRF_MIME: &str = "application/vnd.ctrf+json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    Sarif,
    Ctrf,
}

impl ReportKind {
    #[must_use]
    pub fn from_mime(mime: &str) -> Option<Self> {
        match mime {
            SARIF_MIME => Some(Self::Sarif),
            CTRF_MIME => Some(Self::Ctrf),
            _ => None,
        }
    }
}

#[must_use]
pub fn printer_for(mime: &str) -> Option<ReportKind> {
    ReportKind::from_mime(mime)
}

pub fn parse_sarif(bytes: &[u8]) -> Result<serde_sarif::sarif::Sarif, JsonError> {
    serde_json::from_slice(bytes)
}

pub fn parse_ctrf(bytes: &[u8]) -> Result<Ctrf, JsonError> {
    serde_json::from_slice(bytes)
}

#[cfg(test)]
mod tests {
    use super::{parse_ctrf, parse_sarif, printer_for, ReportKind};

    #[test]
    fn printer_dispatches_known_mimes() {
        assert_eq!(
            printer_for("application/sarif+json"),
            Some(ReportKind::Sarif)
        );
        assert_eq!(
            printer_for("application/vnd.ctrf+json"),
            Some(ReportKind::Ctrf)
        );
        assert_eq!(printer_for("text/plain"), None);
    }

    #[test]
    fn parse_sarif_fixture() {
        let sarif = parse_sarif(
            br#"{
                "version": "2.1.0",
                "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
                "runs": []
            }"#,
        )
        .expect("sarif fixture should parse");

        assert_eq!(sarif.version, "2.1.0");
        assert!(sarif.runs.is_empty());
    }

    #[test]
    fn parse_ctrf_fixture() {
        let ctrf = parse_ctrf(
            br#"{
                "results": {
                    "tool": { "name": "vitest" },
                    "summary": {
                        "tests": 3,
                        "passed": 2,
                        "failed": 1,
                        "pending": 0,
                        "skipped": 0,
                        "start": 1700000000,
                        "stop": 1700000010
                    },
                    "tests": [
                        {
                            "name": "adds numbers",
                            "status": "passed",
                            "duration": 12
                        },
                        {
                            "name": "subtracts numbers",
                            "status": "failed",
                            "message": "expected 2",
                            "trace": "AssertionError"
                        }
                    ]
                }
            }"#,
        )
        .expect("ctrf fixture should parse");

        assert_eq!(ctrf.results.tests.len(), 2);
        assert_eq!(ctrf.results.tests[1].message.as_deref(), Some("expected 2"));
        assert_eq!(
            ctrf.results.tests[1].trace.as_deref(),
            Some("AssertionError")
        );
    }
}
