use std::path::PathBuf;

use luchta_test_support::require_nextest;
use serde_json::{json, Value};
use tempfile::TempDir;

use super::*;
use crate::sarif::build_sarif;

struct Fixture {
    root: TempDir,
    cwd: PathBuf,
}

impl Fixture {
    fn new(options: Value) -> Self {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("packages/pkg");
        let fixture = Self { root, cwd };
        fixture.config("tsconfig.json", options);
        fixture.write("src/index.ts", "export const answer: number = 42;\n");
        fixture
    }

    fn config(&self, name: &str, options: Value) {
        self.write(
            name,
            &json!({"compilerOptions": options, "include": ["src/**/*"]}).to_string(),
        );
    }

    fn write(&self, name: &str, text: &str) {
        let path = self.cwd.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    async fn compile(&self) -> CompileResult {
        compile_package(&self.cwd, self.root.path()).await
    }
}

fn options() -> Value {
    json!({"declaration": true, "outDir": "dist", "rootDir": "src", "module": "nodenext", "moduleResolution": "nodenext"})
}

fn assert_success(result: &CompileResult, outputs: &[&str]) {
    assert_no_diagnostics(result);
    assert_eq!(result.exit_code, 0, "{result:?}");
    assert_eq!(result.outputs, outputs);
}

fn assert_no_diagnostics(result: &CompileResult) {
    assert!(result.findings.is_empty(), "{result:?}");
    assert!(result.stderr.is_empty(), "{result:?}");
}

fn assert_diagnostic_failure(result: &CompileResult) {
    assert_eq!(result.exit_code, 1, "{result:?}");
    assert!(!result.findings.is_empty(), "{result:?}");
    assert!(result.stderr.is_empty(), "{result:?}");
}

fn assert_internal_failure(result: &CompileResult) {
    assert_eq!(result.exit_code, 1, "{result:?}");
    assert!(result.findings.is_empty(), "{result:?}");
    assert!(result.outputs.is_empty(), "{result:?}");
}

// Go: TestCompilePackageSuccess.
#[tokio::test]
async fn compile_package_success() {
    require_nextest();
    let fixture = Fixture::new(options());
    let result = fixture.compile().await;
    assert_success(&result, &["dist/index.d.ts", "dist/index.js"]);
    assert!(fixture.cwd.join("dist/index.d.ts").is_file());
    assert!(fixture.cwd.join("dist/index.js").is_file());
}

// Go: TestCompilePackageRemovesStaleDeclaration.
#[tokio::test]
async fn compile_package_removes_stale_declaration() {
    require_nextest();
    let fixture = Fixture::new(json!({"declaration":true,"outDir":"dist","rootDir":"src"}));
    fixture.write("dist/gone.d.ts", "export declare const gone: number;");
    let result = fixture.compile().await;
    assert_success(&result, &["dist/index.d.ts", "dist/index.js"]);
    assert!(!fixture.cwd.join("dist/gone.d.ts").exists());
}

// Go: TestCompilePackageCleansDeclarationDir.
#[tokio::test]
async fn compile_package_cleans_declaration_dir() {
    require_nextest();
    let fixture = Fixture::new(
        json!({"declaration":true,"outDir":"dist/js","declarationDir":"dist/types","rootDir":"src"}),
    );
    fixture.write("dist/types/gone.d.ts", "export declare const gone: number;");
    let result = fixture.compile().await;
    assert_success(&result, &["dist/js/index.js", "dist/types/index.d.ts"]);
    assert!(!fixture.cwd.join("dist/types/gone.d.ts").exists());
    assert!(fixture.cwd.join("dist/types/index.d.ts").is_file());
}

#[tokio::test]
async fn compile_package_preserves_outputs_from_earlier_configs() {
    require_nextest();
    let mut opts = options();
    opts["emitDeclarationOnly"] = json!(true);
    opts["declarationMap"] = json!(true);
    let fixture = Fixture::new(opts.clone());
    fixture.config("tsconfig.build.json", opts.clone());
    opts["rootDir"] = json!(".");
    fixture.config("tsconfig.json", opts);
    fixture.write("dist/gone.d.ts", "export declare const gone: number;");

    let result = fixture.compile().await;
    assert_success(
        &result,
        &[
            "dist/index.d.ts",
            "dist/index.d.ts.map",
            "dist/src/index.d.ts",
            "dist/src/index.d.ts.map",
        ],
    );
    for output in &result.outputs {
        assert!(
            fixture.cwd.join(output).is_file(),
            "missing output {output}"
        );
    }
    assert!(!fixture.cwd.join("dist/gone.d.ts").exists());
}

#[tokio::test]
async fn compile_package_preserves_authored_declaration_inputs() {
    require_nextest();
    for extension in ["d.ts", "d.mts", "d.cts"] {
        let mut opts = options();
        opts["declarationDir"] = json!("types");
        opts["emitDeclarationOnly"] = json!(true);
        let fixture = Fixture::new(opts.clone());
        let declaration = format!("types/env.{extension}");
        let authored = "interface Env { value: string }\n";
        fixture.write(&declaration, authored);
        let map = format!("{declaration}.map");
        let authored_map = r#"{"version":3,"sources":["env.ts"],"mappings":""}"#;
        fixture.write(&map, authored_map);
        fixture.write("types/gone.d.ts", "interface Gone {}\n");
        fixture.write("types/gone.d.ts.map", "{}");
        fixture.write("src/index.ts", "export const env: Env = { value: 'ok' };\n");
        fixture.write(
            "tsconfig.json",
            &json!({
                "compilerOptions": opts,
                "files": ["src/index.ts", format!("./types/../types/env.{extension}")],
            })
            .to_string(),
        );

        let result = fixture.compile().await;
        assert_success(&result, &["types/index.d.ts"]);
        assert!(fixture.cwd.join("types/index.d.ts").is_file());
        assert_eq!(
            std::fs::read_to_string(fixture.cwd.join(&declaration)).unwrap(),
            authored,
        );
        assert_eq!(
            std::fs::read_to_string(fixture.cwd.join(&map)).unwrap(),
            authored_map,
        );
        assert!(!fixture.cwd.join("types/gone.d.ts").exists());
        assert!(!fixture.cwd.join("types/gone.d.ts.map").exists());
    }
}

// Go: TestCompilePackageRejectsEscapingCleanDirectory (both table cases).
#[tokio::test]
async fn compile_package_rejects_escaping_clean_directory() {
    require_nextest();
    let cases = [
        json!({"declaration":true,"outDir":"../../dist","rootDir":"src"}),
        json!({"declaration":true,"outDir":"dist/js","declarationDir":"../../dist","rootDir":"src"}),
    ];
    for options in cases {
        let fixture = Fixture::new(options);
        fixture.write("../../dist/gone.d.ts", "export declare const gone: number;");
        fixture.write("dist/js/gone.d.ts", "export declare const gone: number;");
        let result = fixture.compile().await;
        assert_internal_failure(&result);
        assert!(result.stderr[0].contains("outside package"), "{result:?}");
        assert!(fixture.root.path().join("dist/gone.d.ts").is_file());
        assert!(fixture.cwd.join("dist/js/gone.d.ts").is_file());
    }
}

// Go: TestCompilePackageUsesConfigDirAsSourceRootWhenRootDirOmitted.
#[tokio::test]
async fn compile_package_uses_config_dir_as_source_root_when_root_dir_omitted() {
    require_nextest();
    let fixture = Fixture::new(json!({"outDir":"dist"}));
    std::fs::rename(
        fixture.cwd.join("src/index.ts"),
        fixture.cwd.join("index.ts"),
    )
    .unwrap();
    fixture.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"outDir":"dist"},"include":["*.ts"]}"#,
    );
    fixture.write("dist/index.d.ts", "export declare const answer: number;");
    let result = fixture.compile().await;
    assert_success(&result, &["dist/index.js"]);
    assert!(fixture.cwd.join("dist/index.d.ts").is_file());
}

