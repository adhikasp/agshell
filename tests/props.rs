//! Property tests: generated input instead of a fixed corpus, and shrinking, so
//! a failure comes back as the smallest script that shows it.
//!
//! Three properties, in the order of how much they cost:
//!
//! - [`quoting_decides_the_scanner`] — the pure rule, no shell, thousands of cases.
//! - [`the_linter_agrees_with_the_scanner`] — the two layers must never disagree.
//! - [`the_linter_never_panics_on_arbitrary_text`] — the parser surface, arbitrary text.
//! - [`generated_scripts_agree_with_bash`] — the compatibility oracle: generated
//!   *valid* scripts, run through agshell and through a real bash, compared on
//!   stdout and exit status.
//!
//! `PROPTEST_CASES=1000 cargo test --release --test props` for a longer pass.

use agshell::{Options, Session};
use proptest::prelude::*;

mod common;
use common::{agshell, bash_program, kinds, run_bash};

/// A session to lint with. The runtime is kept alive by the caller because the
/// shell may hold handles from it.
fn session() -> (tokio::runtime::Runtime, Session) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let session = runtime
        .block_on(Session::new(Options::default()))
        .expect("session");
    (runtime, session)
}

/// A PowerShell body a person would actually put in `-Command`, split by whether
/// this shell has something to expand in it. No quote characters: the quoting
/// under test is the caller's.
fn body() -> impl Strategy<Value = String> {
    prop::sample::select(vec![
        // something to expand
        "$_.Name",
        "$env:PATH",
        "$(Get-Date)",
        "`git rev-parse HEAD`",
        // nothing to expand
        "Get-Item .",
        "Write-Output plain",
        "Get-ChildItem -Path .",
        "-NoProfile",
    ])
    .prop_map(str::to_string)
}

fn powershell_script(quote: char, body: &str) -> String {
    format!("pwsh -NoProfile -Command {quote}{body}{quote}")
}

/// The rule the scanner exists for, at the level of the rule itself.
#[test]
fn quoting_decides_the_scanner() {
    proptest!(|(body in body(), double in any::<bool>())| {
        let quote = if double { '"' } else { '\'' };
        let source = powershell_script(quote, &body);
        let flagged = agshell::scan::foreign_script_expansion(&source).is_some();
        let expandable = body.contains('$') || body.contains('`');

        prop_assert_eq!(
            flagged,
            double && expandable,
            "scanner disagreed with the rule for {}",
            source
        );
    });
}

/// The linter reads the parser's rendering of the command, not the source. It
/// must still come to the same verdict as the scanner on the source.
#[test]
fn the_linter_agrees_with_the_scanner() {
    let (_runtime, session) = session();

    proptest!(|(body in body(), double in any::<bool>())| {
        let quote = if double { '"' } else { '\'' };
        let source = powershell_script(quote, &body);

        let scanner = agshell::scan::foreign_script_expansion(&source).is_some();
        let linter = kinds(&session.check(&source)).contains(&"expanded_before_interpreter");

        prop_assert_eq!(linter, scanner, "layers disagreed for {}", source);
    });
}

/// Arbitrary text into the parser and the linter: no panic, no hang, the same
/// answer twice, and a finding of ours always well-formed.
#[test]
fn the_linter_never_panics_on_arbitrary_text() {
    let (_runtime, session) = session();
    let text = proptest::string::string_regex("(?s).{0,200}").expect("regex strategy");

    proptest!(|(source in text)| {
        let first = session.check(&source);
        let second = session.check(&source);
        prop_assert_eq!(kinds(&first), kinds(&second), "not deterministic for {:?}", source);

        for finding in &first.findings {
            prop_assert!(!finding.detail.is_empty(), "{:?}: {:?}", source, finding);
            if finding.kind == "expanded_before_interpreter" {
                prop_assert!(finding.text.starts_with('"'), "{:?}: {:?}", source, finding);
                prop_assert!(
                    finding.text.contains('$') || finding.text.contains('`'),
                    "{:?}: {:?}",
                    source,
                    finding
                );
            }
        }
    });
}

/// A word as a person writes one: bare, single-quoted (literal here) or
/// double-quoted (expanded by this shell). Every one is valid in a `printf`
/// argument list, so the generated script always parses.
fn word() -> impl Strategy<Value = String> {
    prop_oneof![
        prop::sample::select(vec!["a", "abc", "$x", "${x}", "$((1+2))", "x=y"]).prop_map(str::to_string),
        prop::sample::select(vec![r#"'a b'"#, r#"'$x'"#, r#"'bt `printf c`'"#, r#"'it'\''s'"#])
            .prop_map(str::to_string),
        prop::sample::select(vec![
            r#""a b""#,
            r#""$x""#,
            r#""${x} tail""#,
            r#""a $(printf b) c""#,
            r#""bt `printf c`""#,
            r#""esc \$x""#,
            r#""q \"q\"""#,
        ])
        .prop_map(str::to_string),
    ]
}

/// A small but always-valid script: an assignment, three words through `printf`,
/// and one short-circuit. Nothing here depends on the clock, the user, the cwd
/// or a random seed, so both shells must agree on every run.
fn script() -> impl Strategy<Value = String> {
    (word(), word(), word(), any::<bool>()).prop_map(|(a, b, c, tail)| {
        let mut source = format!("x=7; printf '[%s][%s][%s]\\n' {a} {b} {c}; ");
        source.push_str(if tail {
            "true && printf 'and\\n'"
        } else {
            "false || printf 'or\\n'"
        });
        source
    })
}

/// The compatibility oracle. Skipped, loudly, when this machine has no bash.
#[test]
fn generated_scripts_agree_with_bash() {
    let Some(program) = bash_program() else {
        eprintln!("no working `bash` on this machine: skipping the generated scripts");
        return;
    };

    let config = ProptestConfig {
        cases: 48,
        ..ProptestConfig::default()
    };

    proptest!(config, |(source in script())| {
        let (expected_stdout, expected_code) = run_bash(&program, &source);
        let (report, code) = agshell(&source);

        prop_assert_eq!(
            report["stdout"].as_str().unwrap_or(""),
            expected_stdout.as_str(),
            "printed differently: {}",
            source
        );
        prop_assert_eq!(code, expected_code, "exited differently: {}", source);
    });
}
