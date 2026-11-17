//! The supervisor.
//!
//! brush owns the shell: words, expansions, globbing, arithmetic, builtins,
//! pipelines and redirections are its job and stay unchanged. agshell owns the
//! *topology*: it walks the parsed script, decides what runs next, and records
//! what happened to every command and every pipeline stage.
//!
//! The old implementation reimplemented pipelines with `os.Pipe` and started a
//! `bash -c` per stage. This one embeds `brush-core`, so a builtin command
//! (`echo`, `cd`, `test`, ...) never leaves the process, and a pipeline is
//! executed by a shell that knows how to run a pipeline.

use std::io::Read;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use brush_builtins::{BuiltinSet, ShellBuilderExt};
use brush_core::openfiles::OpenFile;
use brush_core::{ExecutionControlFlow, ProcessGroupPolicy, Shell};
use brush_parser::ast::{
    AndOr, AndOrList, Command, CompoundList, CompoundListItem, Pipeline, Program,
    SeparatorOperator, SourceLocation,
};

use crate::report::{
    CommandRecord, Report, SKIPPED, STDERR_CAP, bin_of, classify, hidden, looks_noisy,
};

/// Knobs the CLI sets.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Per-statement timeout. Zero disables it.
    pub timeout: Duration,
    /// How much stdout to keep in the envelope.
    pub max_stdout: usize,
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
    const fn failed(message: String) -> Self {
        Self {
            status: 1,
            timed_out: false,
            exit_shell: false,
            ms: 0,
            stderr: message,
        }
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

    fn push(&mut self, bytes: &[u8], total: usize) {
        self.total += total;
        if self.buf.len() < self.limit {
            let room = self.limit - self.buf.len();
            let take = room.min(bytes.len());
            self.buf.push_str(&String::from_utf8_lossy(&bytes[..take]));
        }
    }

    fn truncated(&self) -> bool {
        self.total > self.buf.len()
    }
}

pub struct Runner {
    shell: Shell,
    opts: Options,
    sink: Sink,
    commands: Vec<CommandRecord>,
    pipelines: usize,
    error: String,
}

impl Runner {
    /// Builds a non-interactive shell with the standard builtins, no rc files,
    /// and a copy of the caller's environment.
    pub async fn new(opts: Options) -> Result<Self, String> {
        let shell = Shell::builder()
            .default_builtins(BuiltinSet::BashMode)
            .shell_name("agshell".to_string())
            .build()
            .await
            .map_err(|e| format!("failed to start shell: {e}"))?;

        Ok(Self {
            shell,
            opts,
            sink: Sink {
                buf: String::new(),
                total: 0,
                limit: opts.max_stdout,
            },
            commands: Vec::new(),
            pipelines: 0,
            error: String::new(),
        })
    }

    /// Parses and executes `src`, returning the envelope.
    pub async fn run(mut self, src: &str) -> Report {
        let start = Instant::now();

        let program = match self.shell.parse_string(src.to_string()) {
            Ok(p) => p,
            Err(e) => {
                return Report {
                    class: "parse_error".to_string(),
                    exit: 2,
                    duration_ms: ms(start.elapsed()),
                    commands: Vec::new(),
                    hidden_failures: Vec::new(),
                    stdout: String::new(),
                    stdout_bytes: 0,
                    stdout_truncated: false,
                    opaque_substitution: false,
                    error: e.to_string(),
                };
            }
        };

        let mut exit = 0;

        // Reconstruct each top-level statement's original source text once:
        // the parser's per-node spans stop at the compound keyword, so the
        // reliable boundary is "from this statement's start to the next one's".
        let statements = statement_texts(&program, src);

        let mut index = 0;
        'script: for complete in &program.complete_commands {
            for CompoundListItem(and_or, sep) in &complete.0 {
                let text = statements.get(index).cloned().unwrap_or_default();
                index += 1;

                // `cmd &`: the shell runs the whole list asynchronously. We
                // cannot see inside a background job, and say so.
                if matches!(sep, SeparatorOperator::Async) && and_or.additional.is_empty() {
                    let out = self
                        .exec(program_of(and_or.clone(), SeparatorOperator::Async))
                        .await;
                    self.record_and_or(&text, &out);
                    exit = out.status;
                    if out.timed_out {
                        break 'script;
                    }
                    continue;
                }

                let out = self.run_and_or(and_or, &text).await;
                exit = out.status;
                if out.timed_out || out.exit_shell {
                    break 'script;
                }
            }
        }

        let mut class = classify(&self.commands, exit);
        if class == "ok" && looks_noisy(&self.sink.buf) {
            class = "ok_noisy".to_string();
        }

