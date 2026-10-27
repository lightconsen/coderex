#![no_main]

use coderex_core::stream_utils::{Utf8ChunkDecoder, find_sse_message_boundary};
use libfuzzer_sys::fuzz_target;
use std::str;

fuzz_target!(|data: &[u8]| {
    // The decoder must tolerate arbitrary chunk boundaries and invalid UTF-8.
    let mut decoder = Utf8ChunkDecoder::new();
    let _ = decoder.push(data);
    let _ = decoder.push(data);
    let _ = decoder.finish();

    let Ok(text) = str::from_utf8(data) else {
        return;
    };
    // Walk the framing layer as the real SSE stream does. The loop is bounded
    // so a boundary function that stops advancing cannot hang the fuzzer.
    let mut rest = text;
    for _ in 0..10_000 {
        let Some(position) = find_sse_message_boundary(rest) else {
            break;
        };
        if position >= rest.len() {
            break;
        }
        rest = &rest[position + 1..];
    }
});