// Go: TestCompilePackageTypeError, including its end-to-end SARIF assertion.
#[tokio::test]
async fn compile_package_type_error() {
    require_nextest();
    let fixture = Fixture::new(
        json!({"outDir":"dist","rootDir":"src","module":"nodenext","moduleResolution":"nodenext","strict":true}),
    );
    fixture.write(
        "src/index.ts",
        "export const answer: number = \"not a number\";\n",
    );
    let result = fixture.compile().await;
    assert_diagnostic_failure(&result);
    assert_eq!(result.outputs, ["dist/index.js"]);
    // Serialization here happens after the blocking task freed every program.
    let report: Value = serde_json::from_str(&build_sarif(&result.findings).unwrap()).unwrap();
    let finding = &report["runs"][0]["results"][0];
    assert_eq!(finding["ruleId"], "TS2322");
    assert_eq!(
        finding["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
        "packages/pkg/src/index.ts"
    );
}

// Go: TestCompilePackageWithTsconfigBuild. Both configs compile, build first.
#[tokio::test]
async fn compile_package_with_tsconfig_build() {
    require_nextest();
    let fixture = Fixture::new(options());
    fixture.write("tsconfig.json", "{}");
    fixture.config("tsconfig.build.json", options());
    let result = fixture.compile().await;
    assert_success(
        &result,
        &["dist/index.d.ts", "dist/index.js", "src/index.js"],
    );
}

// Go: TestCompilePackageConcurrentRace (four distinct packages, three files each).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compile_package_concurrent_race() {
    require_nextest();
    let mut jobs = Vec::new();
    for _ in 0..4 {
        let fixture = Fixture::new(options());
        std::fs::remove_file(fixture.cwd.join("src/index.ts")).unwrap();
        for name in ["a", "b", "c"] {
            fixture.write(
                &format!("src/{name}.ts"),
                &format!("export const {name} = 1;\n"),
            );
        }
        jobs.push(tokio::spawn(async move { fixture.compile().await }));
    }
    for job in jobs {
        assert_success(
            &job.await.unwrap(),
            &[
                "dist/a.d.ts",
                "dist/a.js",
                "dist/b.d.ts",
                "dist/b.js",
                "dist/c.d.ts",
                "dist/c.js",
            ],
        );
    }
}

