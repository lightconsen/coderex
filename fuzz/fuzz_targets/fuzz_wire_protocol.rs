#![no_main]

use coderex_core::wire_protocol::{decode_event, decode_request, encode_event, encode_request};
use libfuzzer_sys::fuzz_target;
use std::str;

fuzz_target!(|data: &[u8]| {
    let Ok(line) = str::from_utf8(data) else {
        return;
    };
    if let Some(Ok(event)) = decode_event(line) {
        let encoded = encode_event(&event);
        // A decoded event must re-encode into a line this parser accepts.
        assert!(decode_event(&encoded).is_some_and(|decoded| decoded.is_ok()));
    }
    if let Some(Ok(request)) = decode_request(line) {
        let encoded = encode_request(&request);
        assert!(decode_request(&encoded).is_some_and(|decoded| decoded.is_ok()));
    }
});
