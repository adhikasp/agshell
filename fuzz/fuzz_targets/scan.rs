#![no_main]
//! The quote-aware word scanner on arbitrary text. Its first version spun forever
//! on a space inside double quotes, so "terminates, never panics, never reports a
//! match outside the source" is the property worth fuzzing at this level.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };

    let hit = agshell::scan::foreign_script_expansion(source);

    if let Some(hit) = hit {
        assert!(hit.word.starts_with('"'), "{source:?}: {hit:?}");
        assert!(
            hit.word.contains('$') || hit.word.contains('`'),
            "{source:?}: {hit:?}"
        );
        assert!(source.contains(&hit.word), "{source:?}: {hit:?}");
        assert!(
            hit.interpreter == "pwsh" || hit.interpreter == "powershell",
            "{source:?}: {hit:?}"
        );
    }
});
