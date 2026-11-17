//! The supervisor, and the session that owns a shell.
//!
//! brush owns the shell: words, expansions, globbing, arithmetic, builtins,
//! pipelines and redirections are its job and stay unchanged. agshell owns the
//! *topology*: it walks the parsed script, decides what runs next, and records
//! what happened to every command and every pipeline stage.
//!
//! A [`Session`] holds a live shell, so state persists across `run` calls. It is
//! cheap to [`Session::fork`]: the shell's maps are persistent data structures,
//! so a fork is a snapshot, not a copy.

use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use brush_builtins::{BuiltinSet, ShellBuilderExt};
use brush_core::openfiles::OpenFile;
use brush_core::{ExecutionControlFlow, ProcessGroupPolicy, Shell};
use brush_parser::ast::{
    AndOr, AndOrList, Command, CompoundList, CompoundListItem, Pipeline, Program,
    SeparatorOperator, SourceLocation,
};
use serde::{Deserialize, Serialize};

use crate::check::{CheckReport, check_program};
use crate::report::{
    CommandRecord, Report, SKIPPED, STDERR_CAP, bin_of, classify, hidden, looks_noisy,
};
use crate::trace;

/// How many ordinary commands seen inside a statement the envelope keeps. An
/// unhandled failure is always kept; a loop of 5,000 iterations is not 5,000
/// records.
const MAX_INNER: usize = 200;

/// Knobs a caller sets.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Per-statement timeout. Zero disables it.
    pub timeout: Duration,
    /// How much stdout to keep in the envelope.
    pub max_stdout: usize,
    /// Stop the script at the first failure instead of continuing.
    pub strict: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(600),
            max_stdout: 65536,
            strict: false,
        }
    }
}

/// Receives progress as a script runs. Every method has a default, so a caller
/// implements only what it needs.
pub trait Observer {
    /// A statement is about to run.
    fn statement_started(&self, _index: usize, _text: &str) {}
    /// A command (or a whole pipeline) finished and was recorded.
    fn command_finished(&self, _record: &CommandRecord) {}
    /// A statement finished with `status`.
    fn statement_finished(&self, _index: usize, _status: i32) {}
}

/// An observer that ignores everything.
pub struct NoopObserver;

impl Observer for NoopObserver {}

/// A live shell plus the options used to drive it.
///
/// State persists across [`Session::run`] calls: `cd`, `export`, functions,
/// arrays and `set` options all carry over, because they belong to one shell.
#[derive(Clone)]
pub struct Session {
    shell: Shell,
    opts: Options,
}

impl Session {
    /// Builds a non-interactive shell with the standard builtins, no rc files,
    /// and a copy of the caller's environment.
    pub async fn new(opts: Options) -> Result<Self, String> {
        let shell = Shell::builder()
            .default_builtins(BuiltinSet::BashMode)
            .shell_name("agshell".to_string())
            .build()
            .await
            .map_err(|e| format!("failed to start shell: {e}"))?;

        Ok(Self { shell, opts })
    }

    /// The shell's current working directory.
    #[must_use]
    pub fn working_dir(&self) -> PathBuf {
        self.shell.working_dir().to_path_buf()
    }

    /// A cheap snapshot: the shell is `Clone` over persistent data structures.
    ///
    /// Shell state (cwd, variables, functions, aliases, options) is restored.
    /// Filesystem effects are not: a checkpoint cannot un-write a file.
    #[must_use]
    pub fn snapshot(&self) -> Self {
        self.clone()
    }

    /// Executes `src` and returns the envelope. Shell state carries into the
    /// next call.
    pub async fn run(&mut self, src: &str) -> Report {
        execute(&mut self.shell, self.opts, src, &NoopObserver).await
    }

    /// Same as [`Session::run`], reporting progress to `observer`.
    pub async fn run_with(&mut self, src: &str, observer: &dyn Observer) -> Report {
        execute(&mut self.shell, self.opts, src, observer).await
    }

    /// Runs several scripts on isolated forks and returns every envelope.
    ///
    /// The forks run concurrently and share nothing, so one branch's `cd` or
    /// `export` cannot leak into another. File-system effects still collide:
    /// this is for read-only exploration, or for branches that touch separate
    /// trees.
    pub async fn branch(&self, scripts: &[String]) -> Vec<Report> {
        let futures = scripts.iter().map(|script| async move {
            let mut fork = self.clone();
            fork.run(script).await
        });
        futures::future::join_all(futures).await
    }