        Report {
            class,
            exit,
            duration_ms: ms(start.elapsed()),
            hidden_failures: hidden(&self.commands, exit),
            stdout: self.sink.buf.clone(),
            stdout_bytes: self.sink.total,
            stdout_truncated: self.sink.truncated(),
            opaque_substitution: src.contains("$(") || src.contains('`'),
            commands: self.commands,
            error: self.error,
        }
    }

    // ------------------------------------------------------------- topology

    /// Runs the pipelines of an `&&`/`||` list, honouring short-circuiting.
    async fn run_and_or(&mut self, list: &AndOrList, text: &str) -> Outcome {
        let mut out = self.run_pipeline(&list.first, Some(text)).await;
        if out.timed_out || out.exit_shell {
            return out;
        }

        for item in &list.additional {
            match item {
                AndOr::And(p) => {
                    if out.status == 0 {
                        out = self.run_pipeline(p, None).await;
                    } else {
                        self.record_skipped(p);
                    }
                }
                AndOr::Or(p) => {
                    if out.status != 0 {
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
            let command = &pipeline.seq[0];
            let text = statement.map_or_else(|| command.to_string(), ToString::to_string);
            let out = self.exec(program_of_pipeline(pipeline.clone())).await;
            let mut record = CommandRecord::simple(
                self.commands.len(),
                text.clone(),
                bin_of(&text),
                out.status,
                out.ms,
            );
            record.timed_out = out.timed_out;
            record.stderr = out.stderr.clone();
            self.commands.push(record);
            return out;
        }

        // Everything that is genuinely a pipeline, negated, timed, or built
        // from compound commands: let the shell run it, then attribute.
        let out = self.exec(program_of_pipeline(pipeline.clone())).await;

        let mut stages: Vec<String> = pipeline.seq.iter().map(ToString::to_string).collect();
        if stages.len() == 1
            && let Some(text) = statement
        {
            stages[0] = text.to_string();
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
        let negated_single = pipeline.bang && stages.len() == 1;
        let last = stages.len().saturating_sub(1);

        for (stage, text) in stages.into_iter().enumerate() {
            let status = if attributed {
                // `! cmd` reports the negated status, not PIPESTATUS[0]; for a
                // single-stage negated pipeline those are the two candidates.
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
            let mut record = CommandRecord::simple(
                self.commands.len(),
                text.clone(),
                bin_of(&text),
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
            self.commands.push(record);
        }

        out
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
            kill_tree();
        }

        let grace = Duration::from_millis(500);
        if let Ok((bytes, total)) = out_rx.recv_timeout(grace) {
            self.sink.push(&bytes, total);
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

    fn record_and_or(&mut self, text: &str, out: &Outcome) {
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
        self.commands.push(record);
    }

    fn record_skipped(&mut self, pipeline: &Pipeline) {
        let text = pipeline.to_string();
        let mut record =
            CommandRecord::simple(self.commands.len(), text.clone(), bin_of(&text), SKIPPED, 0);
        record.skipped = true;
        record.delegated = true;
        self.commands.push(record);
    }
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

/// Kills every process descended from this one. The shell's children are in
/// their own process groups, so a signal to agshell's group would miss them.
fn kill_tree() {
    #[cfg(target_os = "linux")]
    {
        let me = i32::try_from(std::process::id()).unwrap_or(0);
        for _ in 0..5 {
            let children = descendants_of(me);
            if children.is_empty() {
                break;
            }
            for pid in children {
                // SAFETY: `kill` on a pid read from /proc; a failure just means
                // the process already exited.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(target_os = "linux")]
fn descendants_of(root: i32) -> Vec<i32> {
    let mut edges: Vec<(i32, i32)> = Vec::new();

    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
            else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            // `comm` may contain spaces and parentheses; the fields we want
            // start after the last ')'.
            let Some(paren) = stat.rfind(')') else {
                continue;
            };
            let mut fields = stat[paren + 1..].split_whitespace();
            let _state = fields.next();
            if let Some(ppid) = fields.next().and_then(|f| f.parse::<i32>().ok()) {
                edges.push((pid, ppid));
            }
        }
    }

    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for (pid, ppid) in &edges {
            if *ppid == parent && *pid > 1 && !found.contains(pid) {
                found.push(*pid);
                frontier.push(*pid);
            }
        }
    }
    found
}

/// Asks the kernel to reparent orphaned grandchildren to us, so a timeout can
/// still find them.
pub fn become_subreaper() {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: plain prctl with the documented subreaper option.
        unsafe {
            libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1);
        }
    }
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}
