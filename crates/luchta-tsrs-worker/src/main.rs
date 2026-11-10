#[cfg(not(any(
    target_arch = "arm",
    target_arch = "riscv64",
    miri,
    target_os = "freebsd",
    target_family = "wasm"
)))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use luchta_tsrs_worker::worker::TsrsWorker;
use luchta_worker::run_worker_main;

fn main() {
    if luchta_worker::version_requested(
        &std::env::args().collect::<Vec<_>>(),
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    ) {
        return;
    }

    let repo_root = std::env::current_dir().unwrap_or_else(|error| {
        eprintln!("luchta-tsrs-worker: get working directory: {error}");
        std::process::exit(1);
    });
    let worker = TsrsWorker::new(repo_root);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(async { run_worker_main(worker).await });
}
