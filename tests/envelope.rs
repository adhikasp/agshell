//! End-to-end tests: run the binary and read the envelope it prints.
//!
//! These are behavioural tests of the contract in the README, not unit tests
//! of the walker. They need Linux (process groups and signals); on other
//! platforms the timeout test is skipped.

use std::process::Command;

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_agshell");

fn run_with(script: &str, extra: &[&str]) -> (Value, i32) {
    let output = Command::new(BIN)
        .arg("-c")
        .arg(script)
        .args(extra)
        .output()
        .expect("run agshell");

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "not an envelope: {e}\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (envelope, code)
}

fn run(script: &str) -> Value {
    run_with(script, &[]).0
}

fn status(envelope: &Value, index: usize) -> i64 {
    envelope["commands"][index]["status"]
        .as_i64()
        .unwrap_or_else(|| panic!("no status at {index}: {envelope}"))
}

#[test]
fn chain_keeps_hidden_failure() {
    let r = run("false; echo hi; true");

    assert_eq!(r["exit"], 0, "exit stays bash-compatible");
    assert_eq!(r["class"], "chain_partial");
    assert_eq!(r["hidden_failures"], serde_json::json!([0]));
    assert_eq!(r["commands"].as_array().unwrap().len(), 3);
    assert_eq!(status(&r, 0), 1);
    assert_eq!(status(&r, 1), 0);
    assert_eq!(status(&r, 2), 0);
    assert_eq!(r["stdout"], "hi\n");
}

#[cfg(unix)]
#[test]
fn pipeline_stages() {
    let r = run("false | head -1");

    assert_eq!(r["exit"], 0, "bash would say 0 without pipefail");
    assert_eq!(r["class"], "stage_failed");

    let commands = r["commands"].as_array().unwrap();
    assert_eq!(commands.len(), 2, "one record per stage");
    assert_eq!(status(&r, 0), 1, "the failing stage is not hidden");
    assert_eq!(status(&r, 1), 0);
    assert_eq!(commands[0]["pipeline"], commands[1]["pipeline"]);
    assert_eq!(commands[0]["stage"], 0);
    assert_eq!(commands[1]["stage"], 1);
}

#[cfg(unix)]
#[test]
fn pipeline_data_flows() {
    let r = run("printf 'b\\na\\nc\\n' | sort | head -2");

    assert_eq!(r["class"], "ok");
    assert_eq!(r["stdout"], "a\nb\n");
    assert_eq!(r["commands"].as_array().unwrap().len(), 3);
}

#[test]
fn and_or_short_circuits() {
    let r = run("true && echo yes || echo no");

    assert_eq!(r["class"], "ok");
    assert_eq!(r["stdout"], "yes\n");

    let commands = r["commands"].as_array().unwrap();
    assert_eq!(commands.len(), 3, "two run, one recorded as skipped");
    assert_eq!(commands[2]["skipped"], true);
    assert_eq!(status(&r, 2), -1);

    let r = run("false && echo yes");
    assert_eq!(r["exit"], 1);
    assert_eq!(r["class"], "failed");
    assert!(
        r["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["skipped"] == true),
        "the && right side is recorded as skipped"
    );
}

#[test]
fn explicit_failure_is_failed() {
    let r = run("exit 3");
    assert_eq!(r["class"], "failed");
    assert_eq!(r["exit"], 3);
}

#[test]
fn ok_noisy() {
    let r = run("printf 'fatal: changed nothing\\n'");
    assert_eq!(r["exit"], 0);
    assert_eq!(r["class"], "ok_noisy");
}

#[test]
fn a_substitution_in_a_pipeline_stage_is_marked_opaque() {
    let r = run("echo $(false) | cat");
    assert_eq!(r["opaque_substitution"], true);
}

#[test]
fn a_substitution_in_a_simple_command_is_not() {
    let r = run("echo $(false) done");
    assert!(!r["opaque_substitution"].as_bool().unwrap_or(false), "{r}");
    assert_eq!(r["class"], "substitution_failed");
}

#[test]
fn parse_error() {
    let r = run("if then");
    assert_eq!(r["class"], "parse_error");
    assert_eq!(r["exit"], 2);
}

#[test]
fn stderr_is_captured() {
    let r = run("echo oops >&2");
    assert!(
        r["commands"][0]["stderr"]
            .as_str()
            .unwrap_or_default()
            .contains("oops"),
        "stderr should be attached to the command: {r}"
    );
}

/// The rewrite embeds the shell instead of forking one per command, so state
/// that bash keeps in the shell has to keep working across statements.
#[cfg(unix)]
#[test]
fn shell_state_persists_across_statements() {
    let r = run("x=41; cd /tmp; y=$((x + 1)); echo \"$y in $PWD\"; printf '%s\\n' \"$0\"");
    assert_eq!(r["class"], "ok", "{r}");
    assert_eq!(r["stdout"], "42 in /tmp\nagshell\n");
}

#[test]
fn function_and_loop_are_delegated() {
    let r = run("greet() { echo hello \"$1\"; }; greet world");
    assert_eq!(r["class"], "ok");
    assert_eq!(r["stdout"], "hello world\n");

    let r = run("for i in 1 2 3; do echo $i; done");
    assert_eq!(r["class"], "ok");
    assert_eq!(r["stdout"], "1\n2\n3\n");
}

#[test]
fn a_failing_command_still_reports_stderr_and_status() {
    let r = run("cd /definitely/not/here");
    assert_eq!(r["class"], "failed");
    assert_eq!(r["exit"], 1);
    assert!(
        !r["commands"][0]["stderr"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "the shell's own error is captured: {r}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn timeout_kills_the_command() {
    let (r, code) = run_with("sleep 30", &["--timeout", "300ms"]);

    assert_eq!(r["class"], "timeout");
    assert_eq!(r["commands"][0]["timed_out"], true);
    assert_eq!(code, 137);
}

#[cfg(target_os = "linux")]
#[test]
fn timeout_kills_a_whole_pipeline() {
    let (r, _) = run_with("sleep 30 | cat", &["--timeout", "300ms"]);
    assert_eq!(r["class"], "timeout");
    assert_eq!(r["commands"].as_array().unwrap().len(), 2);
}

