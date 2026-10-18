//! Hidden helper that compiles one WASM module out-of-process.
//!
//! Usage: `coderex-wasm-compile <input> <output>`
//!
//! The plugin runtime looks for this binary next to its own executable and
//! runs it as a child, so the parent can install hard CPU/memory limits and a
//! wall-clock timeout around wasmtime compilation. The output file is
//! `wasmtime::Module::serialize` bytes; the parent deserializes it.
//!
//! This binary is an implementation detail and is not meant to be invoked by
//! users.

fn main() {
    if let Err(err) = run() {
        eprintln!("coderex-wasm-compile: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args_os().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        return Err("usage: coderex-wasm-compile <input> <output>".into());
    };
    if args.next().is_some() {
        return Err("usage: coderex-wasm-compile <input> <output>".into());
    }

    // Test-only hook (debug builds): lets the parent's timeout/kill path be
    // exercised against a helper that never finishes.
    #[cfg(debug_assertions)]
    sleep_for_test();

    let bytes = std::fs::read(&input)
        .map_err(|e| format!("failed to read '{}': {e}", input.to_string_lossy()))?;
    let serialized = coderex_plugins::wasm_runtime::compile_to_serialized(&bytes)?;
    std::fs::write(&output, serialized)
        .map_err(|e| format!("failed to write '{}': {e}", output.to_string_lossy()))?;
    Ok(())
}

#[cfg(debug_assertions)]
fn sleep_for_test() {
    const VAR: &str = "CODEREX_WASM_COMPILE_TEST_SLEEP_MS";
    if let Ok(raw) = std::env::var(VAR) {
        if let Ok(millis) = raw.parse::<u64>() {
            std::thread::sleep(std::time::Duration::from_millis(millis));
        }
    }
}
