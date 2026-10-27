#![no_main]

use coderex_plugins::plugins::PluginManifest;
use libfuzzer_sys::fuzz_target;
use std::str;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = str::from_utf8(data) else {
        return;
    };
    if let Ok(manifest) = serde_json::from_str::<PluginManifest>(text) {
        if let Ok(encoded) = serde_json::to_string(&manifest) {
            let _ = serde_json::from_str::<PluginManifest>(&encoded);
        }
        let _ = manifest.unimplemented_fields();
    }
});
