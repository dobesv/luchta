#![cfg(feature = "oxc")]

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use oxc_allocator::Allocator;
use oxc_formatter::{format, ExternalCallbacks, JsFormatOptions, QuoteStyle, TrailingCommas};
use oxc_formatter_core::{DispatchResult, FormatDispatcher};
use oxc_formatter_css::{
    CssFormatOptions, CssVariant, SingleQuote, TrailingCommas as CssTrailingCommas,
};
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
    let css_options = css_format_options(options);
    let dispatcher: FormatDispatcher = Arc::new(move |ctx, language, texts, _parent| {
        let css_options = match language {
            "css" | "scss" | "less" => css_options,
            _ => return Err(format!("unsupported embedded language: {language}")),
        };
        let [text] = texts else {
            return Err(format!(
                "expected exactly 1 embedded text for {language}, got {}",
                texts.len()
            ));
        };
        let embedded = oxc_formatter_css::format_to_ir(ctx, text, css_options)
            .map_err(|error| error.to_string())?;
        Ok(DispatchResult {
            docs: vec![embedded.ir],
            tailwind_classes: embedded.tailwind_classes,
            meta: None,
        })
    });
    let callbacks = ExternalCallbacks::new().with_dispatcher(Some(dispatcher));
    let formatted: String = format(
        &allocator,
        source,
        source_type,
        options.clone(),
        Some(callbacks),
    )
    .map_err(|error| format_diagnostic(path, &error.to_string()))?
    .print()
    .map_err(|error| format_diagnostic(path, &error.to_string()))?
    .into_code();

    Ok(FormatResult {
        changed: formatted.as_bytes() != source.as_bytes(),
        formatted,
    })
}

fn css_format_options(options: &JsFormatOptions) -> CssFormatOptions {
    CssFormatOptions {
        indent_style: options.indent_style,
        indent_width: options.indent_width,
        line_width: options.line_width,
        line_ending: options.line_ending,
        variant: CssVariant::Scss,
        single_quote: SingleQuote::from(options.quote_style == QuoteStyle::Single),
        trailing_commas: match options.trailing_commas {
            TrailingCommas::All | TrailingCommas::Es5 => CssTrailingCommas::Always,
            TrailingCommas::None => CssTrailingCommas::Never,
        },
        sort_tailwindcss: false,
    }
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

    use super::{css_format_options, format_path, normalize_path, relative_display};

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
    fn format_path_matches_oxfmt_cli_for_multiline_arrow_interpolation() {
        let path = Path::new("src/example.tsx");
        let input = "const Button = styled.button`color:red;${({ theme }) => css`display:flex;align-items:center;justify-content:space-between;`};padding:8px;`;\n";
        // Golden verified byte-for-byte against the authoritative `oxfmt` 0.58.0 CLI
        // (installed from npm: `oxfmt` + `@oxfmt/binding-linux-x64-gnu`, run on this exact
        // input). The worker output equals the CLI output.
        let expected = "const Button = styled.button`\n  color: red;\n  ${({ theme }) =>\n    css`\n      display: flex;\n      align-items: center;\n      justify-content: space-between;\n    `}; padding: 8px;\n`;\n";

        let result = format_path(path, input, &JsFormatOptions::new()).expect("format ok");

        assert_eq!(result.formatted, expected);
    }

    #[test]
    fn format_path_matches_oxfmt_cli_for_binary_expression_interpolation() {
        let path = Path::new("src/example.tsx");
        let input = "const Card = styled.div`${foo+bar+baz?'display:grid;grid-template-columns:1fr auto;':'display:block;'}\nmargin:0 auto;`;\n";
        // Golden verified byte-for-byte against the authoritative `oxfmt` 0.58.0 CLI
        // (installed from npm: `oxfmt` + `@oxfmt/binding-linux-x64-gnu`, run on this exact
        // input). The worker output equals the CLI output.
        let expected = "const Card = styled.div`\n  ${foo + bar + baz ? \"display:grid;grid-template-columns:1fr auto;\" : \"display:block;\"}\n  margin: 0 auto;\n`;\n";

        let result = format_path(path, input, &JsFormatOptions::new()).expect("format ok");

        assert_eq!(result.formatted, expected);
    }

    #[test]
    fn css_options_map_js_options_for_embedded_css() {
        let css_options = css_format_options(&JsFormatOptions::new());
        assert_eq!(css_options.variant, oxc_formatter_css::CssVariant::Scss);
        assert!(!css_options.sort_tailwindcss);
    }

    #[test]
    fn format_path_propagates_non_default_options_into_embedded_css() {
        // Golden verified byte-for-byte against the authoritative `oxfmt` 0.58.0 CLI
        // run with `{ "useTabs": true, "singleQuote": true }` on this exact input.
        // Confirms indent_style (tabs) and single_quote propagate into the embedded
        // CSS: property lines are tab-indented and `url("x.png")` becomes `url('x.png')`.
        let path = Path::new("src/example.tsx");
        let input = "const Box = styled.div`color:red;background:url(\"x.png\");${foo}`;\n";
        let expected =
            "const Box = styled.div`\n\tcolor: red;\n\tbackground: url('x.png');\n\t${foo}\n`;\n";

        let mut options = JsFormatOptions::new();
        options.indent_style = oxc_formatter_core::IndentStyle::Tab;
        options.quote_style = oxc_formatter::QuoteStyle::Single;

        let result = format_path(path, input, &options).expect("format ok");

        assert_eq!(result.formatted, expected);
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