#[tokio::test]
async fn compile_package_no_config_is_success() {
    require_nextest();
    let fixture = Fixture::new(options());
    std::fs::remove_file(fixture.cwd.join("tsconfig.json")).unwrap();
    assert_success(&fixture.compile().await, &[]);
}

#[tokio::test]
async fn compile_package_stops_after_first_config_diagnostics() {
    require_nextest();
    let fixture = Fixture::new(options());
    fixture.write("tsconfig.build.json", "{broken");
    let result = fixture.compile().await;
    assert_diagnostic_failure(&result);
    // Parsed-config diagnostics still allow emit, as in Go. The second config
    // would emit into dist, so its absence proves candidate ordering and stop.
    assert_eq!(result.outputs, ["src/index.js"]);
    assert!(!fixture.cwd.join("dist").exists());
}

#[tokio::test]
async fn compile_package_stops_after_first_config_type_error_but_emits() {
    require_nextest();
    let fixture = Fixture::new(options());
    fixture.config("tsconfig.build.json", options());
    fixture.write("src/index.ts", "export const answer: number = 'bad';");
    let result = fixture.compile().await;
    assert_eq!(result.exit_code, 1, "{result:?}");
    assert!(!result.findings.is_empty());
    // Duplicates would prove the second config was compiled as well.
    assert_eq!(result.outputs, ["dist/index.d.ts", "dist/index.js"]);
}

#[tokio::test]
async fn compile_package_skip_lib_check() {
    require_nextest();
    let fixture = Fixture::new(options());
    fixture.write("src/unchecked.d.ts", "declare const duplicate: number; declare const duplicate: string; declare const unknown: MissingType;");
    for skip in [true, false] {
        let mut opts = options();
        opts["skipLibCheck"] = json!(skip);
        fixture.config("tsconfig.json", opts);
        let result = fixture.compile().await;
        assert_eq!(result.exit_code, i32::from(!skip), "{result:?}");
        assert_eq!(result.findings.is_empty(), skip, "{result:?}");
        assert_eq!(result.outputs, ["dist/index.d.ts", "dist/index.js"]);
    }
}

#[tokio::test]
async fn compile_package_no_emit_preserves_stale_outputs() {
    require_nextest();
    let fixture = Fixture::new(json!({"noEmit":true,"outDir":"dist"}));
    fixture.write("dist/gone.d.ts", "export declare const gone: number;");
    assert_success(&fixture.compile().await, &[]);
    assert!(fixture.cwd.join("dist/gone.d.ts").is_file());
}

#[tokio::test]
async fn compile_package_no_emit_on_error() {
    require_nextest();
    let fixture = Fixture::new(
        json!({"noEmitOnError":true,"declaration":true,"outDir":"dist","rootDir":"src"}),
    );
    fixture.write("src/index.ts", "export const answer: number = 'bad';");
    let result = fixture.compile().await;
    assert_eq!(result.exit_code, 1, "{result:?}");
    assert_eq!(result.findings.len(), 1, "{result:?}");
    assert_eq!(result.findings[0].rule_id.as_deref(), Some("TS2322"));
    assert!(result.outputs.is_empty());
}

