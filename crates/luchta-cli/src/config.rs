//! Configuration loader for `luchta.toml`.

use std::{fs, path::Path};

use miette::{Context, IntoDiagnostic, Result};

pub use luchta_types::LuchtaConfig;

/// Load `luchta.toml` configuration from the given path.
pub fn load_config(path: impl AsRef<Path>) -> Result<LuchtaConfig> {
    let path = path.as_ref();
    let contents = fs::read_to_string(path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read config at {}", path.display()))?;

    toml::from_str(&contents)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to parse config at {}", path.display()))
}
