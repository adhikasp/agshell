# agshell

A supervised execution shell for coding agents. **Proof of concept.**

bash reports one exit status per command string. Agents mostly don't run single
commands — measured over 37,700 real agent shell calls, only **21%** are a single
simple command: 62% contain a pipeline, 51% a `;` chain, 28% an `&&` chain. So for
roughly four commands in five, the status an agent reads is *structurally
incapable* of describing what happened. `a; b; c` returns `c`'s status. `PIPESTATUS`
covers the end of one pipeline and nothing else — not the chains around it, not
`$( )`, not a stage that was killed.

agshell keeps bash as the *evaluator* and takes over the *topology*. It parses the
script with [`mvdan/sh`](https://github.com/mvdan/sh), then starts each simple
command and each pipeline stage itself, as a supervised child with its own fds,
process group and timer. bash still does words, quoting, globbing and arithmetic;
the shell reports what actually ran.

## What that buys

```console
$ bash -c 'false; echo hi; true'; echo "exit=$?"
hi
exit=0                       # the failure is gone

$ agshell -c 'false; echo hi; true' --human
$ false; echo hi; true

  #0  false                              status=1     2ms
  #1  echo hi                            status=0     2ms
  #2  true                               status=0     2ms

class=chain_partial exit=0 hidden_failures=[0]
```

```console
$ bash -c 'false | head -1'; echo "exit=$?"
exit=0                       # pipefail would catch this one, no chain would

$ agshell -c 'false | head -1' --human
  #0  false                              status=1     2ms pipe1.0
  #1  head -1                            status=0     2ms pipe1.1

class=stage_failed exit=0
```

The envelope, not the text, is the interface:

```json
{
  "class": "stage_failed",
  "exit": 0,
  "duration_ms": 7,
  "commands": [
    {"i": 0, "text": "false",   "bin": "false",   "status": 1, "ms": 2, "pipeline": 1, "stage": 0},
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
| `timeout` | a command was killed at `--timeout`; the process **group** is signalled |
| `parse_error` | bash itself would not have parsed it |

`exit` stays bash-compatible, so agshell can sit under an existing harness
unchanged.

## Build

```console
$ go build -o agshell .
$ ./agshell -c 'go test ./... 2>&1 | tail -5'        # JSON envelope
$ ./agshell -c 'make' --human --timeout 5m           # report
$ echo '<script>' | ./agshell                        # positional form works too
```

Requires Linux (process groups, `Setpgid`, signal delivery to the group).

## Measured against `pipefail`

`set -o pipefail` is the usual answer. It only helps when a failing stage is the
last thing evaluated, which is rarely how agents write commands. Same cases, both
runners, real exit codes:

| command | bash | `-o pipefail` | agshell |
|---|---|---|---|
| `false; echo hi; true` | 0 | 0 | `chain_partial` |
| `false \| head -1` | 0 | 1 | `stage_failed` |
| `false \| head -1; true` | 0 | **0** | `stage_failed` |
| `true; false; echo done` | 0 | 0 | `chain_partial` |
| `cd /nope && ls; echo done` | 0 | 0 | `chain_partial` |
| `grep nomatch /etc/hosts; echo x` | 0 | 0 | `chain_partial` |
| `echo ok` | 0 | 0 | `ok` |

Adding `; true` after a failing pipeline defeats `pipefail`. Nothing defeats
chains except a shell that runs the chain.

## What it deliberately does not do

- **No new syntax.** Not one operator. bash fluency is the asset that makes an
  agent useful; this project refuses to spend it.
- **No bash implementation.** Every simple command is handed to `bash -c
  <original text>` in its own process. Globbing, quoting, parameter expansion,
  heredocs, arithmetic: all bash's job, unchanged.
- **No policy engine.** Sandboxing is a different project with good existing
  answers (`nono`, `anthropic/sandbox-runtime`, Landlock, bubblewrap).

## Known limits (PoC)

- **Delegated statements.** Subshells `( )`, `if`/`for`/`while`/`case`, `!`, `&`
  and any statement with its own redirection are passed to bash whole and marked
  `delegated: true`. Their internals are not visible. That is a correctness
  choice: a wrong status is worse than a coarse one.
- **`$( )` is opaque.** A failure inside command substitution still disappears;
  the envelope flags `opaque_substitution: true` rather than pretending. Making
  that visible requires owning the expansion, which is the next real step.
- **Per-stage stdout bytes are not counted** for non-final stages (they go
  straight into the pipe). stderr is captured per stage.
- **No session state.** Each invocation is one script; no persistent cwd or
  environment yet.
- `--timeout` is per command, not per script.

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
