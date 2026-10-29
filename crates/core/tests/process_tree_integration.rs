//! Integration tests for process-tree termination.
//!
//! The unit tests kill a single child; these spawn a real shell with a
//! background grandchild, which is the case a hook or an MCP server creates
//! and the reason the tree exists.

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use coderex_core::process_tree::{JobLimits, ProcessTree};

fn alive(pid: u32) -> bool {
    Command::new("ps")
        .args(["-p", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn wait_until_dead(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn spawn_shell_with_grandchild() -> (ProcessTree, std::process::Child, u32) {
    let tree = ProcessTree::create(JobLimits::default()).expect("process tree");
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg("sleep 60 & echo $!; wait")
        .stdout(Stdio::piped());
    tree.prepare(&mut command);
    let mut child = command.spawn().expect("spawn shell");
    tree.attach(&child).expect("attach");

    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("read grandchild pid");
    let grandchild: u32 = line.trim().parse().expect("pid");
    (tree, child, grandchild)
}

#[test]
fn terminate_kills_the_child_and_its_grandchild() {
    let (tree, mut child, grandchild) = spawn_shell_with_grandchild();
    let shell = child.id();
    assert!(alive(shell), "the shell is running");
    assert!(alive(grandchild), "the background job is running");

    assert!(tree.terminate(), "the tree was owned");
    let _ = child.wait();

    assert!(wait_until_dead(shell), "the shell is gone");
    assert!(wait_until_dead(grandchild), "the grandchild is gone");
}

#[test]
fn ownership_ends_without_terminate_only_when_disarmed() {
    // The Unix tree kills on `terminate`, not on drop: the caller that owns
    // the policy (the hook runner) decides when the tree is done. Dropping it
    // unarmed leaves the processes alone, which is why `HookTree` calls
    // `terminate` from its own `Drop`.
    let (tree, mut child, grandchild) = spawn_shell_with_grandchild();
    // On Linux `ProcessTree` has no `Drop` (its owner calls `terminate`), so
    // clippy sees a no-op here; the move is still what the test is about.
    #[allow(clippy::drop_non_drop)]
    drop(tree);
    let _ = child.kill();
    let _ = child.wait();

    std::thread::sleep(Duration::from_millis(200));
    assert!(alive(grandchild), "a dropped tree issues no kill");
    let _ = Command::new("kill")
        .args(["-9", &grandchild.to_string()])
        .status();
}

#[test]
fn a_disarmed_tree_leaves_the_grandchild_running() {
    let (tree, mut child, grandchild) = spawn_shell_with_grandchild();
    tree.disarm();
    assert!(!tree.terminate(), "a disarmed tree issues no kill");
    let _ = child.kill();
    let _ = child.wait();

    std::thread::sleep(Duration::from_millis(200));
    assert!(
        alive(grandchild),
        "the grandchild is not ours once disarmed"
    );
    // The test owns it now.
    let _ = Command::new("kill")
        .args(["-9", &grandchild.to_string()])
        .status();
}

#[test]
fn from_pid_terminates_a_process_started_elsewhere() {
    let mut child = Command::new("sleep")
        .arg("60")
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn sleep");
    let pid = child.id();
    assert!(alive(pid));

    let tree = ProcessTree::from_pid(pid).expect("adopt");
    assert!(tree.terminate(), "the kill was issued");

    // Reap the child before asking `ps`: an unreaped process is still a
    // process, and SIGTERM does not remove it from the table.
    let _ = child.wait().expect("reap");
    assert!(wait_until_dead(pid), "the adopted process is gone");
}
