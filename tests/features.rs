//! Tests for the v0.3 surface: strict mode, static checks, substitution
//! supervision, and the serve protocol.
//!
//! These run the binary. They need Linux for process groups and signals.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

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
    let value: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "not JSON: {e}\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (value, code)
}

fn records(report: &Value) -> &Vec<Value> {
    report["commands"].as_array().expect("commands array")
}

#[test]
fn strict_stops_at_the_first_failure() {
    let (report, code) = run_with("false; echo hi; true", &["--strict"]);

    assert_eq!(report["class"], "failed");
    assert_eq!(report["aborted"], true);
    assert_eq!(report["stopped_at"], 0);
    assert_eq!(report["exit"], 1);
    assert_eq!(code, 1);
    assert_eq!(records(&report).len(), 1, "nothing after the failure ran");
    assert_eq!(report["stdout"], "", "echo never ran");
}

#[test]
fn strict_leaves_short_circuit_idioms_alone() {
    let (report, _) = run_with("false || true; echo ok", &["--strict"]);
    assert_eq!(report["class"], "ok", "{report}");
    assert_eq!(report["stdout"], "ok\n");
}

#[cfg(unix)]
#[test]
fn strict_stops_on_a_masked_pipeline_stage() {
    let (report, _) = run_with("false | head -1; echo after", &["--strict"]);
    assert_eq!(report["class"], "stage_failed");
    assert_eq!(report["aborted"], true);
    assert_eq!(report["stdout"], "", "the masked failure stops the script");
}

#[test]
fn check_finds_dropped_statuses_without_running_anything() {
    let output = Command::new(BIN)
        .args(["--check", "-c", "cd /tmp; false | head -1; echo done"])
        .output()
        .expect("run agshell --check");

    let report: Value = serde_json::from_slice(&output.stdout).expect("check JSON");
    assert_eq!(report["ok"], false);

    let kinds: Vec<&str> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["kind"].as_str().unwrap_or(""))
        .collect();

    assert!(kinds.contains(&"dropped_status"), "{report}");
    assert!(kinds.contains(&"masked_stage"), "{report}");
}

#[test]
fn check_passes_a_clean_script() {
    let output = Command::new(BIN)
        .args(["--check", "-c", "echo hi && echo there"])
        .output()
        .expect("run agshell --check");

    let report: Value = serde_json::from_slice(&output.stdout).expect("check JSON");
    assert_eq!(report["ok"], true, "{report}");
}

#[test]
fn check_reports_a_parse_error() {
    let output = Command::new(BIN)
        .args(["--check", "-c", "if then"])
        .output()
        .expect("run agshell --check");

    let report: Value = serde_json::from_slice(&output.stdout).expect("check JSON");
    assert!(report["parse_error"].is_string(), "{report}");
}

#[test]
fn substitutions_are_observed_without_a_flag() {
    let report = run_with("x=$(echo hi); printf '[%s]' \"$x\"", &[]).0;

    assert_eq!(report["class"], "ok", "{report}");
    assert_eq!(report["stdout"], "[hi]");
    assert!(
        !report["opaque_substitution"].as_bool().unwrap_or(false),
        "the substitution was observed"
    );

    let substitution = records(&report)
        .iter()
        .find(|c| c["substitution_of"].is_number())
        .expect("a substitution record");
    assert_eq!(substitution["text"], "echo hi");
    assert_eq!(substitution["status"], 0);
}

