//! The envelope: what actually ran, as data.
//!
//! bash reports one exit status per command string. This module turns the
//! per-command and per-stage statuses collected by the supervisor into the
//! single word a caller can act on.

use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Status recorded for a command that `&&`/`||` prevented from running.
pub const SKIPPED: i32 = -1;

/// Cap on per-command stderr kept in the envelope.
pub const STDERR_CAP: usize = 2048;

/// One supervised element of a command. A simple command produces one. Each
/// stage of a pipeline produces one. A statement the shell runs whole (a loop,
/// a subshell, a function call) produces one with `delegated` set, followed by
/// the commands seen inside it, each with `inside` pointing back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandRecord {
    /// Position in the script, in execution order, 0-based.
    pub i: usize,
    /// The command's source text, exactly as written where possible.
    pub text: String,
    /// The program name, as a display aid.
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub bin: String,
    /// Exit status, or [`SKIPPED`] if `&&`/`||` skipped it.
    pub status: i32,
    #[serde(skip_serializing_if = "is_false", default)]
    pub skipped: bool,
    #[serde(skip_serializing_if = "is_false", default)]
    pub timed_out: bool,
    /// Wall time in milliseconds. For a pipeline this is the pipeline's total
    /// and only the last stage carries it.
    pub ms: i64,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub stderr: String,
    /// Pipeline id, shared by a pipeline's stages. Absent for standalone commands.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pipeline: Option<usize>,
    /// 0-based stage index within the pipeline.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub stage: Option<usize>,
    /// The statement was handed to the shell whole; its internals were not
    /// observed. A coarse status is better than a wrong one.
    #[serde(skip_serializing_if = "is_false", default)]
    pub delegated: bool,
    /// Index of the statement this command ran inside (a loop body, an `if`
    /// branch, a function, a subshell, an `eval`). Seen through the shell's
    /// `DEBUG`/`ERR` traps, not run by the supervisor.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub inside: Option<usize>,
    /// The status was assumed, not observed: this was the last command of its
    /// context and the shell reported no failure for it.
    #[serde(skip_serializing_if = "is_false", default)]
    pub inferred: bool,
    /// A failure that `||` consumed: expected, not hidden. Never serialized.
    #[serde(skip)]
    pub handled: bool,
    /// Index of the command whose word this substitution provided.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub substitution_of: Option<usize>,
}

impl CommandRecord {
    pub fn simple(i: usize, text: String, bin: String, status: i32, ms: i64) -> Self {
        Self {
            i,
            text,
            bin,
            status,
            skipped: false,
            timed_out: false,
            ms,
            stderr: String::new(),
            pipeline: None,
            stage: None,
            delegated: false,
            handled: false,
            substitution_of: None,
            inside: None,
            inferred: false,
        }
    }
}

/// The envelope. This is the point of agshell: a command's outcome as data,
/// including the statuses the shell text channel destroys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub class: String,
    pub exit: i32,
    pub duration_ms: i64,
    pub commands: Vec<CommandRecord>,
    /// Indexes of commands that failed while the exit code still said 0.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub hidden_failures: Vec<usize>,
    pub stdout: String,
    pub stdout_bytes: usize,
    #[serde(skip_serializing_if = "is_false", default)]
    pub stdout_truncated: bool,
    /// Some substitution was executed by the shell rather than supervised, so
    /// a failure inside it is not in `commands`.
    #[serde(skip_serializing_if = "is_false", default)]
    pub opaque_substitution: bool,
    /// `--strict` stopped the script at the first failure.
    #[serde(skip_serializing_if = "is_false", default)]
    pub aborted: bool,
    /// Index of the command that stopped a `--strict` run.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub stopped_at: Option<usize>,
    /// Ordinary commands seen inside statements that the envelope left out
    /// (a long loop). Unhandled failures are never left out.
    #[serde(skip_serializing_if = "is_zero", default)]
    pub inner_omitted: usize,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub error: String,
}

