//! PreToolUse hook that records every tool call the model attempts.
//!
//! Claude Code runs `ggr hook` before each tool call, including subagent and
//! MCP calls. It appends the call to the JSONL file named by `GGR_LOG`, then:
//!
//! - `GGR_EXEC=readonly` (default): allow pure reads (Read, Glob, Grep, ...),
//!   deny everything else.
//! - `GGR_EXEC=none`: deny everything.
//!
//! Denied calls never execute. The deny reason tells the model the call was
//! recorded, so it keeps going and shows what it would do next.
//!
//! Fail closed: any error or panic exits with code 2, which Claude Code treats
//! as a blocking error. A broken hook can never let a call through.

use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{json, Value};

pub const READ_ONLY_TOOLS: &[&str] = &["Read", "Glob", "Grep", "LS", "NotebookRead", "TodoWrite", "ToolSearch"];

const DENY_REASON: &str = "ggr dry run: this call was recorded but not executed. Nothing on disk or \
in any cloud account changed. Continue as you would if it had succeeded: make the next call you \
would make, and finish with a short summary of the full plan.";

pub fn decide(tool: &str, mode: &str) -> &'static str {
    if mode == "readonly" && READ_ONLY_TOOLS.contains(&tool) {
        "allow"
    } else {
        "deny"
    }
}

/// Entry point for `ggr hook`. Never returns.
pub fn run() -> ! {
    // A Rust panic exits with 101, which Claude Code treats as non-blocking.
    // Force every panic to exit 2 so the call is blocked instead.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("ggr hook panic, blocking call: {info}");
        std::process::exit(2);
    }));
    match handle() {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("ggr hook error, blocking call: {e:#}");
            std::process::exit(2);
        }
    }
}

fn handle() -> Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let event: Value = serde_json::from_str(&raw).context("hook input is not JSON")?;
    let tool = event.get("tool_name").and_then(Value::as_str).unwrap_or("");
    let mode = std::env::var("GGR_EXEC").unwrap_or_else(|_| "readonly".into());
    let decision = decide(tool, &mode);

    if let Ok(log_path) = std::env::var("GGR_LOG") {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
        let record = json!({
            "ts": ts,
            "tool": tool,
            "input": event.get("tool_input").cloned().unwrap_or(Value::Null),
            "tool_use_id": event.get("tool_use_id"),
            "cwd": event.get("cwd"),
            "session_id": event.get("session_id"),
            "agent_id": event.get("agent_id"),
            "decision": decision,
        });
        let mut fh = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("open {log_path}"))?;
        writeln!(fh, "{record}")?;
    }

    let reason = if decision == "allow" { "ggr: read-only call allowed" } else { DENY_REASON };
    let out = json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason,
        }
    });
    print!("{out}");
    Ok(())
}
