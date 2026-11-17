//! Seeing inside a statement the shell runs whole.
//!
//! `if`, loops, `case`, subshells, functions and `eval` are executed by the
//! embedded shell, so their internals never pass through the supervisor. The
//! shell does announce them, though: bash's `DEBUG` trap fires before every
//! simple command and `ERR` fires on every failure that bash itself does not
//! consider handled (a condition, a `!`, a non-final `&&`/`||` operand). With
//! `functrace`/`errtrace` on, both fire inside functions and subshells.
//!
//! This module installs those two traps around one statement, points them at
//! a builtin of ours, and turns the log into [`Inner`] records. No control flow
//! is re-implemented: the shell stays the only thing that decides what runs.
//!
//! What the log can say, exactly:
//!
//! * which simple commands ran, in order, with their *expanded* words;
//! * which of them failed in a way bash did not handle (`ERR`);
//! * the status of a command, when another command follows it in the same
//!   shell context (`$?` at the next `DEBUG`).
//!
//! What it cannot: the status of the *last* command of a context when nothing
//! follows and nothing failed (reported as `inferred`), and per-stage status
//! of a pipeline that sits inside the body.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use brush_core::traps::TrapSignal;
use brush_core::variables::ShellVariable;
use brush_core::{ExecutionResult, Shell, SourceInfo, builtins};

/// True if `text` holds a `[[ ]]` test or a `(( ))` command (not the `$(( ))`
/// expansion), which the shell runs without announcing them.
#[must_use]
pub fn has_silent_commands(text: &str) -> bool {
    if text.contains("[[") {
        return true;
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .windows(2)
        .enumerate()
        .any(|(i, w)| w == ['(', '('] && (i == 0 || chars[i - 1] != '$'))
}

/// Name of the builtin the traps call.
const PROBE: &str = "__agshell_probe";

/// Shell variable that carries the run id into the traps.
const RUN_VAR: &str = "__AGSHELL_RUN";

static NEXT_RUN: AtomicU64 = AtomicU64::new(1);
static LOGS: Mutex<Option<HashMap<u64, Vec<Event>>>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Debug,
    Err,
}

#[derive(Debug, Clone)]
struct Event {
    kind: Kind,
    /// `$?` when the trap fired.
    status: i32,
    /// Context: inside a subshell, inside a function.
    subshell: bool,
    function: bool,
    /// `$BASH_COMMAND` at the time.
    text: String,
}

/// One command seen inside a statement.
#[derive(Debug, Clone)]
pub struct Inner {
    pub text: String,
    pub status: i32,
    /// bash did not handle this failure: nothing consumed it.
    pub unhandled: bool,
    /// No later command and no failure event: `status` is assumed, not seen.
    pub inferred: bool,
    /// Ran in a subshell, which is where `$( )` and `( )` commands run.
    pub subshell: bool,
    /// Ran inside a function body.
    pub function: bool,
}

struct Probe;

impl builtins::SimpleCommand for Probe {
    fn get_content(
        _name: &str,
        _content_type: builtins::ContentType,
        _options: &builtins::ContentOptions,
    ) -> Result<String, brush_core::Error> {
        Ok("agshell internal trace hook".into())
    }

    fn execute<SE: brush_core::ShellExtensions, I: Iterator<Item = S>, S: AsRef<str>>(
        context: brush_core::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<ExecutionResult, brush_core::Error> {
        let args: Vec<String> = args.map(|a| a.as_ref().to_string()).collect();
        // args[0] is the builtin's name: kind, run id, command text.
        let (Some(kind), Some(run)) = (args.get(1), args.get(2)) else {
            return Ok(ExecutionResult::success());
        };
        let Ok(run) = run.parse::<u64>() else {
            return Ok(ExecutionResult::success());
        };

        let event = Event {
            kind: if kind == "err" { Kind::Err } else { Kind::Debug },
            status: i32::from(context.shell.last_exit_status()),
            subshell: context.shell.is_subshell(),
            function: context.shell.in_function(),
            text: args.get(3).cloned().unwrap_or_default(),
        };

        if let Ok(mut logs) = LOGS.lock()
            && let Some(log) = logs.get_or_insert_with(HashMap::new).get_mut(&run)
        {
            log.push(event);
        }
        Ok(ExecutionResult::success())
    }
}

/// The traps this module replaced, so they can be put back.
pub struct Installed {
    run: u64,
    debug: Option<brush_core::traps::TrapHandler>,
    err: Option<brush_core::traps::TrapHandler>,
    functrace: bool,
    errtrace: bool,
}

/// Installs the hook on `shell` and starts a log for it.
pub fn install<SE: brush_core::ShellExtensions>(shell: &mut Shell<SE>) -> Installed {
    shell.register_builtin_if_unset(PROBE, builtins::simple_builtin::<Probe, SE>());

    let run = NEXT_RUN.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut logs) = LOGS.lock() {
        logs.get_or_insert_with(HashMap::new).insert(run, Vec::new());
    }
    let _ = shell.set_env_global(RUN_VAR, ShellVariable::new(run.to_string()));

