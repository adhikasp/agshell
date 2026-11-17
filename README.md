# agshell

A supervised execution shell for coding agents. **Proof of concept.**

bash reports one exit status per command string. Agents mostly don't run single
commands — measured over 37,700 real agent shell calls, only **21%** are a single
simple command: 62% contain a pipeline, 51% a `;` chain, 28% an `&&` chain. So for
roughly four commands in five, the status an agent reads is *structurally
incapable* of describing what happened. `a; b; c` returns `c`'s status. `PIPESTATUS`
covers the end of one pipeline and nothing else — not the chains around it, not
`$( )`, not a stage that was killed.

agshell takes over the *topology*. It parses the script, then runs each statement
under its own supervision, so every command and every pipeline stage keeps its own
status. The shell is [brush](https://brush.sh) (`brush-core`), embedded as a
library: words, quoting, globbing, arithmetic, builtins, pipelines and
redirections are a real shell's job, unchanged. agshell reports what actually ran.

```console
$ bash -c 'false; echo hi; true'; echo "exit=$?"
hi
exit=0                       # the failure is gone

$ agshell -c 'false; echo hi; true' --human
$ false; echo hi; true

  #0  false                              status=1      0ms
  #1  echo hi                            status=0      0ms
  #2  true                               status=0      0ms

class=chain_partial exit=0 hidden_failures=[0]
```

```console
$ bash -c 'false | head -1'; echo "exit=$?"
exit=0                       # pipefail would catch this one, no chain would

$ agshell -c 'false | head -1' --human
  #0  false                              status=1      0ms pipe1.0
  #1  head -1                            status=0      2ms pipe1.1

class=stage_failed exit=0
```

The envelope, not the text, is the interface:

```json
{
  "class": "stage_failed",
  "exit": 0,
  "duration_ms": 2,
  "commands": [
    {"i": 0, "text": "false",   "bin": "false",   "status": 1, "ms": 0, "pipeline": 1, "stage": 0},
    {"i": 1, "text": "head -1", "bin": "head",    "status": 0, "ms": 2, "pipeline": 1, "stage": 1}
  ],
  "stdout": "",
  "stdout_bytes": 0
}
```

`class` is one of:

| class | meaning |
|---|---|
| `ok` | every command and stage exited 0, and the output looks clean |
| `ok_noisy` | exited 0 but printed something that reads like an error |
| `chain_partial` | exited 0, an earlier command in a chain failed — `hidden_failures` names it |
| `stage_failed` | exited 0, an earlier **pipeline stage** failed |
| `substitution_failed` | exited 0, a supervised `$( )` failed |
| `failed` | the final status was non-zero (bash-compatible) |
| `timeout` | a statement was killed at `--timeout`; its whole process tree is signalled |
| `parse_error` | the shell itself would not have parsed it |

`exit` stays bash-compatible, so agshell can sit under an existing harness
unchanged.

## Build

```console
$ cargo build --release
$ ./target/release/agshell -c 'go test ./... 2>&1 | tail -5'   # JSON envelope
$ ./target/release/agshell -c 'make' --human --timeout 5m      # report
$ echo '<script>' | ./target/release/agshell                   # stdin works too
```

Rust 1.88+. Linux and Windows both run scripts; on Linux timeouts kill the
process tree with `SIGKILL`, on Windows with `TerminateProcess` over a
`CreateToolhelp32Snapshot` walk.

## The four modes

### 1. One shot

`agshell -c '<script>'` runs a script and prints the envelope. This is what an
agent's existing shell tool wants: same interface, better data.

### 2. `--strict`: stop at the first failure

```console
$ agshell -c 'false; echo hi; true' --strict --human
  #0  false                              status=1      0ms

class=failed exit=1 1ms aborted_at=0
```

No bash-expressible equivalent: `set -e` does not fire on `a | b`, on a `;`
chain's earlier commands, or inside `$( )`. `--strict` stops when a statement
fails, **or** when a pipeline stage was masked, **or** when a supervised
substitution failed. `false || true` is left alone: `||` consumed the failure,
so it was handled, not hidden. When it stops, `exit` becomes the failing status.

### 3. `--check`: what this script will hide, before it runs

```console
$ agshell --check -c 'cd /tmp; false | head -1; echo done'
{
  "ok": false,
  "findings": [
    {"index": 0, "text": "cd /tmp", "kind": "dropped_status",
     "detail": "its exit status is discarded; the next statement's status replaces it"},
    {"index": 1, "text": "false", "kind": "masked_stage",
     "detail": "stage 1 of 2 is not the last; its exit status is discarded"}
  ]
}
```

No execution. It reads the same AST the supervisor reads: `dropped_status`,
`masked_stage`, `negated_status`, `wont_persist` (a `cd` in a pipeline subshell),
`opaque_substitution`, `unquoted_substitution`. An agent can lint its own command
before spending a tool call on it.

### 4. `serve`: one shell, many commands

```console
$ agshell serve
{"id":1,"op":"run","script":"cd /tmp; x=41"}
{"id":1,"ok":true,"report":{...}}
{"id":2,"op":"run","script":"echo $((x+1)) in $PWD"}
{"id":2,"ok":true,"report":{"stdout":"42 in /tmp\n",...}}
{"id":3,"op":"snapshot","key":"before-build"}
{"id":4,"op":"run","script":"x=99; cd /"}
{"id":5,"op":"restore","snapshot":"before-build"}
{"id":6,"op":"branch","scripts":["npm test","pnpm test"]}
{"id":7,"op":"state"}
{"id":8,"op":"shutdown"}
```

Operations: `run`, `check`, `branch`, `snapshot`, `restore`, `state`, `snapshots`,
`ping`, `shutdown`. State (`cd`, `export`, functions, arrays, options) persists
because it is one shell, not one process per command.

`snapshot` is a checkpoint: the shell is `Clone` over persistent data structures,
so it costs a fraction of the state. `restore` rewinds **shell** state — cwd,
variables, functions, aliases, options. Files are not restored: a checkpoint
cannot un-write a file.

`branch` runs each script on an isolated fork, concurrently, and returns every
envelope. A branch's `cd` or `export` cannot leak into the session or into
another branch. Filesystem effects still collide, so use it for read-only
exploration or for branches that touch separate trees.

## As a library

```rust
use agshell::{Options, Session};

let mut session = Session::new(Options::default()).await?;

let report = session.run("false | head -1").await;
assert_eq!(report.class, "stage_failed");
assert_eq!(report.commands[0].status, 1);   // typed, not text

let checkpoint = session.snapshot();
session.run("rm -rf build && make").await;
let mut restored = checkpoint;               // shell state rewinds
```

`Session::run_with` takes an [`Observer`] for per-statement and per-command
events, for a harness that wants progress rather than a final envelope.

## Substitution supervision

`--supervise-substitutions` executes each `$( )` and backtick in a simple command
as its own supervised unit, so its status and stderr are in `commands`:

```console
$ agshell -c 'echo $(git rev-parse --abbrev-ref HEAD) done' --supervise-substitutions --human
  #0  $(git rev-parse --abbrev-ref HEAD)  status=128    1ms substitution
  #1  echo $(git rev-parse ...) done      status=0      1ms

class=substitution_failed exit=0
```

The output is handed to the command through a variable and the substitution's
quoting is preserved, so word splitting and globbing still follow the user's
quotes. Supervised substitutions are limited to standalone simple commands (no
heredocs, no compound statements); anything else stays flagged
`opaque_substitution: true` rather than pretending.

## What embedding the shell buys

The first version of agshell parsed with `mvdan/sh` and ran `bash -c <text>` per
command and per pipeline stage. This one links the shell instead:

- **State survives.** One shell instance, one script (or one session).
- **Builtins cost nothing.** `echo`, `cd`, `test`, `[[ ]]`, `printf`, `export` run
  in-process. Measured: ~0.1 ms per additional statement, against ~1.4 ms to
  start `bash`. Startup is ~3.9 ms vs bash's ~1.4 ms, so the win grows with the
  length of the script.
- **Pipelines are pipelines.** The shell runs the pipe graph, so streaming,
  `SIGPIPE` and `head` behave; agshell reads the per-stage statuses back from
  `PIPESTATUS` instead of gluing `os.Pipe` together.
- **Text is exact where it matters.** A statement's `text` is the source you
  wrote, sliced at statement boundaries. Individual pipeline stages are rendered
  from the parse tree — same words, normalized spacing — because brush-parser's
  per-command spans stop short of redirects.

## What it deliberately does not do

- **No new syntax.** Not one operator. bash fluency is the asset that makes an
  agent useful; this project refuses to spend it.
- **No shell implementation of its own.** The parser and evaluator are `brush`;
  agshell only decides what runs next and records the result.
- **No policy engine.** Sandboxing is a different project with good existing
  answers (`nono`, `anthropic/sandbox-runtime`, Landlock, bubblewrap).

## Known limits (PoC)

- **Delegated statements.** Subshells `( )`, `if`/`for`/`while`/`case`, functions
  and `&` are handed to the shell whole and marked `delegated: true`. Their
  internals are not visible. That is a correctness choice: a wrong status is
  worse than a coarse one.
- **Per-pipeline stderr, not per stage.** The shell owns the pipe graph, so a
  pipeline's stderr is captured as one stream and attached to its last stage.
  Stage timings are likewise reported on the last stage only.
- **`--timeout` is fatal.** A timed-out statement stops the script: the whole
  process tree is killed and the envelope says `timeout`.
- **Substitution supervision is bounded** to standalone simple commands, and its
  value crosses into the shell as UTF-8 (binary output is lossy), like `$( )` in
  a shell script that reads it back.
- **Negated pipelines.** `! cmd` reports the negated status for a single-stage
  pipeline; for a multi-stage one the per-stage statuses are `PIPESTATUS`'s
  (pre-negation), as in bash.
- **Windows** runs scripts and enforces timeouts, but the process model is
  Windows': no POSIX signals, no process groups, and `brush`'s Windows support is
  itself a preview.

## Why it exists

The long version — three measurement passes over the transcripts, what was
assumed, what was wrong, and which features died on the numbers — is in
`ideas/agent-shell-sandbox.md` in a private notebook. Short version: output
compression (RTK), sandboxing (nono/srt) and background jobs (`hc`) all have
owners now. The one thing nobody had built is a shell that reports **what
actually ran**, and two thirds of a real agent's success column is the exit
status of `head`.

## License

MIT.
