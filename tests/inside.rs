//! Statements the shell runs whole (`if`, loops, `case`, subshells, functions,
//! `eval`) are observed from the inside, through the shell's own traps.

use agshell::{Options, Report, Session};

async fn run(script: &str) -> Report {
    run_with(script, Options::default()).await
}

async fn run_with(script: &str, opts: Options) -> Report {
    Session::new(opts).await.expect("session").run(script).await
}

fn texts(report: &Report) -> Vec<&str> {
    report.commands.iter().map(|c| c.text.as_str()).collect()
}

#[tokio::test]
async fn failure_inside_an_if_body_is_not_hidden() {
    let r = run("if true; then false; echo in; fi").await;
    assert_eq!(r.exit, 0, "{r:?}");
    assert_eq!(r.class, "chain_partial", "{r:?}");

    let failed = r.commands.iter().find(|c| c.text == "false").expect("false");
    assert_eq!(failed.status, 1);
    assert!(failed.inside.is_some(), "{failed:?}");
    assert_eq!(r.hidden_failures, vec![failed.i]);

    // The statement itself is still a record, and the inner commands point at it.
    let parent = &r.commands[failed.inside.unwrap()];
    assert!(parent.delegated, "{parent:?}");
    assert!(parent.text.starts_with("if "));
}

#[tokio::test]
async fn a_failed_condition_is_handled_not_hidden() {
    let r = run("if false; then echo no; fi; echo ok").await;
    assert_eq!(r.class, "ok", "{r:?}");
    assert!(r.hidden_failures.is_empty(), "{r:?}");
    assert!(texts(&r).contains(&"false"), "{:?}", texts(&r));
}

#[tokio::test]
async fn a_failure_consumed_by_or_is_handled() {
    let r = run("for i in 1 2; do test $i = 1 || echo no; done").await;
    assert_eq!(r.class, "ok", "{r:?}");
    let test2 = r.commands.iter().find(|c| c.text == "test 2 = 1").expect("test");
    assert_eq!(test2.status, 1);
    assert!(test2.handled);
}

#[tokio::test]
async fn function_bodies_are_seen_and_the_call_is_not_duplicated() {
    let r = run("f(){ false; echo x; }; f").await;
    assert_eq!(r.class, "chain_partial", "{r:?}");
    assert_eq!(r.commands.iter().filter(|c| c.text == "f").count(), 1, "{:?}", texts(&r));
    let failed = r.commands.iter().find(|c| c.text == "false").expect("false");
    assert_eq!(r.commands[failed.inside.unwrap()].text, "f");
}

#[tokio::test]
async fn subshell_internals_are_seen() {
    let r = run("(false; echo sub)").await;
    assert_eq!(r.class, "chain_partial", "{r:?}");
    assert!(r.hidden_failures.iter().any(|&i| r.commands[i].text == "false"));
}

