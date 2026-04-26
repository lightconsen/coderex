use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;

#[test]
fn interactive_e2e_tool_runs_scenario_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    )
    .unwrap();

    let responses =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/responses.json");
    let scenario = json!({
        "cwd": dir.path(),
        "model": "K2.6",
        "rows": 40,
        "cols": 120,
        "server": {
            "kind": "static",
            "responses_file": responses,
            "model": "K2.6"
        },
        "steps": [
            { "action": "wait_for_text", "text": "CodeRex v" },
            { "action": "assert_core_regions_clean" },
            { "action": "send_line", "text": "hello" },
            { "action": "wait_for_text", "text": "Hello!" },
            { "action": "assert_region_contains", "region": "message_area", "text": "Hello!" },
            { "action": "assert_region_clean", "region": "message_area" },
            { "action": "assert_region_clean", "region": "statusline" },
            { "action": "assert_region_clean", "region": "input_line" },
            { "action": "assert_region_clean", "region": "permission_bar" }
        ]
    });

    let scenario_path = dir.path().join("scenario.json");
    std::fs::write(
        &scenario_path,
        serde_json::to_vec_pretty(&scenario).unwrap(),
    )
    .unwrap();

    Command::cargo_bin("coderex-interactive-e2e")
        .unwrap()
        .arg("--scenario")
        .arg(&scenario_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("[step"));
}