    let installed = Installed {
        run,
        debug: shell.traps().get_handler(TrapSignal::Debug).cloned(),
        err: shell.traps().get_handler(TrapSignal::Err).cloned(),
        functrace: shell.options().shell_functions_inherit_debug_and_return_traps,
        errtrace: shell.options().shell_functions_inherit_err_trap,
    };

    shell.options_mut().shell_functions_inherit_debug_and_return_traps = true;
    shell.options_mut().shell_functions_inherit_err_trap = true;

    // `$BASH_COMMAND` is read as an argument: it is expanded before the probe's
    // own preexecute overwrites it.
    let source = || SourceInfo::from("agshell");
    shell.traps_mut().register_handler(
        TrapSignal::Debug,
        format!("{PROBE} dbg \"${RUN_VAR}\" \"$BASH_COMMAND\""),
        source(),
    );
    shell.traps_mut().register_handler(
        TrapSignal::Err,
        format!("{PROBE} err \"${RUN_VAR}\" \"$BASH_COMMAND\""),
        source(),
    );

    installed
}

/// Removes the hook, restores what the user had, and returns the commands the
/// log saw.
///
/// `ambiguous` says the statement holds a standalone `[[ ]]` or `(( ))`. The
/// shell does not announce those, so when `ERR` fires its `$BASH_COMMAND` may
/// name the command *before* the one that failed. Failures are then reported
/// as unattributed rather than pinned on a neighbour.
pub fn finish<SE: brush_core::ShellExtensions>(
    shell: &mut Shell<SE>,
    installed: Installed,
    ambiguous: bool,
) -> Vec<Inner> {
    shell.traps_mut().remove_handlers(TrapSignal::Debug);
    shell.traps_mut().remove_handlers(TrapSignal::Err);
    if let Some(h) = installed.debug {
        shell.traps_mut().register_handler(TrapSignal::Debug, h.command, h.source_info);
    }
    if let Some(h) = installed.err {
        shell.traps_mut().register_handler(TrapSignal::Err, h.command, h.source_info);
    }
    shell.options_mut().shell_functions_inherit_debug_and_return_traps = installed.functrace;
    shell.options_mut().shell_functions_inherit_err_trap = installed.errtrace;
    let _ = shell.env_mut().unset(RUN_VAR);

    let events = LOGS
        .lock()
        .ok()
        .and_then(|mut logs| logs.as_mut().and_then(|m| m.remove(&installed.run)))
        .unwrap_or_default();

    interpret(&events, ambiguous)
}

/// Turns the event log into commands.
fn interpret(events: &[Event], ambiguous: bool) -> Vec<Inner> {
    // The hook's own commands are announced by the DEBUG trap too; they are not
    // the script's, and would sit between a failure and its ERR event.
    let events: Vec<&Event> = events
        .iter()
        .filter(|e| !(e.kind == Kind::Debug && e.text.starts_with(PROBE)))
        .collect();
    let same = |a: &Event, b: &Event| a.subshell == b.subshell && a.function == b.function;

    let mut out = Vec::new();
    for (i, e) in events.iter().enumerate() {
        if e.kind != Kind::Debug {
            continue;
        }

        // The next event in the same context says what became of this command.
        let rest = &events[i + 1..];
        let next = rest.iter().position(|n| same(e, n));
        let (status, unhandled, inferred) = match next.map(|at| (at, rest[at])) {
            // bash fired ERR: it failed and nothing consumed the failure. This
            // is exact, and bash's own rules decide what counts as handled.
            Some((_, n)) if n.kind == Kind::Err && ambiguous => {
                out.push(Inner {
                    text: e.text.clone(),
                    status: 0,
                    unhandled: false,
                    inferred: true,
                    subshell: e.subshell,
                    function: e.function,
                });
                out.push(Inner {
                    text: format!("(failure at or after: {})", e.text),
                    status: n.status,
                    unhandled: n.status != 0,
                    inferred: false,
                    subshell: e.subshell,
                    function: e.function,
                });
                continue;
            }
            Some((_, n)) if n.kind == Kind::Err => (n.status, n.status != 0, false),
            // Another command started. Its `$?` is this command's status, unless
            // something ran in between (a `$( )` in its words, a pipeline
            // stage): then `$?` may belong to that, and we claim nothing.
            Some((at, n)) if rest[..at].is_empty() => (n.status, false, false),
            // Nothing follows, or something else ran between: no failure was
            // raised for it, so it succeeded or was handled.
            _ => (0, false, true),
        };

        out.push(Inner {
            text: e.text.clone(),
            status,
            unhandled,
            inferred,
            subshell: e.subshell,
            function: e.function,
        });
    }
    out
}
