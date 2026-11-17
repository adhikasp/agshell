# agshell

[![crates.io](https://img.shields.io/crates/v/agshell.svg)](https://crates.io/crates/agshell)
[![docs.rs](https://docs.rs/agshell/badge.svg)](https://docs.rs/agshell)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Your agent's shell says `exit 0`. Often, something in the command failed.**

agshell is a drop-in supervised shell for coding agents. It runs the same bash your agent already writes, but reports what *actually happened*: the status of every command in a chain, every stage of a pipeline, and every `$( )`, as typed JSON instead of one exit code.

```console
$ bash -c 'false | head -1'; echo "exit=$?"
exit=0                                   # the agent concludes: success

$ agshell -c 'false | head -1' --human
  #0  false      status=1   pipe1.0
  #1  head -1    status=0   pipe1.1

class=stage_failed exit=0                # the agent learns: stage 0 failed
```

> Proof of concept, MIT, one Rust binary, tested on Linux and Windows. Built on [brush](https://brush.sh), a real bash-compatible shell embedded as a library. No new syntax to teach your agent.

## The problem nobody sees

Agents don't run single commands. We measured **37,700 real agent shell calls**:

| What the command looked like | Share |
|---|---|
| a single simple command (the only case one exit code can describe) | **21%** |
| contains a pipeline (`a \| b`) | **62%** |
| a `;` chain (`a; b; c`) | **51%** |
| an `&&` chain | **28%** |

So for about four commands in five, the status the agent reads is structurally unable to describe what happened. `a; b; c` returns `c`'s status. `false | head -1` returns `head`'s. Without `pipefail`, **64% of "successful" calls in the corpus were vouched for only by the last stage of a pipeline**, usually `head`, `grep` or `awk`. And **about 10% of exit-0 calls had printed an error line**. The agent noticed 9 times in 31,000 calls.

The result is not noise. It is wrong conclusions, stated confidently: "tests pass", "file written", "no matches", from a command that failed three stages ago.

## What it does instead

```console
$ bash -c 'false; echo hi; true'; echo "exit=$?"
hi
exit=0                                   # the failure is gone

$ agshell -c 'false; echo hi; true' --human
  #0  false      status=1
  #1  echo hi    status=0
  #2  true       status=0

class=chain_partial exit=0 hidden_failures=[0]
```

Every run returns an envelope your harness can branch on:

```json
{
  "class": "stage_failed",
  "exit": 0,
  "duration_ms": 2,
  "commands": [
    {"i": 0, "text": "false",   "bin": "false", "status": 1, "ms": 0, "pipeline": 1, "stage": 0},
    {"i": 1, "text": "head -1", "bin": "head",  "status": 0, "ms": 2, "pipeline": 1, "stage": 1}
  ],
  "hidden_failures": [0],
  "stdout": "", "stdout_bytes": 0
}
```

`exit` stays bash-compatible, so agshell can slide under an existing harness without breaking anything. Then you opt in to the truth:

| `class` | what the agent is told |
|---|---|
| `ok` | everything exited 0 and the output looks clean |
| `ok_noisy` | exited 0, **but printed something that reads like an error** |
| `chain_partial` | exited 0, **an earlier command in the chain failed** |
| `stage_failed` | exited 0, **a pipeline stage failed** |
| `substitution_failed` | exited 0, **a `$( )` failed** |
| `failed` | the final status was non-zero |
| `timeout` | killed at `--timeout`, **whole process tree**, no orphans |
| `parse_error` | bash itself would not have parsed it |

## What makes it different

**It reports truth without changing behaviour.** `set -e` and `pipefail` change what a script *does*. agshell changes only what the agent is *told*. Same commands, same side effects, honest status.

**It sees inside `if`, loops, functions, subshells and `eval`.** Other tools stop at the statement boundary: a `for` loop is one opaque unit with one exit code. agshell watches the shell's own `DEBUG` and `ERR` traps while the statement runs, so a failure three iterations deep is reported, and a failure bash already handled (an `if` condition, a `||`, a `!`) is not.

```console
$ ls
a.txt                                    # b.txt does not exist
$ agshell -c 'for f in a b; do cat $f.txt; echo "checked $f"; done' --human
  #0  cat a.txt          status=0   in #4
  #1  echo checked a     status=0   in #4
  #2  cat b.txt          status=1   in #4
  #3  echo checked b     status=0   in #4 inferred
  #4  for f in a b; ...  status=0   delegated
       stderr: cat: b.txt: No such file or directory

class=chain_partial exit=0 hidden_failures=[2]
```

bash says `exit 0` and prints "checked b" anyway. agshell says which `cat` failed. No control flow is re-implemented: brush still decides what runs, and agshell reads what it announces. Reported commands carry their *expanded* words (`echo item 7`, not `echo item $i`). The same mechanism sees into `$( )` and backticks wherever they sit, with no flag: `echo $(git rev-parse HEAD) done` exits 0 in bash even when git fails, and comes back as `class=substitution_failed` here, with the git command and its status in `commands`.

**`--strict`: the `set -e` that actually works.** `set -e` does not fire on `a | b`, on earlier commands in a `;` chain, or inside `$( )`. `--strict` stops at the first failed statement, masked pipeline stage, failed substitution, or unhandled failure inside a loop body or function. `false || true` is left alone, because `||` handled it.

```console
$ agshell -c 'echo ok; false; echo never' --strict --human
class=failed exit=1 aborted_at=1         # "never" never ran
```

**`--check`: lint before you spend the tool call.** It parses without executing and tells the agent what its own script is about to hide: `dropped_status`, `masked_stage`, `negated_status`, `wont_persist` (a `cd` inside a pipeline subshell), `unquoted_substitution`, `opaque_substitution`.

**`serve`: one live shell, with undo.** A JSON-lines protocol over stdin/stdout. State (`cd`, `export`, functions, arrays) persists across calls, because it is one shell and not one process per command.

```console
$ agshell serve
{"id":1,"op":"run","script":"cd /tmp; x=41"}
{"id":2,"op":"snapshot","key":"before-build"}
{"id":3,"op":"run","script":"x=99; cd /"}
{"id":4,"op":"restore","snapshot":"before-build"}     # cwd and variables rewind
{"id":5,"op":"branch","scripts":["npm test","pnpm test"]}   # two isolated forks, concurrently
```

- **Checkpoints** are cheap clones of persistent data structures. `restore` rewinds shell state (cwd, vars, functions, options). It does not un-write files.
- **Branches** run each script on an isolated fork. A branch's `cd` or `export` cannot leak into the session or another branch.
- **20 runs in 8 ms**, against about 3.9 ms of startup each if you spawn a shell per call. Builtins cost ~0.1 ms.

**Timeouts that actually kill things.** The default is **10 m per statement** (`--timeout` overrides it, `0` disables it). A timeout takes down the entire process tree (subshells, nested `bash -c`, loops, pipelines), verified with zero leftovers. Linux uses subreaper plus a `/proc` walk; Windows uses a Toolhelp32 snapshot plus `TerminateProcess`.

**It's a library too.**

```rust
let mut session = agshell::Session::new(Options::default()).await?;
let report = session.run("false | head -1").await;
assert_eq!(report.class, "stage_failed");
assert_eq!(report.commands[0].status, 1);     // typed, not text-parsed
```

## Try it

```console
$ cargo install agshell                       # Rust 1.88+, Linux and Windows
$ agshell -c 'go test ./... 2>&1 | tail -5'    # JSON envelope
$ agshell -c 'make' --human --timeout 5m       # readable report
$ agshell --check -c 'cd /tmp; false | head -1; echo done'
```

On Windows, install from git so the pinned `brush` fork carrying the `.cmd` exec fix is applied
(see the limits below). Cargo does the build; there is nothing to clone by hand:

```console
$ cargo install --git https://github.com/adhikasp/agshell --locked
```

### Wire it into your agent

1. **Replace the shell tool's backend.** Run `agshell -c "$SCRIPT"` and return the JSON. Keep your existing interface; use `exit` as before.
2. **Surface the class.** Prepend one line to the tool result when `class` is not `ok`, for example `chain_partial: command #0 (false) failed`. That single line is the whole intervention.
3. **Or keep a session.** Spawn `agshell serve` once and speak JSON lines for the whole conversation: persistent `cd`, checkpoints before risky steps, branches to try two approaches.

## Why not just…

| | | |
|---|---|---|
| `set -o pipefail` | fixes pipelines only, changes script behaviour, silent on chains and `$( )` | agshell reports all of them without altering execution |
| `set -e` | misses pipelines, chains, substitutions; famously inconsistent | `--strict` covers all three |
| a hook or regex on output | text matching on exit-0 output is how errors slip by | structural: reads the parse tree and real statuses |
| a sandbox (`srt`, `nono`, Landlock) | enforces *what may run* | complementary: agshell reports *what did run*. Use both |
| output compressors (RTK) | save tokens | complementary: they shrink the output, agshell makes it truthful |

## Deliberate non-goals

- **No new syntax.** Not one operator. bash fluency is what makes your agent useful; agshell refuses to spend it.
- **No shell of its own.** Parsing and evaluation are `brush`. agshell decides what runs next and records the result.
- **No policy engine.** Sandboxing has good answers already. See the table above.

## Honest limits (PoC)

- Compound statements are run by the shell and observed through its traps, so the numbers are as good as what the shell announces. The last command of a context, when nothing failed, has its status *assumed* (`inferred: true`). A standalone `[[ ]]` or `(( ))` is not announced by the shell, so its failure is reported as `(failure at or after: ...)` instead of being pinned on a neighbour. A statement keeps at most 200 ordinary inner commands (`inner_omitted` says how many were dropped); unhandled failures are never dropped. Your own `DEBUG`/`ERR` traps are suspended while such a statement runs.
- Background jobs (`cmd &`) and pipelines with a compound stage (`{ ...; } | cat`) are still handed over whole and marked `delegated: true`. Coarse, but correct, and a wrong status is worse than a coarse one.
- Stderr and timing are reported per pipeline, not per stage, because the shell owns the pipe graph.
- A `$( )` in a pipeline stage (`echo $(cmd) | grep x`) or a background job is not observed and is flagged `opaque_substitution`. Everywhere else, in a lone command, a heredoc, a loop body or a function, it is.
- `brush`'s Windows support is a preview; timeouts and scripts work, POSIX signals and process groups do not exist there.
- **Windows installs from crates.io** resolve `.cmd`/`.bat` programs (`npm`, `npx`, `opencode`, ...) with the unpatched `brush-core` resolver, so they fail with `os error 193`. The fix is a pinned `brush` fork, and cargo does not publish `[patch]` sections; use `cargo install --git https://github.com/adhikasp/agshell` until it lands upstream.
- Next up: an exec observer in `brush-core` for real `argv`, per-stage pids and a policy seam. That is an upstream change.

## Under the hood

Parse with `brush-parser`, evaluate with embedded `brush-core`, walk the statement list yourself. For statements the shell runs whole, install a `DEBUG`/`ERR` trap that calls a builtin of ours, and read the log back. One shell instance per script (or session), pipelines run by the shell with real streaming and `SIGPIPE`, per-stage statuses read back from `PIPESTATUS`. Statement text is sliced from your original source, so what the report quotes is what you wrote.

## License

MIT. Issues and "my agent hit this" reports welcome, especially real transcripts where the exit code lied.
