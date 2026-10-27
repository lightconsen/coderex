#![no_main]

use coderex_config::settings::Settings;
use libfuzzer_sys::fuzz_target;
use std::str;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = str::from_utf8(data) else {
        return;
    };
    if let Ok(settings) = serde_json::from_str::<Settings>(text) {
        let _ = settings.validate();
        if let Ok(encoded) = serde_json::to_string(&settings) {
            let _ = serde_json::from_str::<Settings>(&encoded);
        }
    }
});
