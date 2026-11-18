//! Cross-platform compatibility: a shape has to mean the same thing here as in
//! the shell a person would type it into, on whichever platform they type it.
//!
//! Three layers, each skipping quietly when the platform lacks the tool:
//!
//! - [`differential_against_bash`]: the same corpus through agshell and through
//!   the system `bash`, compared on stdout and exit status. Runs on Linux and
//!   macOS, and on Windows wherever Git for Windows puts a `bash` on `PATH`.
//! - [`a_real_pwsh_roundtrip_survives_the_shell`] and its neighbours: Windows
//!   only, skipped when `pwsh` is not installed. These are the round trips whose
//!   absence let a mangled `-Command` argument look like a PowerShell bug.
//! - [`fuzz_the_linter_never_hangs_or_panics`] and [`quoting_decides_the_verdict`]:
//!   deterministic junk through the linter, so no shape can hang it and no
//!   quoting can be misread. Raise the round count with `AGSHELL_FUZZ_ROUNDS`.

use std::process::Command;
use std::time::{Duration, Instant};

use agshell::{CheckReport, Options, Session};
use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_agshell");

/// Runs the built binary. `AGSHELL_SESSION` is stripped: it is set in an agent's
/// shell, and a test that inherited it would hand every call to one shared daemon
/// instead of running the script here.
fn agshell(script: &str) -> (Value, i32) {
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

fn check(script: &str) -> CheckReport {
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

fn kinds(report: &CheckReport) -> Vec<&str> {
    report.findings.iter().map(|f| f.kind.as_str()).collect()
}

/// A `bash` that actually runs. On Windows, `bash.exe` on `PATH` is usually the
/// WSL stub, which fails when no distro is installed; Git for Windows ships a
/// real one.
fn bash_program() -> Option<String> {
    const CANDIDATES: &[&str] = &[
        "bash",
        "C:/Program Files/Git/bin/bash.exe",
        "C:/Program Files/Git/usr/bin/bash.exe",
        "/bin/bash",
        "/usr/bin/bash",
    ];

    for candidate in CANDIDATES {
        let Ok(output) = Command::new(candidate).args(["-c", "printf ok"]).output() else {
            continue;
        };
        if output.status.success() && output.stdout == b"ok" {
            return Some((*candidate).to_string());
        }
    }

    None
}

fn run_bash(program: &str, script: &str) -> (String, i32) {
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

/// Ordinary quoting and expansion, plus the shapes that broke a real session.
/// Everything is builtins only, so the corpus means the same thing on every
/// platform's bash.
const CORPUS: &[&str] = &[
    r#"x=7; printf '%s\n' "$x ${x} $((x*2))""#,
    r#"printf '%s\n' 'single $HOME'"#,
    r#"printf '%s\n' "escaped \$x""#,
    r#"printf '%s\n' "quote \" inside""#,
    r#"printf '%s\n' 'it'\''s'"#,
    r#"printf '%s\n' "a $(printf b) c""#,
    r#"printf '%s\n' "nested $(printf '%s' 'x $(y)')""#,
    r#"printf '%s\n' "backtick `printf c`""#,
    r#"printf '%s\n' "${undefined:-fallback}""#,
    r#"printf '%s\n' "$@"  # no arguments"#,
    r#"false; printf 'after:%s\n' $?"#,
    r#"false || printf 'or\n'"#,
    r#"true && printf 'and\n'"#,
    r#"for i in 1 2 3; do printf '%s\n' "$i"; done"#,
    r#"case 2 in 1) printf 'one\n';; 2) printf 'two\n';; esac"#,
    r#"f() { printf '%s\n' "$1"; }; f 'arg one'"#,
    r#"if false; then printf 'no\n'; else printf 'yes\n'; fi"#,
    r#"printf 'x\n' | while read -r line; do printf '[%s]\n' "$line"; done"#,
    r#"printf '%s\n' "[$(printf 'a b')]""#,
    r#"printf '%s\n' 'pwsh -Command "$x"'"#,
];

/// agshell must not change what a script prints or what it exits with.
#[test]
fn differential_against_bash() {
    let Some(program) = bash_program() else {
        eprintln!("no working `bash` on this machine: skipping the differential corpus");
        return;
    };

    let mut compared = 0;

    for script in CORPUS {
        let (expected_stdout, expected_code) = run_bash(&program, script);

        let (report, code) = agshell(script);
        assert_eq!(
            report["stdout"], expected_stdout,
            "{script:?}: agshell printed {:?}, bash printed {expected_stdout:?}",
            report["stdout"]
        );
        assert_eq!(
            code, expected_code,
            "{script:?}: agshell exited {code}, bash exited {expected_code} ({report})"
        );
        compared += 1;
    }

    assert!(compared >= CORPUS.len(), "every corpus entry was compared");
}

/// A script passed through single quotes reaches PowerShell untouched, whatever
/// it contains — the fix for the mangled `-Command` case.
#[test]
fn the_linter_never_flags_a_single_quoted_script() {
    for script in [
        r#"pwsh -NoProfile -Command 'Get-Item $_.Name'"#,
        r#"pwsh -NoProfile -Command 'Get-CimInstance X | Select-Object @{n="GB";e={$_.Capacity/1GB}}'"#,
        r#"powershell.exe -c '$env:PATH; `x`'"#,
    ] {
        let report = check(script);
        assert!(
            !kinds(&report).contains(&"expanded_before_interpreter"),
            "{script:?} was flagged: {report:?}"
        );
    }
}

/// The double-quoted spelling of the same script is flagged, with the argument
/// quoted back so the caller can see what to change.
#[test]
fn the_linter_flags_the_double_quoted_script() {
    let report = check(r#"pwsh -NoProfile -Command "Get-CimInstance X | Select-Object @{e={$_.Capacity/1GB}}""#);

    let finding = report
        .findings
        .iter()
        .find(|f| f.kind == "expanded_before_interpreter")
        .unwrap_or_else(|| panic!("no finding: {report:?}"));

    assert!(finding.text.starts_with('"'), "{finding:?}");
    assert!(finding.text.contains("$_"), "{finding:?}");
    assert!(finding.detail.contains("pwsh"), "{finding:?}");
    assert!(finding.detail.contains("EncodedCommand"), "{finding:?}");
}

/// Static analysis is platform-free: this runs everywhere, so a Windows-shaped
/// script is still checked on Linux CI.
#[test]
fn the_linter_reads_the_windows_shapes_everywhere() {
    let cases: &[(&str, bool)] = &[
        (r#"pwsh -Command "$env:PATH""#, true),
        (r#"pwsh.exe -Command "$env:PATH""#, true),
        (r#""C:/Program Files/PowerShell/7/pwsh.exe" -Command "$env:PATH""#, true),
        (r#"NO_COLOR=1 pwsh -Command "$env:PATH""#, true),
        (r#"pwsh -File build.ps1"#, false),
        (r#"pwsh -EncodedCommand RQBjAGgAbwA="#, false),
        (r#"cmd /c "echo %PATH%""#, false),
        (r#"powershell -Command 'Write-Output $env:PATH'"#, false),
    ];

    for (script, expected) in cases {
        let flagged = kinds(&check(script)).contains(&"expanded_before_interpreter");
        assert_eq!(flagged, *expected, "{script:?}: flagged={flagged} ({expected})");
    }
}

#[cfg(windows)]
fn pwsh_available() -> bool {
    Command::new("pwsh")
        .args(["-NoProfile", "-Command", "exit 0"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// The round trip that started this: single quotes in, PowerShell's answer out.
#[cfg(windows)]
#[test]
fn a_real_pwsh_roundtrip_survives_the_shell() {
    if !pwsh_available() {
        eprintln!("no `pwsh` on PATH: skipping the PowerShell round trip");
        return;
    }

    let (report, code) = agshell(r#"pwsh -NoProfile -Command 'Write-Output (1 + 1)'"#);
    assert_eq!(code, 0, "{report}");
    assert_eq!(report["stdout"].as_str().map(str::trim), Some("2"), "{report}");

    // `$_` is PowerShell's here, not this shell's last-argument variable.
    let (report, code) = agshell(r#"pwsh -NoProfile -Command '"x" | ForEach-Object { $_.ToUpper() }'"#);
    assert_eq!(code, 0, "{report}");
    assert_eq!(report["stdout"].as_str().map(str::trim), Some("X"), "{report}");
}

/// The failing shape reports the parse error and says why, in one hint.
#[cfg(windows)]
#[test]
fn a_mangled_pwsh_script_comes_back_with_a_hint() {
    if !pwsh_available() {
        eprintln!("no `pwsh` on PATH: skipping the mangled-script test");
        return;
    }

    // `$_` is empty in a fresh shell, so pwsh is handed `Round(.Capacity/1GB,0)`.
    let (report, code) =
        agshell(r#"pwsh -NoProfile -Command "Write-Output ([math]::Round($_.Capacity/1GB,0))""#);

    assert_ne!(code, 0, "the mangled script must not look like success: {report}");

    let stderr = report["commands"][0]["stderr"].as_str().unwrap_or("");
    assert!(stderr.contains("ParserError"), "{report}");
    assert!(stderr.contains("hint:"), "{report}");
    assert!(stderr.contains("Single-quote"), "{report}");
    assert!(stderr.contains("EncodedCommand"), "{report}");
}

const JUNK: &[&str] = &[
    "pwsh", "powershell.exe", "-Command", "-c", "-File", "-EncodedCommand", "\"", "'", "$", "$_",
    "$env:X", "`", "(", ")", "{", "}", "@", " ", "  ", "\n", "\\", "|", "&&", ";", "||", "Get-Item",
    "echo", "&", "command", "X=1", "-NoProfile", "a", "=", ">", "2>&1", "if", "then", "fi", "for",
    "do", "done", "case", "esac", "$(printf x)", "Write-Output",
];

fn rounds() -> usize {
    std::env::var("AGSHELL_FUZZ_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600)
}

/// A deterministic xorshift: the corpus is the same on every run, and a failure
/// is reproducible from its round number.
fn junk_source(next: &mut impl FnMut() -> u64) -> String {
    let len = (next() % 12) as usize;
    let mut source = String::new();
    for _ in 0..len {
        source.push_str(JUNK[(next() % JUNK.len() as u64) as usize]);
    }
    source
}

fn xorshift(seed: u64) -> impl FnMut() -> u64 {
    let mut state = seed;
    move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    }
}

/// Random junk through the linter: it must answer, answer the same way twice,
/// and never report a shape it promises not to report.
#[tokio::test]
async fn fuzz_the_linter_never_hangs_or_panics() {
    let session = Session::new(Options::default()).await.expect("session");
    let mut next = xorshift(0x243f_6a88_85a3_08d3);
    let started = Instant::now();

    for round in 0..rounds() {
        let source = junk_source(&mut next);

        let first = session.check(&source);
        let second = session.check(&source);
        assert_eq!(
            kinds(&first),
            kinds(&second),
            "round {round}: not deterministic for {source:?}"
        );

        for finding in &first.findings {
            assert!(!finding.detail.is_empty(), "round {round}: {source:?}");
            if finding.kind == "expanded_before_interpreter" {
                assert!(
                    finding.text.starts_with('"'),
                    "round {round}: not a quoted argument: {source:?} → {finding:?}"
                );
                assert!(
                    finding.text.contains('$') || finding.text.contains('`'),
                    "round {round}: nothing to expand: {source:?} → {finding:?}"
                );
            }
        }
    }

    assert!(
        started.elapsed() < Duration::from_secs(60),
        "the linter took {:?} for {} rounds — it is meant to be instant",
        started.elapsed(),
        rounds()
    );
}

/// The oracle the fuzz above cannot state: quoting alone decides this finding.
#[tokio::test]
async fn quoting_decides_the_verdict() {
    let session = Session::new(Options::default()).await.expect("session");

    for body in [
        "Get-Item .",
        "Write-Output $_.Name",
        "$env:PATH",
        "`git rev-parse HEAD`",
        "Write-Output plain",
    ] {
        let single = format!("pwsh -NoProfile -Command '{body}'");
        assert!(
            !kinds(&session.check(&single)).contains(&"expanded_before_interpreter"),
            "single quotes must never be flagged: {single}"
        );

        let double = format!(r#"pwsh -NoProfile -Command "{body}""#);
        let expected = body.contains('$') || body.contains('`');
        assert_eq!(
            kinds(&session.check(&double)).contains(&"expanded_before_interpreter"),
            expected,
            "{double}"
        );
    }
}