#[tokio::test]
async fn compile_package_records_declaration_and_source_maps() {
    require_nextest();
    let fixture = Fixture::new(
        json!({"declaration":true,"sourceMap":true,"declarationMap":true,"outDir":"dist","rootDir":"src"}),
    );
    assert_success(
        &fixture.compile().await,
        &[
            "dist/index.d.ts",
            "dist/index.d.ts.map",
            "dist/index.js",
            "dist/index.js.map",
        ],
    );
}

#[tokio::test]
async fn compile_package_emit_write_error_is_diagnostic() {
    require_nextest();
    let fixture = Fixture::new(options());
    fixture.write("dist/index.js/blocked", "blocks writing index.js");
    let result = fixture.compile().await;
    assert_eq!(result.exit_code, 1, "{result:?}");
    assert!(!result.findings.is_empty(), "{result:?}");
    assert_eq!(result.outputs, ["dist/index.d.ts"]);
}

#[tokio::test]
async fn compile_package_does_not_write_tsbuildinfo() {
    require_nextest();
    let fixture = Fixture::new(options());
    for kind in ["composite", "incremental"] {
        let mut opts = options();
        opts[kind] = json!(true);
        opts["tsBuildInfoFile"] = json!("dist/tsconfig.build.tsbuildinfo");
        fixture.config("tsconfig.json", opts);
        assert_success(
            &fixture.compile().await,
            &["dist/index.d.ts", "dist/index.js"],
        );
        assert!(!fixture.cwd.join("dist/tsconfig.build.tsbuildinfo").exists());
    }
}

#[tokio::test]
async fn compile_panic_frees_program_and_shared_data() {
    require_nextest();
    let fixture = Fixture::new(options());
    let cwd = fixture.cwd.clone();
    let result = tokio::task::spawn_blocking(move || {
        let region = Region::new(1 << 20);
        let _scope = region.enter();
        let system = CompilerSystem::new(&cwd).unwrap();
        let fs = system.filesystem();
        let count_before = Arc::strong_count(&fs);
        let result = catch_compile(|| {
            let path = format!("{}/tsconfig.json", system.get_current_directory());
            let (parsed, errors) =
                get_parsed_command_line_of_config_file(&path, None, None, &system, None);
            assert!(errors.is_empty());
            let program = create_program(&system, parsed.unwrap());
            // Exercise checker allocation before unwinding, with no production
            // injection flag. The FS is held by both program and shared hosts.
            let mut result = CompileResult::default();
            check_and_emit(program.program, &system, &cwd, &mut result);
            assert!(Arc::strong_count(&fs) > count_before);
            panic!("injected compile panic");
        });
        assert_eq!(
            Arc::strong_count(&fs),
            count_before,
            "program or shared hosts leaked"
        );
        result
    })
    .await
    .unwrap();
    assert_eq!(result.exit_code, 1);
    assert_eq!(result.stderr, ["panic: injected compile panic"]);
    assert!(result.findings.is_empty());
    assert_panic_protocol(result).await;
    assert_success(
        &fixture.compile().await,
        &["dist/index.d.ts", "dist/index.js"],
    );
}

async fn assert_panic_protocol(result: CompileResult) {
    use luchta_worker::{InProcessOutcome, JobContext, LogStream, WorkerResponse};
    use tokio::io::{AsyncBufReadExt, BufReader};

    let (writer, reader) = tokio::io::duplex(4096);
    let ctx = JobContext::new(
        "panic-job".to_owned(),
        Arc::new(tokio::sync::Mutex::new(Box::new(writer))),
    );
    assert_eq!(
        crate::worker::finish_compile(result, &ctx).await,
        InProcessOutcome::Done {
            exit_code: 1,
            outputs: None
        }
    );
    drop(ctx);
    let mut lines = BufReader::new(reader).lines();
    let response: WorkerResponse =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(
        response,
        WorkerResponse::log(
            "panic-job",
            LogStream::Stderr,
            "panic: injected compile panic"
        )
    );
    assert!(
        lines.next_line().await.unwrap().is_none(),
        "no panic SARIF report"
    );
}

