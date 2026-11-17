//! A line-oriented server: one shell, many commands.
//!
//! Requests are JSON objects on stdin, one per line; responses are JSON on
//! stdout, one per line, echoing the request's `id`. This is the shape a coding
//! agent's tool harness wants: shell state that survives between tool calls, and
//! checkpoints it can rewind to.
//!
//! ```text
//! {"id":1,"op":"run","script":"cd /tmp; pwd"}
//! {"id":2,"op":"snapshot","key":"before-build"}
//! {"id":3,"op":"branch","scripts":["npm test","pnpm test"]}
//! {"id":4,"op":"restore","snapshot":"before-build"}
//! {"id":5,"op":"shutdown"}
//! ```

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::check::CheckReport;
use crate::report::Report;
use crate::session::{Options, Session, StateSummary};

#[derive(Debug, Default, Deserialize)]
struct Request {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    op: String,
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    scripts: Vec<String>,
    #[serde(default)]
    snapshot: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Default, Serialize)]
struct Response {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<Report>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reports: Option<Vec<Report>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    check: Option<CheckReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<StateSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshots: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Response {
    fn ok(id: Option<Value>) -> Self {
        Self {
            id,
            ok: true,
            ..Self::default()
        }
    }

    fn err(id: Option<Value>, message: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            error: Some(message.into()),
            ..Self::default()
        }
    }
}

/// Runs the server until `shutdown` or end of input.
pub async fn run(opts: Options) -> Result<(), String> {
    let mut session = Session::new(opts).await?;
    let mut snapshots: HashMap<String, Session> = HashMap::new();
    let mut next_snapshot = 0usize;

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        if line.trim().is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(e) => {
                let response = Response::err(None, format!("bad request: {e}"));
                write(&mut stdout, &response).await?;
                continue;
            }
        };

        let id = request.id.clone();

        match request.op.as_str() {
            "run" => {
                let Some(script) = request.script.clone() else {
                    write(&mut stdout, &Response::err(id, "run needs a script")).await?;
                    continue;
                };
                let timeout = request.timeout_ms.map(Duration::from_millis);
                let report = session.run_opts(&script, timeout).await;
                let mut response = Response::ok(id);
                response.report = Some(report);
                write(&mut stdout, &response).await?;
            }
            "check" => {
                let Some(script) = request.script.clone() else {
                    write(&mut stdout, &Response::err(id, "check needs a script")).await?;
                    continue;
                };
                let mut response = Response::ok(id);
                response.check = Some(session.check(&script));
                write(&mut stdout, &response).await?;
            }
            "branch" => {
                if request.scripts.is_empty() {
                    write(&mut stdout, &Response::err(id, "branch needs scripts")).await?;
                    continue;
                }
                let reports = session.branch(&request.scripts).await;
                let mut response = Response::ok(id);
                response.reports = Some(reports);
                write(&mut stdout, &response).await?;
            }
            "snapshot" => {
                let key = request.key.unwrap_or_else(|| {
                    next_snapshot += 1;
                    format!("snap-{next_snapshot}")
                });
                snapshots.insert(key.clone(), session.snapshot());
                let mut response = Response::ok(id);
                response.snapshot = Some(key);
                write(&mut stdout, &response).await?;
            }
            "restore" => {
                let Some(key) = request.snapshot.or(request.key) else {
                    write(&mut stdout, &Response::err(id, "restore needs a snapshot")).await?;
                    continue;
                };
                let Some(saved) = snapshots.get(&key) else {
                    write(
                        &mut stdout,
                        &Response::err(id, format!("no snapshot named {key}")),
                    )
                    .await?;
                    continue;
                };
                session = saved.clone();
                let mut response = Response::ok(id);
                response.snapshot = Some(key);
                write(&mut stdout, &response).await?;
            }
            "state" => {
                let mut response = Response::ok(id);
                response.state = Some(session.summary());
                write(&mut stdout, &response).await?;
            }
            "snapshots" => {
                let mut response = Response::ok(id);
                response.snapshots = Some(snapshots.keys().cloned().collect());
                write(&mut stdout, &response).await?;
            }
            "ping" => {
                write(&mut stdout, &Response::ok(id)).await?;
            }
            "shutdown" => {
                write(&mut stdout, &Response::ok(id)).await?;
                return Ok(());
            }
            other => {
                write(
                    &mut stdout,
                    &Response::err(
                        id,
                        format!("unknown op {other:?}: run, check, branch, snapshot, restore, state, snapshots, ping, shutdown"),
                    ),
                )
                .await?;
            }
        }
    }

    Ok(())
}

async fn write(stdout: &mut tokio::io::Stdout, response: &Response) -> Result<(), String> {
    let line = serde_json::to_string(response).map_err(|e| e.to_string())?;
    stdout
        .write_all(line.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stdout.write_all(b"\n").await.map_err(|e| e.to_string())?;
    stdout.flush().await.map_err(|e| e.to_string())
}
