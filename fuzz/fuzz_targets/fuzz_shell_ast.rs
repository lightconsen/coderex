#![no_main]

use coderex_core::shell_security::analyze_command;
use libfuzzer_sys::fuzz_target;
use std::str;

fuzz_target!(|data: &[u8]| {
    let Ok(command) = str::from_utf8(data) else {
        return;
    };
    let first = analyze_command(command);
    let second = analyze_command(command);
    // Analysis feeds a permission decision, so identical input must produce
    // identical segments and the same opaque classification.
    assert_eq!(first, second);
    for segment in &first.segments {
        assert!(!segment.trim().is_empty());
    }
});
