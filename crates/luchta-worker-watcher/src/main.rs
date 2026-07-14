use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWrite, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use luchta_worker::{split_current_process_argv, version_requested, WorkerMessage};
use luchta_worker_watcher::{
    cli,
    router::{MessageRouter, RouterEvent},
    watch::{self, WatchConfig},
};

fn main() {
    let exit_code = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(async_main()),
        Err(error) => {
            eprintln!("failed to build tokio runtime: {error}");
            1
        }
    };
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

async fn async_main() -> i32 {
    let split = split_current_process_argv();
    if version_requested(
        &split.stage_args,
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    ) {
        return 0;
    }

    let cli = match cli::parse(std::env::args()) {
        Ok(cli) => cli,
        Err(error) => {
            eprintln!("{error}");
            eprintln!("{}", cli::usage());
            return 2;
        }
    };

    let (events_tx, events_rx) = mpsc::channel::<RouterEvent>(1024);
    let stdout_writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(stdout());
    let router = match MessageRouter::new(
        cli.delegate_command.clone(),
        events_tx.clone(),
        stdout_writer,
    )
    .await
    {
        Ok(router) => router,
        Err(error) => {
            eprintln!("failed to create router: {error}");
            return 1;
        }
    };

    let watcher_task = spawn_watcher(cli.watch_globs.clone(), cli.debounce, events_tx.clone());
    let stdin_task = spawn_stdin_reader(events_tx.clone());
    drop(events_tx);

    let result = router.run(events_rx).await;

    watcher_task.abort();
    stdin_task.abort();
    let _ = watcher_task.await;
    let _ = stdin_task.await;

    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("router failed: {error}");
            1
        }
    }
}

fn spawn_watcher(
    watch_globs: Vec<String>,
    debounce: std::time::Duration,
    events_tx: mpsc::Sender<RouterEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (change_tx, mut change_rx) = mpsc::channel::<()>(16);
        let watch_config = WatchConfig {
            globs: watch_globs,
            debounce,
        };

        let watcher_task = tokio::spawn(async move {
            // Watcher setup failure is logged; wrapper keeps running as passthrough.
            if let Err(error) = watch::run(watch_config, change_tx).await {
                eprintln!("watcher failed: {error}");
            }
        });

        while change_rx.recv().await.is_some() {
            if events_tx.send(RouterEvent::FileChanged).await.is_err() {
                break;
            }
        }

        watcher_task.abort();
        let _ = watcher_task.await;
    })
}

fn spawn_stdin_reader(events_tx: mpsc::Sender<RouterEvent>) -> JoinHandle<()> {
    tokio::spawn(async move {
        run_stdin_reader(events_tx).await;
    })
}

async fn run_stdin_reader(events_tx: mpsc::Sender<RouterEvent>) {
    let mut lines = BufReader::new(stdin()).lines();

    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if forward_stdin_line(&events_tx, &line).await.is_err() {
                    break;
                }
            }
            Ok(None) => {
                send_shutdown(&events_tx).await;
                break;
            }
            Err(error) => {
                eprintln!("failed to read worker stdin: {error}");
                send_shutdown(&events_tx).await;
                break;
            }
        }
    }
}

async fn forward_stdin_line(events_tx: &mpsc::Sender<RouterEvent>, line: &str) -> Result<(), ()> {
    match serde_json::from_str::<WorkerMessage>(line) {
        // Middleware stays alive on one bad line: log and continue.
        Ok(message) => events_tx
            .send(RouterEvent::Inbound(message))
            .await
            .map_err(|_| ()),
        Err(error) => {
            eprintln!("failed to parse worker message: {error}");
            Ok(())
        }
    }
}

async fn send_shutdown(events_tx: &mpsc::Sender<RouterEvent>) {
    let _ = events_tx.send(RouterEvent::ShutdownAll).await;
}
