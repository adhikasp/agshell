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

The same invariants run on every platform, without a fuzzer, in
`../tests/props.rs` (proptest) and in the deterministic junk passes in
`../tests/compat.rs` and `../src/scan.rs`. Those are the ones that run in
`cargo test`; this package is the extra pair of eyes when someone asks "but what
about the input nobody thought of".
