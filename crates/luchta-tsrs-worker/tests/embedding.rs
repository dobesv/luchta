use std::collections::BTreeMap;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};

use tsrs_compiler::{
    free_program, get_diagnostics_of_any_program, new_cached_fs_compiler_host, new_program,
    shared_program_data, Context, EmitOptions, ProgramOptions, WriteFileData,
};
use tsrs_core::P;
use tsrs_tsoptions::{get_parsed_command_line_of_config_file, ParseConfigHost};
use tsrs_vfs::{bundled, vfstest, FS};

struct ConfigHost(Arc<dyn FS>);

impl ParseConfigHost for ConfigHost {
    fn fs(&self) -> &dyn FS {
        self.0.as_ref()
    }

    fn get_current_directory(&self) -> &str {
        "/project"
    }
}

fn fixture_program() -> &'static tsrs_compiler::Program {
    let fs: Arc<dyn FS> = Arc::new(bundled::wrap_fs(vfstest::from_map(
        [
            (
                "/project/tsconfig.json",
                include_str!("fixtures/tsconfig.json"),
            ),
            ("/project/index.ts", include_str!("fixtures/index.ts")),
            (
                "/project/unchecked.d.ts",
                include_str!("fixtures/unchecked.d.ts"),
            ),
        ],
        true,
    )));
    let config_host = ConfigHost(fs.clone());
    let (config, errors) = get_parsed_command_line_of_config_file(
        "/project/tsconfig.json",
        None,
        None,
        &config_host,
        None,
    );
    assert!(errors.is_empty(), "config read diagnostics: {errors:?}");
    let config = config.expect("fixture config must parse");
    assert!(
        config.errors.is_empty(),
        "config diagnostics: {:?}",
        config.errors
    );
    assert!(config
        .compiler_options()
        .expect("fixture compiler options must exist")
        .skip_lib_check
        .is_true());

    let host = new_cached_fs_compiler_host("/project", fs, &bundled::lib_path(), None, None, None);
    let mut options = ProgramOptions::new(P::new(config), host);
    options.checker_regions = true;
    options.checker_diagnostic_regions = true;
    options.regions = Some(Arc::default());
    new_program(options)
}

#[test]
fn tsrs_embedding_parses_checks_emits_and_frees_program() {
    // tsrs initializes process-wide worker pools and thread-local arenas.
    luchta_test_support::require_nextest();
    let program = fixture_program();
    let shared = shared_program_data(program);

    // Even a failed assertion must release the program before propagating the panic.
    let result = catch_unwind(AssertUnwindSafe(|| {
        let ctx = Context::default();
        let diagnostics = get_diagnostics_of_any_program(
            &ctx,
            program,
            None,
            false,
            &mut |ctx, file| program.get_bind_diagnostics(ctx, file),
            &mut |ctx, file| program.get_semantic_diagnostics(ctx, file),
        );
        assert!(
            diagnostics.is_empty(),
            "program diagnostics: {diagnostics:?}"
        );

        let written = Mutex::new(BTreeMap::new());
        let write_file = |path: &str, text: &str, _data: &mut WriteFileData| {
            written
                .lock()
                .unwrap()
                .insert(path.to_owned(), text.to_owned());
            Ok(())
        };
        let emitted = program.emit(
            &ctx,
            &EmitOptions {
                write_file: Some(&write_file),
                ..Default::default()
            },
        );
        assert!(!emitted.emit_skipped);
        assert!(
            emitted.diagnostics.is_empty(),
            "emit diagnostics: {:?}",
            emitted.diagnostics
        );
        let written = written.into_inner().unwrap();
        assert_eq!(
            written.len(),
            1,
            "only declarations should be emitted: {written:?}"
        );
        assert_eq!(
            written["/project/dist/index.d.ts"].trim(),
            "export declare const answer: number;"
        );
    }));

    // SAFETY: diagnostics, emit results and callbacks have been dropped; no checkers
    // are held and this one-shot program never shared data with another program.
    unsafe {
        free_program(program);
        shared.free();
    }
    if let Err(panic) = result {
        resume_unwind(panic);
    }
}
