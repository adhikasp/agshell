//! agshell — a supervised execution shell for coding agents.
//!
//! The shell is a library here, not a subprocess: [`Session`] owns a live
//! `brush-core` shell, so `cd`, `export`, functions and options survive between
//! calls, and a [`Session::snapshot`] is a cheap checkpoint.
//!
//! ```no_run
//! # async fn demo() -> Result<(), String> {
//! use agshell::{Options, Session};
//!
//! let mut session = Session::new(Options::default()).await?;
//! let report = session.run("cd /tmp; false | head -1").await;
//! assert_eq!(report.class, "stage_failed");
//! # Ok(())
//! # }
//! ```

pub mod check;
pub mod report;
pub mod scan;
pub mod serve;
pub mod session;
pub mod sys;

pub use check::{CheckReport, Finding};
pub use report::{CommandRecord, Report};
pub use session::{NoopObserver, Observer, Options, Session, StateSummary};
