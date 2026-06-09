//! `luchta.toml` configuration model and loader.
//!
//! These types are wired into the `run` command end-to-end in Phase 2
//! (Task 2.7). They are defined here so the schema is stable for downstream
//! crates; `#![allow(dead_code)]` suppresses unused-warnings until then.
#![allow(dead_code)]

use std::{collections::HashMap, fs, path::Path};

use miette::{Context, IntoDiagnostic, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct LuchtaConfig {
    pub pipeline: HashMap<String, luchta_types::TaskDefinition>,
    pub concurrency: ConcurrencyConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConcurrencyConfig {
    pub max_weight: u32,
}

pub fn load_config(path: impl AsRef<Path>) -> Result<LuchtaConfig> {
    let path = path.as_ref();
    let contents = fs::read_to_string(path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read config at {}", path.display()))?;

    toml::from_str(&contents)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to parse config at {}", path.display()))
}
