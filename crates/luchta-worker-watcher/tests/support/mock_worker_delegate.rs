use std::io::{self, BufRead, Write};
use std::thread;
use std::time::Duration;

use serde_json::Value;

fn main() {
    let instance =
        std::env::var("MOCK_INSTANCE").unwrap_or_else(|_| format!("pid-{}", std::process::id()));
    eprintln!("INSTANCE:{instance}");

    let delay_ms = std::env::var("MOCK_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let default_delay = Duration::from_millis(delay_ms);

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                eprintln!("READ_ERROR:{error}");
                break;
            }
        };

        let value: Value = serde_json::from_str(&line).expect("valid worker message json");
        let id = value["id"].as_str().expect("message id string");
        let message_type = value["type"].as_str().expect("message type string");
        let command = value["command"].as_str().unwrap_or_default();
        let delay = command
            .strip_prefix("delay:")
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(default_delay);

        if command.contains("log-first") {
            writeln!(
                stdout,
                "{}",
                serde_json::json!({
                    "type": "log",
                    "id": id,
                    "stream": "stdout",
                    "line": format!("log from {instance}"),
                })
            )
            .expect("write log");
            stdout.flush().expect("flush log");
        }

        if !delay.is_zero() {
            thread::sleep(delay);
        }

        let response = match message_type {
            "run" => serde_json::json!({
                "type": "done",
                "id": id,
                "exitCode": 0,
            }),
            "resolveTask" => serde_json::json!({
                "type": "resolved",
                "id": id,
                "result": { "decision": "accept" },
            }),
            other => panic!("unexpected message type: {other}"),
        };

        writeln!(stdout, "{response}").expect("write response");
        stdout.flush().expect("flush response");
    }

    eprintln!("EOF:{instance}");
}
