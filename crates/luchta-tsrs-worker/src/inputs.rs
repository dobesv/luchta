//! Static config inputs for cache invalidation; no typecheck or emit.

use std::collections::BTreeSet;
use std::panic::catch_unwind;
use std::path::Path;

use luchta_worker::{ResolveResult, ResolveTask, TaskModification};
use tsrs_core::arena::Region;
use tsrs_tsoptions::{
    get_parsed_command_line_of_config_file, CompilerOptionsValue, ParseConfigHost,
    ParsedCommandLine,
};

use crate::pnp::CompilerSystem;

pub const TSCONFIG_CANDIDATES: [&str; 2] = ["tsconfig.build.json", "tsconfig.json"];
const DEFAULT_INCLUDE: &str = "src/**";
// Empty include globs still need hashing: future matching files must bust cache.
const NO_INPUTS_FOUND: i32 = 18003;

pub fn resolve_task(req: &ResolveTask) -> ResolveResult {
    let Some(cwd) = req.cwd.as_deref() else {
        return ResolveResult::accept();
    };
    match catch_unwind(|| resolve_inputs(Path::new(cwd))) {
        Ok(Ok(inputs)) => ResolveResult::modify(TaskModification {
            inputs: Some(merge_inputs(&req.inputs, inputs)),
            tool_version: Some(env!("LUCHTA_TOOL_VERSION").to_owned()),
            ..TaskModification::default()
        }),
        Ok(Err(error)) => {
            eprintln!("luchta-tsrs-worker: resolve inputs: {error}");
            ResolveResult::accept()
        }
        Err(_) => {
            eprintln!("luchta-tsrs-worker: resolve inputs panicked");
            ResolveResult::accept()
        }
    }
}

/// Report absent candidates too: adding either config must invalidate cache.
pub fn resolve_inputs(cwd: &Path) -> Result<Vec<String>, String> {
    // Config ASTs must not accumulate in reused blocking threads' default arenas.
    // Only owned input strings (or an owned error) leave this scope.
    let region = Region::new(1 << 20);
    let _scope = region.enter();
    let system = CompilerSystem::new(cwd)?;
    let mut inputs = BTreeSet::from(TSCONFIG_CANDIDATES.map(str::to_owned));
    let mut found_config = false;
    for name in TSCONFIG_CANDIDATES {
        let config_path = format!("{}/{name}", system.get_current_directory());
        if !system.fs().file_exists(&config_path) {
            continue;
        }
        found_config = true;
        let parsed = parse_config(&system, &config_path)?;
        inputs.extend(include_patterns(&parsed));
        inputs.extend(local_extended_configs(&system, &parsed));
    }
    if !found_config {
        inputs.insert(DEFAULT_INCLUDE.to_owned());
    }
    Ok(inputs.into_iter().collect())
}

fn parse_config(system: &CompilerSystem, path: &str) -> Result<ParsedCommandLine, String> {
    let (parsed, mut errors) =
        get_parsed_command_line_of_config_file(path, None, None, system, None);
    if let Some(parsed) = &parsed {
        errors.extend(parsed.get_config_file_parsing_diagnostics());
    }
    let error = errors
        .iter()
        .find(|error| error.code() != NO_INPUTS_FOUND && error.category().name() == "error");
    if let Some(error) = error {
        return Err(format!(
            "parse {path}: TS{}: {} {:?}",
            error.code(),
            error.message_text(),
            error.message_args()
        ));
    }
    parsed.ok_or_else(|| format!("parse {path}: no parsed command line returned"))
}

fn include_patterns(parsed: &ParsedCommandLine) -> Vec<String> {
    let patterns: Vec<_> = parsed
        .raw
        .as_object()
        .and_then(|raw| raw.get("include"))
        .and_then(CompilerOptionsValue::as_array)
        .into_iter()
        .flatten()
        .filter_map(CompilerOptionsValue::as_str)
        .map(str::to_owned)
        .collect();
    if patterns.is_empty() {
        vec![DEFAULT_INCLUDE.to_owned()]
    } else {
        patterns
    }
}

fn local_extended_configs(system: &CompilerSystem, parsed: &ParsedCommandLine) -> Vec<String> {
    // Other packages' configs are covered by the package-dependency hash.
    parsed
        .extended_source_files()
        .into_iter()
        .filter_map(|path| {
            Path::new(&path)
                .strip_prefix(system.get_current_directory())
                .ok()
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        })
        .filter(|path| !path.is_empty() && !path.starts_with(".."))
        .collect()
}

fn merge_inputs(declared: &[String], worker: Vec<String>) -> Vec<String> {
    let mut inputs: BTreeSet<_> = worker.into_iter().collect();
    inputs.extend(declared.iter().cloned());
    inputs.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::merge_inputs;

    // Go: TestMergeInputs (all five table cases).
    #[test]
    fn resolve_merge_inputs() {
        let cases = [
            (vec![], vec![], vec![]),
            (vec!["a.txt", "b.txt"], vec![], vec!["a.txt", "b.txt"]),
            (vec![], vec!["x/**", "y/**"], vec!["x/**", "y/**"]),
            (
                vec!["a.txt", "shared/**"],
                vec!["shared/**", "b.txt"],
                vec!["a.txt", "b.txt", "shared/**"],
            ),
            (
                vec!["a", "b", "c"],
                vec!["a", "b", "c"],
                vec!["a", "b", "c"],
            ),
        ];
        for (declared, worker, expected) in cases {
            let declared: Vec<_> = declared.into_iter().map(str::to_owned).collect();
            let worker = worker.into_iter().map(str::to_owned).collect();
            assert_eq!(merge_inputs(&declared, worker), expected);
        }
    }
}