#[tokio::test]
async fn eval_is_seen_into() {
    let r = run(r#"eval "false; echo e""#).await;
    assert_eq!(r.class, "chain_partial", "{r:?}");
    assert!(texts(&r).contains(&"echo e"));
}

#[tokio::test]
async fn a_compound_keeps_its_own_status() {
    // The loop's status is the last command's; the trap handlers must not
    // disturb it.
    let r = run("for i in 1; do false; done").await;
    assert_eq!(r.exit, 1, "{r:?}");
    assert_eq!(r.class, "failed");
    let parent = r.commands.iter().find(|c| c.delegated).expect("loop");
    assert_eq!(parent.status, 1);
}

#[tokio::test]
async fn the_hooks_own_commands_never_appear() {
    let r = run("for i in 1 2; do false; done; true").await;
    assert!(
        r.commands.iter().all(|c| !c.text.contains("__agshell")),
        "{:?}",
        texts(&r)
    );
}

#[tokio::test]
async fn expanded_words_are_what_is_reported() {
    let r = run("for i in 7; do echo item $i; done").await;
    assert!(texts(&r).contains(&"echo item 7"), "{:?}", texts(&r));
}

#[tokio::test]
async fn substitution_failure_inside_a_body_is_seen() {
    let r = run(r#"if true; then echo "$(false)"; fi"#).await;
    assert_eq!(r.exit, 0);
    assert_ne!(r.class, "ok", "{r:?}");
}

#[tokio::test]
async fn strict_stops_after_a_statement_with_an_unhandled_inner_failure() {
    let opts = Options {
        strict: true,
        ..Options::default()
    };
    let r = run_with("for i in 1 2; do false; echo body; done; echo after", opts).await;
    assert!(r.aborted, "{r:?}");
    assert_eq!(r.exit, 1);
    assert!(!r.stdout.contains("after"), "{r:?}");
}

#[tokio::test]
async fn strict_leaves_handled_inner_failures_alone() {
    let opts = Options {
        strict: true,
        ..Options::default()
    };
    let r = run_with("if false; then :; fi; echo after", opts).await;
    assert!(!r.aborted, "{r:?}");
    assert!(r.stdout.contains("after"));
}

#[tokio::test]
async fn the_users_own_traps_survive_a_traced_statement() {
    let mut session = Session::new(Options::default()).await.expect("session");
    session.run("trap 'echo bye' EXIT").await;
    session.run("for i in 1; do :; done").await;
    let r = session.run("trap -p EXIT").await;
    assert!(r.stdout.contains("echo bye"), "{r:?}");
    let r = session.run("trap -p DEBUG; trap -p ERR").await;
    assert!(!r.stdout.contains("agshell"), "{r:?}");
}

#[tokio::test]
async fn branches_trace_independently() {
    let session = Session::new(Options::default()).await.expect("session");
    let reports = session
        .branch(&[
            "for i in 1; do false; echo a; done".to_string(),
            "for i in 1; do true; echo b; done".to_string(),
        ])
        .await;
    assert_eq!(reports[0].class, "chain_partial", "{:?}", reports[0]);
    assert_eq!(reports[1].class, "ok", "{:?}", reports[1]);
}

#[tokio::test]
async fn a_long_loop_does_not_flood_the_envelope_but_failures_survive() {
    let r = run("for i in {1..1000}; do :; done; for i in 1 2 3; do false; done").await;
    assert!(r.commands.len() < 400, "{}", r.commands.len());
    assert!(r.inner_omitted > 0, "{r:?}");
    // Every unhandled failure is still there.
    assert_eq!(r.commands.iter().filter(|c| c.text == "false" && !c.handled).count(), 3);
}

#[tokio::test]
async fn a_failed_test_command_is_not_pinned_on_its_neighbour() {
    let r = run("for i in 1; do echo a; [[ -f /no/such/file ]]; echo b; done").await;
    assert_eq!(r.class, "chain_partial", "{r:?}");
    let echo_a = r.commands.iter().find(|c| c.text == "echo a").expect("echo a");
    assert_eq!(echo_a.status, 0, "{echo_a:?}");
    let failure = r.commands.iter().find(|c| c.text.starts_with("(failure")).expect("failure");
    assert_eq!(failure.status, 1);
    assert!(failure.text.contains("echo a"));
}

#[tokio::test]
async fn arithmetic_expansion_does_not_make_a_statement_ambiguous() {
    let r = run("for i in 1; do false; echo $((i+1)); done").await;
    let f = r.commands.iter().find(|c| c.text == "false").expect("false");
    assert_eq!(f.status, 1);
    assert!(!f.handled);
}

#[tokio::test]
async fn a_function_with_a_silent_test_is_treated_as_ambiguous_too() {
    let r = run("g(){ echo a; [[ -f /no/such/file ]]; }; for i in 1; do g; done").await;
    let echo_a = r.commands.iter().find(|c| c.text == "echo a").expect("echo a");
    assert_eq!(echo_a.status, 0, "{r:?}");
    assert!(r.commands.iter().any(|c| c.text.starts_with("(failure")));
}
