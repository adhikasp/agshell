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
status. It no longer reimplements the shell to do it: [brush](https://brush.sh)
(`brush-core`) is embedded as the evaluator, so words, quoting, globbing,
arithmetic, builtins, pipelines and redirections are a real shell's job, unchanged.
agshell reports what actually ran.

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

Rust 1.88+. Timeouts kill processes, so they need Linux; elsewhere everything but
`--timeout` works.

## What embedding the shell buys

The first version of agshell parsed with `mvdan/sh` and ran `bash -c <text>` per
command and per pipeline stage. This one links the shell instead:

- **State survives.** One shell instance, one script: `cd`, `export`, functions,
  arrays and `set` options carry across statements because they are one shell, not
  one process per command. (`bash -c` per stage was a fresh shell every time.)
- **Builtins cost nothing.** `echo`, `cd`, `test`, `[[ ]]`, `printf`, `export` and
  friends run in-process. Measured: ~0.1 ms per additional statement, against
  ~1.4 ms to start `bash`. Startup is ~3.9 ms vs bash's ~1.4 ms, so the win grows
  with the length of the script.
- **Pipelines are pipelines.** The shell runs the pipe graph, so streaming,
  `SIGPIPE` and `head` behave; agshell reads the per-stage statuses back from
  `PIPESTATUS` rather than gluing `os.Pipe` together.
- **Text is exact.** Each record's `text` is the source you wrote, sliced by
  statement boundaries — not a re-render of the parse tree.
- **One parser for topology and execution.** No more "parse with A, evaluate with
  B" split brain.

## Measured

Release build, Linux x86_64, 20 runs each:

| | agshell | bash |
|---|---|---|
| `echo hi` | 3.9 ms | 1.4 ms |
| 20 builtin statements | 5.9 ms | 1.4 ms |

The old design paid a shell startup *per command*; this pays it once per script.

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
  internals are not visible. That is a correctness choice: a wrong status is worse
  than a coarse one. A `;` chain inside a delegated statement is invisible; a
  pipeline inside one still reports the status the shell gave the whole thing.
- **`$( )` is opaque.** A failure inside command substitution still disappears; the
  envelope flags `opaque_substitution: true` rather than pretending. Owning
  expansion is the next real step.
- **Per-pipeline stderr, not per stage.** The shell owns the pipe graph, so a
  pipeline's stderr is captured as one stream and attached to its last stage.
  Stage timings are likewise reported on the last stage only.
- **`--timeout` is fatal.** A timed-out statement stops the script: the whole
  process tree is killed and the envelope says `timeout`. Continuing would mean
  reusing a shell that was interrupted mid-statement.
- **No session state between invocations.** Each run is one script. The shell
  instance lives for the script and no longer.
- **Negated pipelines.** `! cmd` reports the negated status for a single-stage
  pipeline; for a multi-stage one the per-stage statuses are `PIPESTATUS`'s
  (pre-negation), as in bash.

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
