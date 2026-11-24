//! Run the real binary as a hook and check it records and blocks correctly.

use std::io::Write;
use std::process::{Command, Stdio};

fn run_hook(input: &str, mode: &str, log: &std::path::Path) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ggr"))
        .arg("hook")
        .env("GGR_LOG", log)
        .env("GGR_EXEC", mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn event(tool: &str) -> String {
    serde_json::json!({
        "tool_name": tool,
        "tool_input": {"command": "az group delete -n x"},
        "tool_use_id": "t1",
    })
    .to_string()
}

fn decision(out: &std::process::Output) -> String {
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v["hookSpecificOutput"]["permissionDecision"].as_str().unwrap().to_string()
}

fn tmp_log(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("ggr-test-{}-{name}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn bash_is_denied_and_logged() {
    let log = tmp_log("bash");
    let out = run_hook(&event("Bash"), "readonly", &log);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(decision(&out), "deny");
    let line = std::fs::read_to_string(&log).unwrap();
    let rec: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(rec["tool"], "Bash");
    assert_eq!(rec["tool_use_id"], "t1");
    assert_eq!(rec["decision"], "deny");
}

#[test]
fn read_is_allowed_in_readonly() {
    let out = run_hook(&event("Read"), "readonly", &tmp_log("read"));
    assert_eq!(decision(&out), "allow");
}

#[test]
fn read_is_denied_in_none() {
    let out = run_hook(&event("Read"), "none", &tmp_log("none"));
    assert_eq!(decision(&out), "deny");
}

#[test]
fn bad_input_fails_closed() {
    let out = run_hook("not json", "readonly", &tmp_log("bad"));
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn unwritable_log_fails_closed() {
    let out = run_hook(&event("Bash"), "readonly", std::path::Path::new("/nonexistent/dir/log.jsonl"));
    assert_eq!(out.status.code(), Some(2));
}