#[cfg(target_os = "linux")]
fn rss_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn compile_sequential_rss_is_bounded() {
    require_nextest();
    let fixture = Fixture::new(options());
    for index in 0..32 {
        fixture.write(&format!("src/file{index}.ts"), &format!("import {{ answer }} from './index';\nexport const value{index}: Map<string, number[]> = new Map([['answer', [answer]]]);\n"));
    }
    for _ in 0..3 {
        let result = fixture.compile().await;
        assert_eq!(result.exit_code, 0, "{result:?}");
        assert_eq!(result.outputs.len(), 66);
    }
    let baseline = rss_kib();
    for _ in 0..20 {
        let result = fixture.compile().await;
        assert_eq!(result.exit_code, 0, "{result:?}");
        assert_eq!(result.outputs.len(), 66);
    }
    let final_rss = rss_kib();
    eprintln!("compile RSS: baseline={baseline} KiB final={final_rss} KiB");
    assert!(
        final_rss <= baseline * 3 / 2 + 64 * 1024,
        "RSS grew: baseline={baseline} KiB final={final_rss} KiB"
    );
}

#[tokio::test]
async fn compile_package_reports_both_candidates_including_duplicate_outputs() {
    require_nextest();
    let fixture = Fixture::new(options());
    fixture.config("tsconfig.build.json", options());
    assert_success(
        &fixture.compile().await,
        &[
            "dist/index.d.ts",
            "dist/index.d.ts",
            "dist/index.js",
            "dist/index.js",
        ],
    );
}

#[tokio::test]
async fn compile_package_collects_declaration_emit_diagnostics() {
    require_nextest();
    let fixture = Fixture::new(options());
    fixture.write(
        "src/index.ts",
        "export const instance = new class { private secret = 1; };\n",
    );
    let result = fixture.compile().await;
    assert_diagnostic_failure(&result);
    assert_eq!(result.outputs, ["dist/index.js"]);
}

#[tokio::test]
async fn compile_package_pnp_extends_and_zipped_import() {
    require_nextest();
    use luchta_yarn_env::test_fixture::{FixtureOptions, PnpFixture};
    use std::io::Write;

    let temp = tempfile::tempdir().unwrap();
    let root = tsrs_vfs::osvfs::fs().realpath(&temp.path().to_string_lossy());
    let fixture = PnpFixture::write(Path::new(&root), FixtureOptions::default());
    let archive = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fixture.left_pad_zip)
        .unwrap();
    let mut writer = zip::ZipWriter::new_append(archive).unwrap();
    writer
        .start_file(
            "node_modules/left-pad/index.d.ts",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    writer
        .write_all(b"export declare const answer: number;\n")
        .unwrap();
    writer.finish().unwrap();
    let base = fixture
        .root
        .join(".yarn/unplugged/native-thing-npm-1.0.0-xyz/node_modules/native-thing/compile.json");
    std::fs::write(
        base,
        r#"{"compilerOptions":{"declaration":true,"module":"nodenext","skipLibCheck":true}}"#,
    )
    .unwrap();
    std::fs::write(fixture.app_dir.join("tsconfig.json"), r#"{"extends":"native-thing/compile.json","compilerOptions":{"outDir":"dist","rootDir":"."},"files":["index.ts"]}"#).unwrap();
    std::fs::write(
        fixture.app_dir.join("index.ts"),
        "import {answer} from 'left-pad'; export const result: number = answer;\n",
    )
    .unwrap();
    let result = compile_package(&fixture.app_dir, &fixture.root).await;
    assert_success(&result, &["dist/index.d.ts", "dist/index.js"]);
    assert!(fixture.app_dir.join("dist/index.d.ts").is_file());
}

#[cfg(unix)]
#[tokio::test]
async fn compile_package_symlinked_checkout_keeps_outputs_and_sarif_relative() {
    require_nextest();
    let fixture = Fixture::new(options());
    fixture.write("src/index.ts", "export const answer: number = 'bad';");
    let alias = fixture.root.path().join("alias");
    std::os::unix::fs::symlink(fixture.root.path(), &alias).unwrap();
    let result = compile_package(&alias.join("packages/pkg"), &alias).await;
    assert_diagnostic_failure(&result);
    assert_eq!(result.outputs, ["dist/index.d.ts", "dist/index.js"]);
    assert_eq!(
        result.findings[0].uri.as_deref(),
        Some("packages/pkg/src/index.ts")
    );
}
