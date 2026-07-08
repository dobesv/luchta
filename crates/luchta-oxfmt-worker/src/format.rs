#![cfg(feature = "oxc")]

use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_formatter::{format, JsFormatOptions};
use oxc_span::SourceType;

pub struct FormatResult {
    pub formatted: String,
    pub changed: bool,
}

pub fn format_path(
    path: &Path,
    source: &str,
    options: &JsFormatOptions,
) -> Result<FormatResult, String> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).map_err(|error| {
        format!(
            "failed to determine source type for {}: {error}",
            path.display()
        )
    })?;
    let formatted: String = format(&allocator, source, source_type, options.clone(), None)
        .map_err(|error| format_diagnostic(path, &error.to_string()))?
        .print()
        .map_err(|error| format_diagnostic(path, &error.to_string()))?
        .into_code();

    Ok(FormatResult {
        changed: formatted.as_bytes() != source.as_bytes(),
        formatted,
    })
}

fn format_diagnostic(path: &Path, message: &str) -> String {
    format!("{}: {message}", path.display())
}

pub fn relative_display(cwd: &Path, path: &Path) -> String {
    normalize_path(path.strip_prefix(cwd).unwrap_or(path))
}

pub fn normalize_path(path: &Path) -> String {
    let path_buf: PathBuf = path.iter().collect();
    path_buf.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use oxc_formatter::JsFormatOptions;

    use super::{format_path, normalize_path, relative_display};

    #[test]
    fn format_path_reformats_unformatted_ts() {
        let path = Path::new("src/example.ts");
        let result = format_path(
            path,
            "export const value={foo:'bar'}\n",
            &JsFormatOptions::new(),
        )
        .expect("format ok");
        assert!(result.changed);
        assert_ne!(result.formatted, "export const value={foo:'bar'}\n");
    }

    #[test]
    fn relative_display_normalizes_separators() {
        let cwd = Path::new("/repo");
        let file = Path::new("/repo/src/nested/file.ts");
        assert_eq!(relative_display(cwd, file), "src/nested/file.ts");
        assert_eq!(
            normalize_path(Path::new("src/nested/file.ts")),
            "src/nested/file.ts"
        );
    }
}