    /// Static analysis: parses and lints without executing anything.
    #[must_use]
    pub fn check(&self, src: &str) -> CheckReport {
        match self.shell.parse_string(src.to_string()) {
            Ok(program) => check_program(&program, src),
            Err(err) => CheckReport {
                ok: false,
                findings: Vec::new(),
                parse_error: Some(err.to_string()),
            },
        }
    }

    /// Runs `src`, overriding the timeout for this call only.
    pub async fn run_opts(&mut self, src: &str, timeout: Option<Duration>) -> Report {
        let opts = Options {
            timeout: timeout.unwrap_or(self.opts.timeout),
            ..self.opts
        };
        execute(&mut self.shell, opts, src, &NoopObserver).await
    }

    /// What the shell currently holds, for a harness that wants to show state.
    #[must_use]
    pub fn summary(&self) -> StateSummary {
        StateSummary {
            cwd: self.shell.working_dir().display().to_string(),
            last_exit: i32::from(self.shell.last_exit_status()),
            variables: self.shell.env().iter().count(),
            functions: self.shell.funcs().iter().count(),
            aliases: self.shell.aliases().len(),
        }
    }
}

/// A snapshot of what the session is holding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSummary {
    pub cwd: String,
    pub last_exit: i32,
    pub variables: usize,
    pub functions: usize,
    pub aliases: usize,
}

/// Executes `src` against `shell`, returning the envelope.
async fn execute(
    shell: &mut Shell,
    opts: Options,
    src: &str,
    observer: &dyn Observer,
) -> Report {
    let start = Instant::now();

    let program = match shell.parse_string(src.to_string()) {
        Ok(p) => p,
        Err(e) => return Report::parse_error(e.to_string(), ms(start.elapsed())),
    };

    let statements = statement_texts(&program, src);
    let mut run = Run {
        shell,
        opts,
        observer,
        sink: Sink {
            buf: String::new(),
            total: 0,
            limit: opts.max_stdout,
        },
        commands: Vec::new(),
        pipelines: 0,
        error: String::new(),
        opaque: false,
        aborted: false,
        stopped_at: None,
        abort_exit: None,
        inner_omitted: 0,
    };

    let mut exit = 0;
    let mut index = 0;

    'script: for complete in &program.complete_commands {
        for CompoundListItem(and_or, sep) in &complete.0 {
            let text = statements.get(index).cloned().unwrap_or_default();
            observer.statement_started(index, &text);
            index += 1;

            let window = run.commands.len();

            // `cmd &`: the shell runs the whole list asynchronously. We cannot
            // see inside a background job, and say so.
            if matches!(sep, SeparatorOperator::Async) && and_or.additional.is_empty() {
                let out = run
                    .exec(program_of(and_or.clone(), SeparatorOperator::Async))
                    .await;
                run.record_delegated(&text, &out);
                run.mark_opaque(&text);
                exit = out.status;
                observer.statement_finished(index - 1, exit);
                if out.timed_out {
                    break 'script;
                }
                continue;
            }

            let out = run.run_and_or(and_or, &text).await;
            exit = out.status;
            observer.statement_finished(index - 1, exit);

            if out.timed_out || out.exit_shell {
                break 'script;
            }

            if run.opts.strict
                && let Some((record, status)) = run.strict_failure(window, exit)
            {
                run.aborted = true;
                run.stopped_at = Some(record);
                run.abort_exit = Some(status);
                break 'script;
            }
        }
    }

    let natural_exit = exit;
    let mut class = classify(&run.commands, natural_exit);
    if class == "ok" && looks_noisy(&run.sink.buf) {
        class = "ok_noisy".to_string();
    }

    let exit = run.abort_exit.unwrap_or(natural_exit);

    Report {
        class,
        exit,
        duration_ms: ms(start.elapsed()),
        hidden_failures: hidden(&run.commands, exit),
        stdout: run.sink.buf.clone(),
        stdout_bytes: run.sink.total,
        stdout_truncated: run.sink.truncated(),
        opaque_substitution: run.opaque,
        aborted: run.aborted,
        stopped_at: run.stopped_at,
        inner_omitted: run.inner_omitted,
        commands: run.commands,
        error: run.error,
    }
}

/// Accumulates captured stdout across statements, bounded.
struct Sink {
    buf: String,
    total: usize,
    limit: usize,
}

impl Sink {
    fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.buf.len())
    }

    fn append(&mut self, bytes: &[u8], total: usize) {
        self.total += total;
        let room = self.limit.saturating_sub(self.buf.len());
        let take = room.min(bytes.len());
        self.buf.push_str(&String::from_utf8_lossy(&bytes[..take]));
    }

    fn truncated(&self) -> bool {
        self.total > self.buf.len()
    }
}

