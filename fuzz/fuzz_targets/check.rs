#![no_main]
//! The parser and the linter on arbitrary text: the path `agshell --check` takes,
//! minus the shell. A panic here is a crash an agent would see as a lost tool
//! call, so the target is noise-sensitive on purpose.

use std::cell::RefCell;

use libfuzzer_sys::fuzz_target;

thread_local! {
    /// One session per fuzz thread. Creating it is the expensive part; the parse
    /// and the walk are what we are here to fuzz.
    static SESSION: RefCell<Option<agshell::Session>> = const { RefCell::new(None) };
}

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };

    SESSION.with(|cell| {
        let mut slot = cell.borrow_mut();
        let session = slot.get_or_insert_with(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            runtime
                .block_on(agshell::Session::new(agshell::Options::default()))
                .expect("session")
        });

        let report = session.check(source);

        // Invariants that must hold for every input, not just the parsed ones.
        let again = session.check(source);
        assert_eq!(
            report.findings.len(),
            again.findings.len(),
            "the linter is not deterministic for {source:?}"
        );
        for finding in &report.findings {
            assert!(!finding.detail.is_empty(), "{source:?}: {finding:?}");
        }
    });
});
