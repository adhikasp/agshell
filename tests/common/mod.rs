//! Helpers shared by the end-to-end test files. `tests/common/mod.rs` is a
//! module, not a test binary, so nothing here runs on its own.
//!
//! Each test binary compiles this module for itself, so most of it is unused in
//! any one of them.

#![allow(dead_code)]

use std::process::Command;

use agshell::CheckReport;
use serde_json::Value;

pub const BIN: &str = env!("CARGO_BIN_EXE_agshell");

/// Runs the built binary and returns its envelope. `AGSHELL_SESSION` is stripped:
/// it is set in an agent's shell, and a test that inherited it would hand every
/// call to one shared daemon instead of running the script here.
pub fn agshell(script: &str) -> (Value, i32) {
    let output = Command::new(BIN)
        .env_remove("AGSHELL_SESSION")
        .arg("-c")
        .arg(script)
        .output()
        .expect("run agshell");

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "not an envelope: {e}\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (report, code)
}

pub fn check(script: &str) -> CheckReport {
    let output = Command::new(BIN)
        .env_remove("AGSHELL_SESSION")
        .args(["--check", "-c", script])
        .output()
        .expect("run agshell --check");

    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "not a check report: {e}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

pub fn kinds(report: &CheckReport) -> Vec<&str> {
    report.findings.iter().map(|f| f.kind.as_str()).collect()
}

/// A `bash` that actually runs. On Windows, `bash.exe` on `PATH` is usually the
/// WSL stub, which fails when no distro is installed — and can be worse than
/// failing: it answers a trivial probe and then runs a script differently
/// (`x=7; printf "$x"` printed empty, 2026-11-18). Probe with an assignment, an
/// expansion and arithmetic, so only a real bash passes.
pub fn bash_program() -> Option<String> {
    const CANDIDATES: &[&str] = &[
        "bash",
        "C:/Program Files/Git/bin/bash.exe",
        "C:/Program Files/Git/usr/bin/bash.exe",
        "/bin/bash",
        "/usr/bin/bash",
    ];

    for candidate in CANDIDATES {
        let Ok(output) = Command::new(candidate)
            .args(["-c", r#"x=7; printf '%s' "$x $((x*2)) ${x:-none}""#])
            .output()
        else {
            continue;
        };
        if output.status.success() && output.stdout == b"7 14 7" {
            return Some((*candidate).to_string());
        }
    }

    None
}

pub fn run_bash(program: &str, script: &str) -> (String, i32) {
    let output = Command::new(program)
        .arg("-c")
        .arg(script)
        .output()
        .expect("run bash");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        output.status.code().unwrap_or(-1),
    )
}