/// Why a statement is watched from the inside.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Inside {
    /// A function, `eval` or `source`: it runs a body.
    Body,
    /// Words with `$( )`: it runs commands to build its arguments.
    Substitution,
}

/// What one executed statement did.
struct Outcome {
    status: i32,
    timed_out: bool,
    exit_shell: bool,
    ms: i64,
    stderr: String,
}

impl Outcome {
    fn failed(message: String) -> Self {
        Self {
            status: 1,
            timed_out: false,
            exit_shell: false,
            ms: 0,
            stderr: message,
        }
    }
}

struct Run<'a, 'o> {
    shell: &'a mut Shell,
    opts: Options,
    observer: &'o dyn Observer,
    sink: Sink,
    commands: Vec<CommandRecord>,
    pipelines: usize,
    error: String,
    opaque: bool,
    aborted: bool,
    stopped_at: Option<usize>,
    abort_exit: Option<i32>,
    inner_omitted: usize,
}

impl Run<'_, '_> {
    // ------------------------------------------------------------- topology

    /// Runs the pipelines of an `&&`/`||` list, honouring short-circuiting.
    async fn run_and_or(&mut self, list: &AndOrList, text: &str) -> Outcome {
        // Start of the records produced by the pipeline that produced `out`.
        let mut start = self.commands.len();
        let mut out = self.run_pipeline(&list.first, Some(text)).await;
        if out.timed_out || out.exit_shell {
            return out;
        }

        for item in &list.additional {
            match item {
                AndOr::And(p) => {
                    if out.status == 0 {
                        start = self.commands.len();
                        out = self.run_pipeline(p, None).await;
                    } else {
                        self.record_skipped(p);
                    }
                }
                AndOr::Or(p) => {
                    if out.status != 0 {
                        // `||` consumed the failure: it was handled, not hidden.
                        self.mark_handled(start);
                        start = self.commands.len();
                        out = self.run_pipeline(p, None).await;
                    } else {
                        self.record_skipped(p);
                    }
                }
            }
            if out.timed_out || out.exit_shell {
                break;
            }
        }
        out
    }

    /// Runs one pipeline. A standalone simple command is observed exactly; a
    /// real pipeline is run by the shell, and its per-stage statuses are read
    /// back from `PIPESTATUS`.
    async fn run_pipeline(&mut self, pipeline: &Pipeline, statement: Option<&str>) -> Outcome {
        let compound = pipeline
            .seq
            .iter()
            .any(|c| !matches!(c, Command::Simple(_) | Command::ExtendedTest(_, _)));

        if pipeline.seq.len() == 1 && !compound && pipeline.timed.is_none() && !pipeline.bang {
            return self.run_simple(pipeline, statement).await;
        }

        // Everything that is genuinely a pipeline, negated, timed, or built
        // from compound commands: let the shell run it, then attribute. A
        // compound command is watched from the inside through the shell's traps.
        //
        // Only a single compound command is traced: the trap handlers run
        // pipelines of their own, which would overwrite the `PIPESTATUS` the
        // stages of a real pipeline are read back from.
        let traced = compound && pipeline.seq.len() == 1;
        let (out, inner) = if traced {
            let text = statement.map_or_else(|| pipeline.to_string(), str::to_string);
            self.exec_traced(program_of_pipeline(pipeline.clone()), &text)
                .await
        } else {
            (self.exec(program_of_pipeline(pipeline.clone())).await, Vec::new())
        };
        let nested = self.emit_inner(inner);

        let mut stages: Vec<String> = pipeline.seq.iter().map(ToString::to_string).collect();
        if stages.len() == 1
            && let Some(text) = statement
        {
            stages[0] = text.to_string();
        }
        if !traced && (stages.len() > 1 || compound) {
            self.mark_opaque(&stages.join(" "));
        }
        if pipeline.bang
            && statement.is_none()
            && let Some(first) = stages.first_mut()
        {
            first.insert_str(0, "! ");
        }

        let statuses = self.shell.last_pipeline_statuses().to_vec();
        self.pipelines += 1;
        let pipeline_id = self.pipelines;

        let attributed = statuses.len() == stages.len();
        let last = stages.len().saturating_sub(1);
        let negated_single = pipeline.bang && stages.len() == 1;

        for (stage, text) in stages.into_iter().enumerate() {
            let status = if traced {
                out.status
            } else if attributed {
                // `! cmd` reports the negated status, not PIPESTATUS[0].
                if negated_single {
                    out.status
                } else {
                    i32::from(statuses[stage])
                }
            } else if stage == last {
                out.status
            } else {
                0
            };
            let text_for_bin = text.clone();
            let mut record = CommandRecord::simple(
                self.commands.len(),
                text,
                bin_of(&text_for_bin),
                status,
                if stage == last { out.ms } else { 0 },
            );
            record.pipeline = Some(pipeline_id);
            record.stage = Some(stage);
            // When every stage's status was read back the pipeline is fully
            // observed; when attribution failed we say so rather than lie.
            record.delegated = compound || !attributed;
            if stage == last {
                record.stderr = out.stderr.clone();
                record.timed_out = out.timed_out;
            }
            self.emit(record);
        }

        self.link_inner(nested, false);
        out
    }

