#[cfg(feature = "oxc")]
mod config;
#[cfg(feature = "oxc")]
mod format;
#[cfg(feature = "oxc")]
mod opts;
#[cfg(feature = "oxc")]
mod worker;

#[cfg(feature = "oxc")]
use luchta_worker::run_worker_main;
#[cfg(feature = "oxc")]
use worker::OxfmtWorker;

fn main() {
    if luchta_worker::version_requested(
        &std::env::args().collect::<Vec<_>>(),
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    ) {
        return;
    }

    real_main();
}

#[cfg(feature = "oxc")]
fn real_main() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(async { run_worker_main(OxfmtWorker).await });
}

#[cfg(not(feature = "oxc"))]
fn real_main() {
    eprintln!("this binary was built without the 'oxc' feature; the oxfmt worker is unavailable");
    std::process::exit(1);
}
