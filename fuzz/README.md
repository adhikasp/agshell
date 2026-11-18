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
# ASan needs headroom for its shadow and quarantine; a tight -rss_limit_mb makes
# it report an OOM that is not in the code.
ASAN_OPTIONS=quarantine_size_mb=64 cargo +nightly fuzz run check seeds/check \
  -- -max_total_time=120 -rss_limit_mb=2048
```

A crash writes its input to `artifacts/<target>/`; reproduce it with
`cargo +nightly fuzz run <target> artifacts/<target>/<file>` and keep the file in
`seeds/` as a regression seed. Corpus growth lives in `corpus/<target>/` — worth
keeping between runs, not worth committing.

## Found so far

**Found and fixed: a 10-byte script made the parser allocate without bound and
abort.** The `check` target found it in its first minute (2026-11-18, semar).

```console
$ agshell --check -c '$( ( <<- )'          # before the fix: 2.4 GB, then abort
memory allocation of 14680064 bytes failed
```

The parse loop kept allocating (112 MB chunks in the first variant, 14 MB here)
until the allocator gave up — 2.4 GB RSS in ~8 s. libFuzzer's original input was
`F\n$( (% ($n<<- ) )`; `tmin` reduced it to that 10-byte script. Nothing in
agshell's own code was involved: `Session::check` calls `Shell::parse_string`
first, so the growth was `brush-parser` on an unterminated `$(` whose body holds
a `<<-` heredoc operator with `)` for a delimiter.

**Fixed upstream, and ported.** `reubeno/brush` fixed this in
`7dc43800` ("improve tokenizer handling of here-docs and nested expansions",
#1416, with a snapshot test named
`parse_here_doc_left_open_in_nested_command_substitution` — the same shape), but
released nothing past `brush-parser 0.4.0`. The fork therefore carries a port:
`adhikasp/brush@d7cc2189` = the Windows `.cmd`/`.bat` exec patch + that tokenizer
fix (the cache macro, which needs `cached` 2.x, is dropped instead of bumping the
dependency). Verified on semar with a parser probe under a 900 MB cap: all three
variants return `parse error: missing here-document tag before ")"` in 0.00 s, and
a *valid* heredoc inside `$( )` still parses. agshell 0.5.3 pins it.

Re-run this target with `-rss_limit_mb=900` so a memory bug is reported as a
finding instead of taking the machine. Both the 10-byte and the original 18-byte
input are now regression seeds in `seeds/check/`: with the fix in place they are
ordinary parse errors, so the target replays them on every run.

A libFuzzer OOM is a claim about the process, not the input: before believing it,
replay it against the plain release binary (`agshell --check -c "$(cat
artifacts/check/<file>)"`, or a probe) under a memory cap. A tight
`-rss_limit_mb` with ASan's default quarantine produces false ones — 1029 MB on
the 10-byte seed that the plain build answers in 7 MB. Use
`-rss_limit_mb=2048` and `ASAN_OPTIONS=quarantine_size_mb=64`.

**Found and fixed: a panic on a huge io-number.** Twenty seconds into the next
run (with the here-doc fix in place) the same target found

```console
$ agshell --check -c 'echo 88888888888888888888<8888'
thread '<unnamed>' panicked at brush-parser/src/parser/peg.rs:695:27:
called `Result::unwrap()` on an `Err` value: ParseIntError { kind: PosOverflow }
```

— a redirection's io-number parsed into an integer without a range check. A panic
in the parse path is a lost tool call, so it got the same treatment: upstream had
already fixed it in `48cbaed5` (#1271, "decline out-of-range numeric values
instead of unwrapping"), released nowhere, so the fork ports it too
(`f79bc762`). The input is `seeds/check/huge-io-number`.

The same invariants run on every platform, without a fuzzer, in
`../tests/props.rs` (proptest) and in the deterministic junk passes in
`../tests/compat.rs` and `../src/scan.rs`. Those are the ones that run in
`cargo test`; this package is the extra pair of eyes when someone asks "but what
about the input nobody thought of".