    /// A single simple command. One that runs more than itself (a function, an
    /// `eval`, a `source`, or words with `$( )`) is watched from the inside.
    async fn run_simple(&mut self, pipeline: &Pipeline, statement: Option<&str>) -> Outcome {
        let command = &pipeline.seq[0];
        let original = statement.map_or_else(|| command.to_string(), ToString::to_string);
        let to_exec = program_of_pipeline(pipeline.clone());

        let reason = self.inner_reason(&original);
        let (out, mut inner) = if reason.is_some() {
            self.exec_traced(to_exec, &original).await
        } else {
            (self.exec(to_exec).await, Vec::new())
        };

        // The shell also announces the command itself, in the base context,
        // after its words were expanded: that one is the record below.
        if let Some(at) = inner.iter().position(|i| !i.subshell && !i.function) {
            inner.remove(at);
        }
        let nested = self.emit_inner(inner);

        let mut record = CommandRecord::simple(
            self.commands.len(),
            original.clone(),
            bin_of(&original),
            out.status,
            out.ms,
        );
        record.timed_out = out.timed_out;
        record.stderr = out.stderr.clone();
        self.emit(record);

        // With nothing but `$( )` in its words, everything seen inside was a
        // substitution.
        self.link_inner(nested, reason == Some(Inside::Substitution));
        out
    }

    /// Why a simple command is worth watching from the inside, if it is.
    fn inner_reason(&self, text: &str) -> Option<Inside> {
        let name = bin_of(text);
        if matches!(name.as_str(), "eval" | "source" | ".") || self.shell.funcs().get(&name).is_some()
        {
            Some(Inside::Body)
        } else if text.contains("$(") || text.contains('`') {
            Some(Inside::Substitution)
        } else {
            None
        }
    }

    /// Runs `program` with the shell's `DEBUG`/`ERR` traps pointed at us, and
    /// returns what the shell reported running inside it.
    async fn exec_traced(&mut self, program: Program, text: &str) -> (Outcome, Vec<trace::Inner>) {
        let ambiguous = self.has_silent_commands(text);
        let installed = trace::install(self.shell);
        let out = self.exec(program).await;
        let inner = trace::finish(self.shell, installed, ambiguous);
        (out, inner)
    }

    /// Whether `text`, or a function it calls, holds a standalone `[[ ]]` or
    /// `(( ))`. `source` runs a file we have not read, so it counts too.
    fn has_silent_commands(&self, text: &str) -> bool {
        use std::collections::HashSet;

        if text.split_whitespace().any(|w| w == "source" || w == ".") {
            return true;
        }
        let mut tainted: HashSet<String> = HashSet::new();
        let bodies: Vec<(String, String)> = self
            .shell
            .funcs()
            .iter()
            .map(|(name, f)| (name.clone(), f.definition().to_string()))
            .collect();
        // A function is tainted if its body is, or if it calls one that is.
        loop {
            let before = tainted.len();
            for (name, body) in &bodies {
                if !tainted.contains(name)
                    && (trace::has_silent_commands(body)
                        || tainted.iter().any(|t| mentions(body, t)))
                {
                    tainted.insert(name.clone());
                }
            }
            if tainted.len() == before {
                break;
            }
        }
        trace::has_silent_commands(text) || tainted.iter().any(|t| mentions(text, t))
    }