#[test]
fn a_failing_substitution_reaches_the_class() {
    let (report, _) = run_with("echo $(false) done", &[]);

    assert_eq!(report["class"], "substitution_failed", "{report}");
    assert_eq!(report["exit"], 0, "bash-compatible: the echo succeeded");
    assert!(
        report["hidden_failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_u64().is_some()),
        "{report}"
    );
    assert_eq!(
        report["stdout"], "done
",
        "an empty substitution yields no field"
    );
}

#[test]
fn the_old_flag_is_still_accepted() {
    let (report, _) = run_with("echo $(false) done", &["--supervise-substitutions"]);
    assert_eq!(report["class"], "substitution_failed", "{report}");
}

#[test]
fn quoting_is_the_shells_own() {
    let report = run_with("printf '[%s]\n' $(printf 'a b')", &[]).0;

    // Unquoted output is split, as bash would.
    assert_eq!(report["stdout"], "[a]
[b]
", "{report}");
}

#[cfg(unix)]
#[test]
fn heredoc_substitutions_are_observed_too() {
    let report = run_with("cat <<EOF
$(false)
EOF", &[]).0;
    assert_eq!(report["class"], "substitution_failed", "{report}");
    assert!(
        !report["opaque_substitution"].as_bool().unwrap_or(false),
        "{report}"
    );
}

#[test]
fn a_substitution_in_a_pipeline_stage_is_still_opaque() {
    let report = run_with("echo $(false) | cat", &[]).0;
    assert_eq!(report["opaque_substitution"], true, "{report}");
}

/// A harness-shaped run: state persists, snapshots rewind it, branches are
/// isolated.
#[cfg(unix)]
#[test]
fn serve_keeps_state_snapshots_and_branches() {
    let mut child = Command::new(BIN)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve");

    let requests = [
        json!({"id": 1, "op": "run", "script": "x=41; cd /tmp"}),
        json!({"id": 2, "op": "run", "script": "echo $((x+1)) in $PWD"}),
        json!({"id": 3, "op": "snapshot", "key": "before"}),
        json!({"id": 4, "op": "run", "script": "x=99; cd /"}),
        json!({"id": 5, "op": "restore", "snapshot": "before"}),
        json!({"id": 6, "op": "run", "script": "echo $x in $PWD"}),
        json!({"id": 7, "op": "branch", "scripts": ["x=1; echo one", "x=2; echo two"]}),
        json!({"id": 8, "op": "run", "script": "echo $x"}),
        json!({"id": 9, "op": "state"}),
        json!({"id": 10, "op": "shutdown"}),
    ];

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for request in &requests {
            writeln!(stdin, "{request}").expect("write request");
        }
    }
    child.stdin.take();

    let mut stdout = String::new();
    child
        .stdout
        .as_mut()
        .expect("stdout")
        .read_to_string(&mut stdout)
        .expect("read responses");
    let status = child.wait().expect("wait");
    assert!(status.success(), "serve exited: {status}");

    let responses: Vec<Value> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("response JSON"))
        .collect();
    assert_eq!(responses.len(), requests.len(), "{stdout}");

    for response in &responses {
        assert_eq!(response["ok"], true, "{response}");
    }

    assert_eq!(responses[1]["report"]["stdout"], "42 in /tmp\n");
    assert_eq!(
        responses[5]["report"]["stdout"], "41 in /tmp\n",
        "restore rewound x and cwd"
    );

    let branches = responses[6]["reports"].as_array().expect("branch reports");
    assert_eq!(branches.len(), 2);
    assert_eq!(branches[0]["stdout"], "one\n");
    assert_eq!(branches[1]["stdout"], "two\n");

    assert_eq!(
        responses[7]["report"]["stdout"], "41\n",
        "a branch's assignment must not leak into the session"
    );
    assert_eq!(responses[8]["state"]["cwd"], "/tmp");
}

#[test]
fn serve_answers_errors_without_dying() {
    let mut child = Command::new(BIN)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn serve");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "not json").unwrap();
        writeln!(stdin, r#"{{"id":1,"op":"nope"}}"#).unwrap();
        writeln!(stdin, r#"{{"id":2,"op":"restore","snapshot":"missing"}}"#).unwrap();
        writeln!(stdin, r#"{{"id":3,"op":"ping"}}"#).unwrap();
        writeln!(stdin, r#"{{"id":4,"op":"shutdown"}}"#).unwrap();
    }
    child.stdin.take();

    let mut stdout = String::new();
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());

    let responses: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("response JSON"))
        .collect();

    assert_eq!(responses.len(), 5, "{stdout}");
    assert_eq!(responses[0]["ok"], false);
    assert_eq!(responses[1]["ok"], false);
    assert_eq!(responses[2]["ok"], false);
    assert_eq!(responses[3]["ok"], true);
}

