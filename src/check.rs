//! Static analysis: what this script will hide, before it runs.
//!
//! No execution, no side effects. It reads the same AST the supervisor reads and
//! reports the shapes whose status the shell will destroy: chains, pipelines,
//! delegated compounds, opaque substitutions.

use brush_parser::ast::{AndOrList, Command, CompoundList, Pipeline, Program};
use serde::{Deserialize, Serialize};

use crate::scan;

/// One thing worth knowing before running the script.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    /// 0-based statement index in the script.
    pub index: usize,
    /// The statement, as the parser renders it.
    pub text: String,
    /// A stable kind for machine handling.
    pub kind: String,
    /// One sentence a person can act on.
    pub detail: String,
}

/// The result of analysing a script without running it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckReport {
    /// True when nothing was found.
    pub ok: bool,
    pub findings: Vec<Finding>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parse_error: Option<String>,
}

/// Analyses `program`, parsed from `src`.
#[must_use]
pub fn check_program(program: &Program, _src: &str, supervise_substitutions: bool) -> CheckReport {
    let mut findings = Vec::new();
    let mut index = 0;

    for complete in &program.complete_commands {
        let items = &complete.0;

        for (position, item) in items.iter().enumerate() {
            let list = &item.0;
            let text = list.to_string();

            // `a; b; c` keeps only `c`'s status. `&&`/`||` are one item and are
            // exempt: the shell is supposed to drop the left side there.
            if items.len() > 1 && position + 1 < items.len() {
                findings.push(Finding {
                    index,
                    text: text.clone(),
                    kind: "dropped_status".to_string(),
                    detail: "its exit status is discarded; the next statement's status replaces it"
                        .to_string(),
                });
            }

            check_list(list, index, &text, supervise_substitutions, &mut findings);

            if scan::has_heredoc(&text) && has_substitution(&text) && !supervise_substitutions {
                findings.push(Finding {
                    index,
                    text: text.clone(),
                    kind: "opaque_substitution".to_string(),
                    detail: "a heredoc body hides the substitution from supervision".to_string(),
                });
            }

            index += 1;
        }
    }

    CheckReport {
        ok: findings.is_empty(),
        findings,
        parse_error: None,
    }
}

fn check_list(
    list: &AndOrList,
    index: usize,
    text: &str,
    supervise_substitutions: bool,
    findings: &mut Vec<Finding>,
) {
    for (_, pipeline) in list.iter() {
        check_pipeline(pipeline, index, text, supervise_substitutions, findings);
    }
}

fn check_pipeline(
    pipeline: &Pipeline,
    index: usize,
    text: &str,
    supervise_substitutions: bool,
    findings: &mut Vec<Finding>,
) {
    let stages = pipeline.seq.len();

    if stages > 1 {
        for (stage, command) in pipeline.seq.iter().enumerate() {
            if stage + 1 < stages {
                findings.push(Finding {
                    index,
                    text: command.to_string(),
                    kind: "masked_stage".to_string(),
                    detail: format!(
                        "stage {} of {} is not the last; its exit status is discarded",
                        stage + 1,
                        stages
                    ),
                });
            }

            if is_state_changing(command) {
                findings.push(Finding {
                    index,
                    text: command.to_string(),
                    kind: "wont_persist".to_string(),
                    detail: "runs in a pipeline subshell; the change does not survive the pipeline"
                        .to_string(),
                });
            }
        }
    }

    if pipeline.bang {
        findings.push(Finding {
            index,
            text: text.to_string(),
            kind: "negated_status".to_string(),
            detail: "`!` replaces the pipeline's status with its negation".to_string(),
        });
    }

    if !supervise_substitutions && has_substitution(text) {
        findings.push(Finding {
            index,
            text: text.to_string(),
            kind: "opaque_substitution".to_string(),
            detail: "a failure inside `$( )` is invisible to the exit status".to_string(),
        });
    }

    if supervise_substitutions {
        for sub in scan::substitutions(text) {
            if !sub.quoted {
                findings.push(Finding {
                    index,
                    text: format!("$({})", sub.inner),
                    kind: "unquoted_substitution".to_string(),
                    detail: "unquoted substitution output is word-split and globbed".to_string(),
                });
            }
        }
    }
}

fn is_state_changing(command: &Command) -> bool {
    let Command::Simple(simple) = command else {
        return false;
    };
    let Some(name) = &simple.word_or_name else {
        return false;
    };
    matches!(
        name.to_string().as_str(),
        "cd" | "export" | "unset" | "alias" | "unalias" | "declare" | "local"
    )
}

fn has_substitution(text: &str) -> bool {
    text.contains("$(") || text.contains('`')
}

/// Analyses a bare `CompoundList` (useful for callers holding a subset).
#[must_use]
pub fn check_list_only(list: &CompoundList, index: usize, text: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    for item in &list.0 {
        check_list(&item.0, index, text, false, &mut findings);
    }
    findings
}