    /// Records the commands seen inside a statement. They ran before the
    /// statement finished, so they come first; [`Self::link_inner`] points them
    /// at it afterwards.
    fn emit_inner(&mut self, inner: Vec<trace::Inner>) -> std::ops::Range<usize> {
        let start = self.commands.len();
        let mut kept = 0usize;
        for item in inner {
            if !item.unhandled {
                kept += 1;
                if kept > MAX_INNER {
                    self.inner_omitted += 1;
                    continue;
                }
            }
            let mut record = CommandRecord::simple(
                self.commands.len(),
                item.text.clone(),
                bin_of(&item.text),
                item.status,
                0,
            );
            record.inferred = item.inferred;
            // A non-zero status bash did not raise ERR for was consumed by a
            // condition, `!` or `||`: expected, not hidden.
            record.handled = item.status != 0 && !item.unhandled;
            self.emit(record);
        }
        start..self.commands.len()
    }

    fn link_inner(&mut self, range: std::ops::Range<usize>, substitution: bool) {
        let parent = self.commands.len().saturating_sub(1);
        for i in range {
            if i != parent
                && let Some(rec) = self.commands.get_mut(i)
            {
                rec.inside = Some(parent);
                if substitution {
                    rec.substitution_of = Some(parent);
                }
            }
        }
    }

    // ------------------------------------------------------------ execution

    /// Hands one parsed program to the shell, capturing its output and
    /// enforcing the timeout.
    async fn exec(&mut self, program: Program) -> Outcome {
        let start = Instant::now();

        let (Ok((out_read, out_write)), Ok((err_read, err_write))) =
            (std::io::pipe(), std::io::pipe())
        else {
            return Outcome::failed("failed to create pipe".to_string());
        };

        let (out_tx, out_rx) = mpsc::channel();
        let (err_tx, err_rx) = mpsc::channel();
        spawn_reader(out_read, self.sink.remaining(), out_tx);
        spawn_reader(err_read, STDERR_CAP, err_tx);

        let mut params = self.shell.default_exec_params();
        params.set_fd(1, OpenFile::PipeWriter(out_write));
        params.set_fd(2, OpenFile::PipeWriter(err_write));
        if let Ok(null) = brush_core::openfiles::null() {
            params.set_fd(0, null);
        }
        params.process_group_policy = ProcessGroupPolicy::NewProcessGroup;

        let future = self.shell.run_program(program, &params);
        let result = if self.opts.timeout.is_zero() {
            Some(future.await)
        } else {
            tokio::time::timeout(self.opts.timeout, future).await.ok()
        };

        // Closing our copies of the write ends is what lets the readers see EOF.
        drop(params);

        let timed_out = result.is_none();
        if timed_out {
            crate::sys::kill_tree();
        }

        let grace = Duration::from_millis(500);
        if let Ok((bytes, total)) = out_rx.recv_timeout(grace) {
            self.sink.append(&bytes, total);
        }
        let stderr = err_rx
            .recv_timeout(grace)
            .map(|(bytes, _)| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();

        let mut outcome = Outcome {
            status: 0,
            timed_out,
            exit_shell: false,
            ms: ms(start.elapsed()),
            stderr,
        };

        match result {
            None => {
                outcome.status = 137;
                outcome.stderr.push_str("\nkilled: timeout");
            }
            Some(Ok(res)) => {
                outcome.status = i32::from(u8::from(res.exit_code));
                outcome.exit_shell =
                    matches!(res.next_control_flow, ExecutionControlFlow::ExitShell);
            }
            Some(Err(err)) => {
                outcome.status = 1;
                if outcome.stderr.is_empty() {
                    outcome.stderr = err.to_string();
                }
                if self.error.is_empty() {
                    self.error = err.to_string();
                }
            }
        }

        outcome
    }

    // ------------------------------------------------------------- recording

    fn emit(&mut self, record: CommandRecord) {
        self.observer.command_finished(&record);
        self.commands.push(record);
    }

    fn record_delegated(&mut self, text: &str, out: &Outcome) {
        let mut record = CommandRecord::simple(
            self.commands.len(),
            text.to_string(),
            bin_of(text),
            out.status,
            out.ms,
        );
        record.delegated = true;
        record.timed_out = out.timed_out;
        record.stderr = out.stderr.clone();
        self.emit(record);
    }

    fn record_skipped(&mut self, pipeline: &Pipeline) {
        let text = pipeline.to_string();
        let mut record =
            CommandRecord::simple(self.commands.len(), text.clone(), bin_of(&text), SKIPPED, 0);
        record.skipped = true;
        record.delegated = true;
        self.emit(record);
    }

    /// Marks records from `window` on as failures that `||` consumed.
    fn mark_handled(&mut self, window: usize) {
        for record in &mut self.commands[window..] {
            record.handled = true;
        }
    }

    /// Records that some substitution in `text` ran without being watched, so
    /// its failure is invisible: it sits in a pipeline stage or a background
    /// job.
    fn mark_opaque(&mut self, text: &str) {
        if text.contains("$(") || text.contains('`') {
            self.opaque = true;
        }
    }

    /// The first failure a `--strict` run should stop at, if any.
    fn strict_failure(&self, window: usize, status: i32) -> Option<(usize, i32)> {
        let records = &self.commands[window..];

        if status != 0 {
            let record = records.last()?;
            return Some((record.i, status));
        }

        for record in records {
            if record.skipped {
                continue;
            }
            let is_earlier_stage = record.pipeline.is_some_and(|pid| {
                let last = self
                    .commands
                    .iter()
                    .filter(|o| o.pipeline == Some(pid))
                    .map(|o| o.stage.unwrap_or(0))
                    .max()
                    .unwrap_or(0);
                record.stage.unwrap_or(0) < last
            });

            // A failure inside a loop body, branch or function that bash did
            // not consider handled. The statement ran whole, so it finishes;
            // the script stops after it.
            let failed_inside = record.inside.is_some() && !record.handled;

            if record.status != 0
                && (is_earlier_stage || record.substitution_of.is_some() || failed_inside)
            {
                return Some((record.i, record.status));
            }
        }

        None
    }
}

/// True if `name` appears in `text` as a whole word.
fn mentions(text: &str, name: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '.'))
        .any(|w| w == name)
}