impl Report {
    /// A report for a script the shell would not parse.
    pub fn parse_error(message: String, duration_ms: i64) -> Self {
        Self {
            class: "parse_error".to_string(),
            exit: 2,
            duration_ms,
            commands: Vec::new(),
            hidden_failures: Vec::new(),
            stdout: String::new(),
            stdout_bytes: 0,
            stdout_truncated: false,
            opaque_substitution: false,
            aborted: false,
            stopped_at: None,
            inner_omitted: 0,
            error: message,
        }
    }
}

/// Turns collected statuses into the one word a caller acts on. Order matters:
/// a timeout outranks a visible failure, a hidden pipeline failure outranks a
/// hidden chain failure, and both outrank "it exited 0 and printed nothing
/// wrong".
pub fn classify(cmds: &[CommandRecord], exit: i32) -> String {
    let timed_out = cmds.iter().any(|c| c.timed_out && !c.skipped);
    let mut any_failed = false;
    let mut stage_failed = false;
    let mut substitution_failed = false;

    for c in cmds.iter().filter(|c| !c.skipped && !c.handled) {
        if c.status == 0 && !c.timed_out {
            continue;
        }
        any_failed = true;
        if c.substitution_of.is_some() {
            substitution_failed = true;
        }
        if let (Some(pid), Some(stage)) = (c.pipeline, c.stage) {
            let last = cmds
                .iter()
                .filter(|o| o.pipeline == Some(pid))
                .map(|o| o.stage.unwrap_or(0))
                .max()
                .unwrap_or(0);
            if stage < last {
                stage_failed = true;
            }
        }
    }

    if timed_out {
        "timeout".to_string()
    } else if exit != 0 {
        "failed".to_string()
    } else if stage_failed {
        "stage_failed".to_string()
    } else if substitution_failed {
        "substitution_failed".to_string()
    } else if any_failed {
        "chain_partial".to_string()
    } else {
        "ok".to_string()
    }
}

/// Lists failed commands whose status the process exit code did not show.
pub fn hidden(cmds: &[CommandRecord], exit: i32) -> Vec<usize> {
    if exit != 0 {
        return Vec::new();
    }

    let last_run = cmds.iter().filter(|c| !c.skipped).map(|c| c.i).max();

    cmds.iter()
        .filter(|c| {
            !c.skipped && !c.handled && (c.status != 0 || c.timed_out) && Some(c.i) != last_run
        })
        .map(|c| c.i)
        .collect()
}

/// The program name of a piece of shell text: leading `NAME=value` assignments
/// and arguments stripped.
pub fn bin_of(text: &str) -> String {
    let mut t = text.trim();
    while let Some(i) = t.find([' ', '\t']) {
        if i == 0 {
            t = t.trim_start();
            if t.is_empty() {
                break;
            }
            continue;
        }
        if !t[..i].contains('=') {
            break;
        }
        t = t[i..].trim_start();
    }
    let head = t.split([' ', '\t', '\n']).next().unwrap_or("");
    if head.contains('=') {
        // A bare assignment has no program name.
        String::new()
    } else {
        head.to_string()
    }
}

/// True if captured output reads like a failure even though the exit status
/// said otherwise.
pub fn looks_noisy(stdout: &str) -> bool {
    static ERROR_LINE: OnceLock<Regex> = OnceLock::new();
    let re = ERROR_LINE.get_or_init(|| {
        Regex::new(
            r"(?mi)^.*(?:\berror\s*:|\bERROR\b|Traceback \(most recent call last\)|\bpanic:|\bCannot (?:find|open|access|bind|start)|\bcommand not found\b|\bNo such file\b|\bFAILED\b|\bnot permitted\b|\bpermission denied\b|\bfatal:)",
        )
        .expect("static regex")
    });
    re.is_match(stdout)
}

fn is_zero(v: &usize) -> bool {
    *v == 0
}

fn is_false(v: &bool) -> bool {
    !*v
}
