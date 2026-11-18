# Fuzz targets

Coverage-guided fuzzing for the two pure surfaces: the word scanner
(`scan`) and the parser + linter (`check`, the path `agshell --check` takes).
`corpus/` is generated and gitignored; `seeds/` holds a handful of inputs that
start the fuzzer near the interesting shapes, because random bytes almost never
spell `pwsh -Command "`.

**LibFuzzer does not run on Windows** (it needs LLVM sanitizers and a Unix-like
host), so this package is for Linux and macOS. It is its own Cargo package and
is excluded from the published crate; nothing here is built by `cargo test` at
the top level.

On a Linux box (semar, kresna), from this directory:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz

# The scanner: terminates, never panics, never reports a match it invented.
cargo +nightly fuzz run scan seeds/scan -- -max_total_time=60

# The parser and the linter: no panic on arbitrary text, deterministic findings.
cargo +nightly fuzz run check seeds/check -- -max_total_time=120
```

A crash writes its input to `artifacts/<target>/`; reproduce it with
`cargo +nightly fuzz run <target> artifacts/<target>/<file>` and keep the file in
`seeds/` as a regression seed. Corpus growth lives in `corpus/<target>/` — worth
keeping between runs, not worth committing.

## Found so far

**Open: a 10-byte script makes the parser allocate without bound and abort.**
Found by the `check` target in its first minute (2026-11-18, semar); not fixed.

```console
$ agshell --check -c '$( ( <<- )'          # also: agshell -c '$( ( <<- )'
memory allocation of 14680064 bytes failed
```

The parse loop keeps allocating (112 MB chunks in the first variant, 14 MB here)
until the allocator gives up — 2.4 GB RSS in ~8 s before the abort. libFuzzer's
original input was `F\n$( (% ($n<<- ) )`; `tmin` reduced it to
`F\n$( (% ($n<<- ) )` → `$( (% ($n<<- ) )` → this. Nothing in agshell's own code
is involved: `Session::check` calls `Shell::parse_string` first, so the growth is
in `brush-parser`'s handling of an unterminated `$(` whose body holds a `<<-`
heredoc operator with a `)` for a delimiter. `$( ( <<- ) )` and `$( ( <<- )` (a
balanced and an unbalanced variant) are both fine, so it needs the specific
unterminated shape.

Consequences worth knowing: an agent can type this by accident, and the abort
takes the process — under `--session` that is the *daemon*, and every session
keyed to that directory. Until it is fixed upstream, run the fuzzer with
`-rss_limit_mb=900` so it reports the finding instead of eating the machine.

The same file is kept on semar at
`~/Workspace/agshell-fuzz/fuzz/artifacts/check/minimized-from-4e0d9d76afb9d7ad9a7200e9f164856ca0aec991`.
It is deliberately **not** committed to `seeds/`: replaying it makes the target
die at startup, which would hide every other finding.

The same invariants run on every platform, without a fuzzer, in
`../tests/props.rs` (proptest) and in the deterministic junk passes in
`../tests/compat.rs` and `../src/scan.rs`. Those are the ones that run in
`cargo test`; this package is the extra pair of eyes when someone asks "but what
about the input nobody thought of".
