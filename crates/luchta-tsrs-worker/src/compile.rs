//! One-shot compilation. No compiler pointers escape the blocking task.

use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Arc, Mutex};

use luchta_worker::sarif::SarifFinding;
use tsrs_compiler::{
    free_unshared_program, get_diagnostics_of_any_program, new_cached_fs_compiler_host,
    new_program, sort_and_deduplicate_diagnostics, Context, EmitOptions, Program, ProgramOptions,
    WriteFileData,
};
use tsrs_core::{arena::Region, P};
use tsrs_tsoptions::{get_parsed_command_line_of_config_file, ParseConfigHost};
use tsrs_vfs::bundled;

use crate::inputs::TSCONFIG_CANDIDATES;
use crate::outputs::{clean_outputs, resolve_outputs};
use crate::pnp::CompilerSystem;
use crate::sarif::convert_diagnostics;

/// Owned data for Worker wiring: findings become a report; stderr becomes logs.
#[derive(Debug, Default)]
pub struct CompileResult {
    pub exit_code: i32,
    pub outputs: Vec<String>,
    pub findings: Vec<SarifFinding>,
    pub stderr: Vec<String>,
}

impl CompileResult {
    fn failure(message: String) -> Self {
        Self {
            exit_code: 1,
            stderr: vec![message],
            ..Self::default()
        }
    }
}

/// Parse, clean, check and emit each existing candidate in Go worker order.
/// `repo_root` is the worker's launch directory, not the package directory.
pub async fn compile_package(cwd: &Path, repo_root: &Path) -> CompileResult {
    let cwd = cwd.to_path_buf();
    let repo_root = repo_root.to_path_buf();
    match tokio::task::spawn_blocking(move || {
        catch_compile(|| compile_package_blocking(&cwd, &repo_root))
    })
    .await
    {
        Ok(result) => result,
        Err(error) => CompileResult::failure(format!("compile task failed: {error}")),
    }
}

fn catch_compile(compile: impl FnOnce() -> CompileResult) -> CompileResult {
    match catch_unwind(AssertUnwindSafe(compile)) {
        Ok(result) => result,
        Err(panic) => CompileResult::failure(format!("panic: {}", panic_message(panic.as_ref()))),
    }
}

fn panic_message(panic: &(dyn Any + Send)) -> &str {
    if let Some(message) = panic.downcast_ref::<String>() {
        message
    } else {
        panic
            .downcast_ref::<&str>()
            .copied()
            .unwrap_or("unknown panic")
    }
}

fn compile_package_blocking(cwd: &Path, repo_root: &Path) -> CompileResult {
    let system = match CompilerSystem::new(cwd) {
        Ok(system) => system,
        Err(error) => return CompileResult::failure(error),
    };
    // CompilerSystem uses real paths for source/output names. Relativize against
    // the same spelling (macOS /var and /private/var, or a symlinked checkout).
    let cwd = Path::new(system.get_current_directory());
    let repo_root = system.fs().realpath(&repo_root.to_string_lossy());
    let mut result = CompileResult::default();
    for name in TSCONFIG_CANDIDATES {
        let path = format!("{}/{name}", system.get_current_directory());
        if !system.fs().file_exists(&path) {
            continue;
        }
        compile_config(&system, &path, Path::new(&repo_root), &mut result);
        if result.exit_code != 0 {
            break;
        }
    }
    result.outputs = resolve_outputs(cwd, &result.outputs);
    result
}

fn compile_config(
    system: &CompilerSystem,
    path: &str,
    repo_root: &Path,
    result: &mut CompileResult,
) {
    // Config parsing and the calling thread's compiler allocations also need
    // ownership. Loader/checker regions cover work on tsrs's own threads.
    let region = Region::new(1 << 20);
    let _scope = region.enter();
    let (parsed, errors) = get_parsed_command_line_of_config_file(path, None, None, system, None);
    if !errors.is_empty() {
        result
            .findings
            .extend(convert_diagnostics(&repo_root.to_string_lossy(), &errors));
        result.exit_code = 1;
        return;
    }
    let Some(parsed) = parsed else {
        result
            .stderr
            .push(format!("parse {path}: no parsed command line returned"));
        result.exit_code = 1;
        return;
    };
    let cwd = Path::new(system.get_current_directory());
    if let Err(error) = clean_outputs(cwd, &parsed, &result.outputs) {
        result.stderr.push(format!("clean outputs failed: {error}"));
        result.exit_code = 1;
        return;
    }
    let program = create_program(system, parsed);
    check_and_emit(program.program, system, repo_root, result);
}

fn create_program(
    system: &CompilerSystem,
    parsed: tsrs_tsoptions::ParsedCommandLine,
) -> OwnedProgram {
    let host = new_cached_fs_compiler_host(
        system.get_current_directory(),
        system.filesystem(),
        &bundled::lib_path(),
        None,
        None,
        None,
    );
    let mut options = ProgramOptions::new(P::new(parsed), host);
    options.create_module_resolver = system.module_resolver();
    options.checker_regions = true;
    options.checker_diagnostic_regions = true;
    let regions = Arc::default();
    options.regions = Some(Arc::clone(&regions));
    OwnedProgram {
        // A panic before new_program returns leaks upstream: no Program pointer
        // reaches this guard to free. See https://github.com/oxc-project/tsrs/issues/646.
        program: new_program(options),
        _regions: regions,
    }
}

// Private guard: no program, checker or diagnostic pointer can be returned by
// this module. Keep loader regions alive until shared data has also been freed.
// The guard drops before the config region, including during unwinding.
struct OwnedProgram {
    program: &'static Program,
    _regions: Arc<tsrs_compiler::ProgramRegions>,
}

impl Drop for OwnedProgram {
    fn drop(&mut self) {
        // SAFETY: this is a one-shot program, never reused or shared with another
        // program. Checkers, callbacks and diagnostics have finished before drop.
        unsafe { free_unshared_program(self.program) };
    }
}

fn check_and_emit(
    program: &'static Program,
    system: &CompilerSystem,
    repo_root: &Path,
    result: &mut CompileResult,
) {
    let ctx = Context::default();
    let mut diagnostics = get_diagnostics_of_any_program(
        &ctx,
        program,
        None,
        false,
        &mut |ctx, file| program.get_bind_diagnostics(ctx, file),
        &mut |ctx, file| program.get_semantic_diagnostics(ctx, file),
    );
    let paths = Mutex::new(Vec::new());
    let write_file = |path: &str, text: &str, _: &mut WriteFileData| {
        system.fs().write_file(path, text)?;
        paths.lock().unwrap().push(path.to_owned());
        Ok(())
    };
    // Emit even on diagnostics unless compiler options themselves prohibit it.
    let emitted = program.emit(
        &ctx,
        &EmitOptions {
            write_file: Some(&write_file),
            ..Default::default()
        },
    );
    result.outputs.extend(paths.into_inner().unwrap());
    diagnostics.extend(emitted.diagnostics);
    // noEmitOnError returns pre-emit diagnostics again when emission is skipped.
    let diagnostics = sort_and_deduplicate_diagnostics(&diagnostics);
    if !diagnostics.is_empty() {
        result.exit_code = 1;
        result.findings.extend(convert_diagnostics(
            &repo_root.to_string_lossy(),
            &diagnostics,
        ));
    }
}

#[cfg(test)]
mod tests;