/// Reconstructs each top-level statement's original source text.
///
/// brush-parser's per-node spans are only reliable for simple commands (a
/// compound command's span stops at its keyword), so the boundary used here is
/// "from this statement's start to the next statement's start". The result is
/// exactly what the caller wrote, not a re-rendering of the AST.
fn statement_texts(program: &Program, src: &str) -> Vec<String> {
    let chars: Vec<char> = src.chars().collect();
    let offsets: Vec<usize> = src
        .char_indices()
        .map(|(byte, _)| byte)
        .chain(std::iter::once(src.len()))
        .collect();
    let total = offsets.len() - 1;

    let starts: Vec<usize> = program
        .complete_commands
        .iter()
        .flat_map(|complete| complete.0.iter())
        .map(|item| {
            let reported = item.0.location().map_or(0, |span| span.start.index);
            true_start(&chars, reported)
        })
        .collect();

    starts
        .iter()
        .enumerate()
        .map(|(i, &start)| {
            let end = starts.get(i + 1).copied().unwrap_or(total);
            if start >= end || end > total {
                return String::new();
            }
            src[offsets[start]..offsets[end]]
                .trim()
                .trim_end_matches(';')
                .trim_end()
                .to_string()
        })
        .collect()
}

/// A pipeline's span excludes a leading `!` (and `time`), so a statement's real
/// start can be a few characters before the span says. Walk back over
/// whitespace and those prefixes.
fn true_start(chars: &[char], reported: usize) -> usize {
    let mut i = reported.min(chars.len());
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }

    if i > 0 && chars[i - 1] == '!' && (i < 2 || !chars[i - 2].is_alphanumeric()) {
        return i - 1;
    }

    if i >= 4 && chars[i - 4..i].iter().copied().eq("time".chars()) {
        let before = i.checked_sub(5).and_then(|j| chars.get(j)).copied();
        if before.is_none_or(|c| !c.is_alphanumeric()) {
            return i - 4;
        }
    }

    reported
}

/// Wraps an and-or list back into a runnable program. No re-parsing: the AST
/// is the one the shell produced, so semantics are exact.
fn program_of(list: AndOrList, separator: SeparatorOperator) -> Program {
    Program {
        complete_commands: vec![CompoundList(vec![CompoundListItem(list, separator)])],
    }
}

fn program_of_pipeline(pipeline: Pipeline) -> Program {
    program_of(
        AndOrList {
            first: pipeline,
            additional: Vec::new(),
        },
        SeparatorOperator::Sequence,
    )
}

/// Drains a pipe on a blocking thread, keeping at most `limit` bytes.
fn spawn_reader(mut reader: std::io::PipeReader, limit: usize, tx: mpsc::Sender<(Vec<u8>, usize)>) {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut total = 0usize;
        let mut chunk = [0u8; 8192];

        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    if kept.len() < limit {
                        let take = (limit - kept.len()).min(n);
                        kept.extend_from_slice(&chunk[..take]);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }

        let _ = tx.send((kept, total));
    });
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}
