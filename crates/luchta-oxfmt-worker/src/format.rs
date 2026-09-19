#![cfg(feature = "oxc")]

use std::{path::Path, sync::Arc};

use oxc_allocator::Allocator;
use oxc_formatter::{
    format_with_session, CssInJsTemplate, JsFormatOptions, QuoteStyle, TrailingCommas,
};
use oxc_formatter_core::{
    DispatchRequest, DispatchResponse, FormatDispatcher, FormatSession, InputKind, SessionServices,
};
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
    repo_root: &Path,
    source: &str,
    options: &JsFormatOptions,
) -> Result<FormatResult, String> {
    let allocator = Allocator::default();
    let css_options = css_format_options(options);
    let dispatcher: FormatDispatcher = Arc::new(
        move |session: &FormatSession<'_>, request: DispatchRequest<'_>| {
            // CSS-in-JS is the only embedded language this worker serves. Any
            // other request is a deliberate "do not format", and the JS
            // formatter keeps the template literal exactly as written.
            //
            // The pre-session API reached the same outcome for this worker when
            // the closure answered `Err`, though not by one uniform rule: most
            // embed sites bailed out on `Err`, while html-in-js instead fell
            // through to a string-based fallback. That fallback is inert here
            // only because it starts by calling the string-embedding callback,
            // which this worker never installed (it set a dispatcher and nothing
            // else) — so it bailed out in turn.
            if !matches!(request.language, "css" | "scss" | "less") {
                return Ok(DispatchResponse::PreserveOriginal);
            }
            // `${...}` interpolations reach the child as `` `PLACEHOLDER-N` ``
            // markers, which parse only in the css-in-js mode. `oxc_formatter`'s
            // CSS embed sites are the only senders and always tag the request
            // with `CssInJsTemplate`, so this is the `allow_placeholders = true`
            // that the pre-session `format_to_ir` hard-coded.
            let template_placeholders = request
                .parent_context
                .is_some_and(|context| context.downcast_ref::<CssInJsTemplate>().is_some());
            match oxc_formatter_css::format_to_ir(
                session,
                request.text,
                css_options,
                template_placeholders,
            ) {
                Ok(embedded) => Ok(DispatchResponse::Formatted(embedded.into())),
                // `format_to_ir` currently errors only when child CSS will not parse or a
                // fragment contains front matter; both are deliberate preserve-as-is cases.
                // `DispatchResponse` reserves `Result::Err` for operational/transport failures
                // and says not to conflate them. Revisit this blanket match if `format_to_ir`
                // gains such an error.
                Err(_) => Ok(DispatchResponse::PreserveOriginal),
            }
        },
    );
    // Only the IR dispatcher is installed, matching the single
    // `ExternalCallbacks::with_dispatcher` of the pre-session API: no string
    // embedder (JSDoc fences stay as-is) and no Tailwind sorter (classes print
    // in source order).
    let services = SessionServices {
        dispatcher: Some(dispatcher),
        ..SessionServices::default()
    };
    // `PhysicalFile`: this worker formats files on disk, so the root owns the
    // file-level envelope (BOM) exactly as the pre-session entry point did.
    let session = FormatSession::with_services(&allocator, InputKind::PhysicalFile, services);
    let source_type = SourceType::from_path(path).map_err(|error| {
        format!(
            "failed to determine source type for {}: {error}",
            luchta_worker::paths::repo_relative(path, repo_root)
        )
    })?;
    let formatted: String = format_with_session(&session, source, source_type, options.clone())
        .map_err(|error| format_diagnostic(path, repo_root, &error.to_string()))?
        .print()
        .map_err(|error| format_diagnostic(path, repo_root, &error.to_string()))?
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

fn format_diagnostic(path: &Path, repo_root: &Path, message: &str) -> String {
    format!(
        "{}: {message}",
        luchta_worker::paths::repo_relative(path, repo_root)
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use oxc_formatter::{JsFormatOptions, SortImportsOptions};

    use super::{css_format_options, format_path};

    #[test]
    fn format_path_reformats_unformatted_ts() {
        let path = Path::new("src/example.ts");
        let result = format_path(
            path,
            Path::new(""),
            "export const value={foo:'bar'}\n",
            &JsFormatOptions::default(),
        )
        .expect("format ok");
        assert!(result.changed);
        assert_ne!(result.formatted, "export const value={foo:'bar'}\n");
    }

    #[test]
    fn format_path_matches_oxfmt_cli_for_multiline_arrow_interpolation() {
        let path = Path::new("src/example.tsx");
        let input = "const Button = styled.button`color:red;${({ theme }) => css`display:flex;align-items:center;justify-content:space-between;`};padding:8px;`;\n";
        let expected = "const Button = styled.button`\n  color: red;\n  ${({ theme }) =>\n    css`\n      display: flex;\n      align-items: center;\n      justify-content: space-between;\n    `}; padding: 8px;\n`;\n";

        let result = format_path(path, Path::new(""), input, &JsFormatOptions::default())
            .expect("format ok");

        assert_eq!(result.formatted, expected);
    }

    #[test]
    fn format_path_matches_oxfmt_cli_for_binary_expression_interpolation() {
        let path = Path::new("src/example.tsx");
        let input = "const Card = styled.div`${foo+bar+baz?'display:grid;grid-template-columns:1fr auto;':'display:block;'}\nmargin:0 auto;`;\n";
        let expected = "const Card = styled.div`\n  ${foo + bar + baz ? \"display:grid;grid-template-columns:1fr auto;\" : \"display:block;\"}\n  margin: 0 auto;\n`;\n";

        let result = format_path(path, Path::new(""), input, &JsFormatOptions::default())
            .expect("format ok");

        assert_eq!(result.formatted, expected);
    }

    #[test]
    fn format_path_sorts_imports_when_enabled() {
        let path = Path::new("src/example.ts");
        let input = "import z from 'z';\nimport a from 'a';\n\nexport { z, a };\n";
        let expected = "import a from \"a\";\nimport z from \"z\";\n\nexport { z, a };\n";

        let options = JsFormatOptions {
            sort_imports: Some(SortImportsOptions::default()),
            ..JsFormatOptions::default()
        };

        let result = format_path(path, Path::new(""), input, &options).expect("format ok");

        assert_eq!(result.formatted, expected);
        assert!(result.changed);
    }

    #[test]
    fn format_path_leaves_unsupported_embedded_languages_untouched() {
        // The dispatcher serves CSS only. A `gql` tagged template dispatches
        // the "graphql" language, which this worker declines; the JS formatter
        // must then leave the template's contents byte-for-byte alone, exactly
        // as it did when declining meant answering `Err`.
        let path = Path::new("src/example.ts");
        let input = "const q=gql`query   Foo{  id }`;\n";
        let expected = "const q = gql`query   Foo{  id }`;\n";

        let result = format_path(path, Path::new(""), input, &JsFormatOptions::default())
            .expect("format ok");

        assert_eq!(result.formatted, expected);
    }

    #[test]
    fn css_options_map_js_options_for_embedded_css() {
        let css_options = css_format_options(&JsFormatOptions::default());
        assert_eq!(css_options.variant, oxc_formatter_css::CssVariant::Scss);
        assert!(!css_options.sort_tailwindcss);
    }

    #[test]
    fn format_path_propagates_non_default_options_into_embedded_css() {
        let path = Path::new("src/example.tsx");
        let input = "const Box = styled.div`color:red;background:url(\"x.png\");${foo}`;\n";
        let expected =
            "const Box = styled.div`\n\tcolor: red;\n\tbackground: url('x.png');\n\t${foo}\n`;\n";

        let options = JsFormatOptions {
            indent_style: oxc_formatter_core::IndentStyle::Tab,
            quote_style: oxc_formatter::QuoteStyle::Single,
            ..JsFormatOptions::default()
        };

        let result = format_path(path, Path::new(""), input, &options).expect("format ok");

        assert_eq!(result.formatted, expected);
    }
}
