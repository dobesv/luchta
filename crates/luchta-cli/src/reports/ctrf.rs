use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Ctrf {
    pub results: CtrfResults,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CtrfResults {
    pub summary: CtrfSummary,
    pub tests: Vec<CtrfTest>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CtrfSummary {
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CtrfTest {
    pub name: String,
    pub status: String,
    pub message: Option<String>,
    pub trace: Option<String>,
}
