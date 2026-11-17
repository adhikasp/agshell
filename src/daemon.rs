//! A persistent shell behind a per-directory daemon.
//!
//! `agshell serve` already keeps one shell so `cd`, `export`, functions and arrays
//! survive between requests. That only works while one process holds the pipe, and a
//! coding agent spawns its shell once per tool call. So the client here hands the script
//! to a daemon over loopback and keeps nothing of its own.
//!
//! The daemon is keyed by working directory: one per project, so `cd src` in one call is
//! still there in the next. It exits after [`IDLE`] with no client, and removes its state
//! file on the way out, so stale entries do not accumulate.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::report::Report;
use crate::session::{Options, Session};

/// How long a daemon waits for a client before it goes away.
const IDLE: Duration = Duration::from_secs(900);

/// Where the port of each daemon is recorded.
fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("AGSHELL_STATE_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(windows) {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("agshell").join("sessions")
    } else {
        std::env::temp_dir().join("agshell-sessions")
    }
}

/// A stable short key for a directory: FNV-1a over its path.
fn key_for(cwd: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in cwd.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn state_file(cwd: &Path) -> PathBuf {
    state_dir().join(format!("{}.addr", key_for(cwd)))
}

async fn connect(path: &Path) -> Option<TcpStream> {
    let address = std::fs::read_to_string(path).ok()?;
    TcpStream::connect(address.trim()).await.ok()
}

/// Returns a connection to this directory's daemon, starting one if needed.
async fn ensure_daemon(cwd: &Path) -> Result<TcpStream, String> {
    let path = state_file(cwd);
    if let Some(stream) = connect(&path).await {
        return Ok(stream);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = Command::new(exe);
    command
        .arg("serve")
        .arg("--listen")
        .arg("127.0.0.1:0")
        .arg("--state-file")
        .arg(&path)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // The daemon has to outlive this tool call, so it gets no console of its own.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }

    command
        .spawn()
        .map_err(|e| format!("could not start the session daemon: {e}"))?;

    // The daemon writes its port once it has bound; poll briefly rather than guess.
    for _ in 0..100 {
        if let Some(stream) = connect(&path).await {
            return Ok(stream);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    Err("the session daemon did not come up".to_string())
}

/// Runs one script in this directory's persistent shell.
pub async fn run(script: &str) -> Result<Report, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut stream = ensure_daemon(&cwd).await?;

    let request = serde_json::json!({ "id": 1, "op": "run", "script": script });
    let mut line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream.flush().await.map_err(|e| e.to_string())?;

    let mut lines = BufReader::new(stream).lines();
    let response = lines
        .next_line()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "the session daemon closed the connection".to_string())?;

    let value: serde_json::Value =
        serde_json::from_str(&response).map_err(|e| format!("bad daemon response: {e}"))?;

    if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        let message = value
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown daemon error");
        return Err(message.to_string());
    }

    let report = value
        .get("report")
        .ok_or_else(|| "the daemon returned no report".to_string())?;
    serde_json::from_value(report.clone()).map_err(|e| e.to_string())
}

/// Serves `run` on a loopback port until `shutdown` or [`IDLE`] with no client.
pub async fn run_server(opts: Options, listen: &str, state_file: &Path) -> Result<(), String> {
    let listener = TcpListener::bind(listen).await.map_err(|e| e.to_string())?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;

    if let Some(parent) = state_file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(state_file, address.to_string()).map_err(|e| e.to_string())?;

    let mut session = Session::new(opts).await?;

    loop {
        let Ok(accepted) = tokio::time::timeout(IDLE, listener.accept()).await else {
            break; // nobody came: retire, and let the next call start a fresh shell
        };
        let Ok((stream, _)) = accepted else { break };

        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut lines = BufReader::new(read_half).lines();

        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let op = request.get("op").and_then(|v| v.as_str()).unwrap_or("");

            let response = match op {
                "run" => {
                    let script = request.get("script").and_then(|v| v.as_str()).unwrap_or("");
                    let report = session.run(script).await;
                    serde_json::json!({ "id": 1, "ok": true, "report": report })
                }
                "ping" => serde_json::json!({ "id": 1, "ok": true }),
                "shutdown" => {
                    let _ = std::fs::remove_file(state_file);
                    return Ok(());
                }
                other => {
                    serde_json::json!({ "id": 1, "ok": false, "error": format!("unknown op {other}") })
                }
            };

            let mut text = serde_json::to_string(&response).map_err(|e| e.to_string())?;
            text.push('\n');
            if write_half.write_all(text.as_bytes()).await.is_err() {
                break;
            }
            let _ = write_half.flush().await;
        }
    }

    let _ = std::fs::remove_file(state_file);
    Ok(())
}
