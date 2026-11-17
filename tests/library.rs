//! The crate surface: a session that keeps state, snapshots that rewind it,
//! and branches that do not leak into each other.

use agshell::{Options, Session};

#[cfg(unix)]
#[tokio::test]
async fn state_survives_runs() {
    let mut session = Session::new(Options::default()).await.expect("session");

    let report = session.run("x=41; cd /tmp").await;
    assert_eq!(report.exit, 0, "{report:?}");

    let report = session.run("echo $((x+1)) in $PWD").await;
    assert_eq!(report.stdout, "42 in /tmp\n");
}

#[cfg(unix)]
#[tokio::test]
async fn snapshot_rewinds_shell_state() {
    let mut session = Session::new(Options::default()).await.expect("session");
    session.run("x=1; cd /tmp").await;

    let checkpoint = session.snapshot();

    session.run("x=2; cd /").await;

    let mut restored = checkpoint;
    let report = restored.run("echo $x in $PWD").await;
    assert_eq!(
        report.stdout, "1 in /tmp\n",
        "the snapshot restored the variable and the cwd"
    );
}

#[tokio::test]
async fn branches_are_isolated() {
    let mut session = Session::new(Options::default()).await.expect("session");
    session.run("x=base").await;

    let branches = vec![
        "x=branch-a; echo $x".to_string(),
        "x=branch-b; echo $x".to_string(),
    ];
    let reports = session.branch(&branches).await;

    assert_eq!(reports[0].stdout, "branch-a\n");
    assert_eq!(reports[1].stdout, "branch-b\n");

    let report = session.run("echo $x").await;
    assert_eq!(
        report.stdout, "base\n",
        "a branch cannot leak into the session"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn reports_are_typed_data() {
    let mut session = Session::new(Options::default()).await.expect("session");
    let report = session.run("false | head -1").await;

    assert_eq!(report.class, "stage_failed");
    assert_eq!(report.commands.len(), 2);
    assert_eq!(report.commands[0].status, 1);
    assert_eq!(report.commands[0].stage, Some(0));
    assert_eq!(report.hidden_failures, vec![0]);
}

