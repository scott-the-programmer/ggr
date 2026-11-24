//! Run the prompt through headless Claude Code and record what it reaches for.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::classify::Call;

#[derive(Debug, Clone)]
pub struct ProbeConfig {
    pub prompt: String,
    pub cwd: PathBuf,
    pub out_dir: PathBuf,
    pub runs: usize,
    pub parallel: usize,
    pub model: Option<String>,
    pub exec_mode: String,
    pub isolated: bool,
    pub no_mcp: bool,
    pub timeout: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub prompt: String,
    pub cwd: String,
    pub runs: usize,
    pub model: Option<String>,
    pub exec_mode: String,
    pub isolated: bool,
    pub no_mcp: bool,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub run: usize,
    pub exit_code: i32,
    pub calls: Vec<Call>,
    /// tool_use blocks in the transcript with no hook record: they ran unguarded.
    pub unguarded: Vec<Value>,
    pub assistant_text: Vec<String>,
    pub final_text: Option<String>,
    pub num_turns: Option<u64>,
    pub cost_usd: Option<f64>,
    pub model: Option<String>,
    pub available_tools: Value,
    pub skills: Value,
}

/// Quote a string for /bin/sh.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+:@".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn hook_settings() -> Result<String> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let command = format!("{} hook", shell_quote(&exe.to_string_lossy()));
    Ok(json!({
        "hooks": {
            "PreToolUse": [
                {"matcher": "*", "hooks": [{"type": "command", "command": command, "timeout": 30}]}
            ]
        }
    })
    .to_string())
}

pub fn build_command(cfg: &ProbeConfig) -> Result<Vec<String>> {
    let mut cmd: Vec<String> = [
        "claude", "-p", &cfg.prompt,
        "--output-format", "stream-json",
        "--verbose",
        "--settings", &hook_settings()?,
        // Second safety layer behind the hook: in dontAsk mode anything that
        // is not explicitly allowed gets denied instead of prompting.
        "--permission-mode", "dontAsk",
        "--permission-prompts", "none",
        "--no-session-persistence",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if cfg.exec_mode == "readonly" {
        cmd.extend(["--allowedTools".into(), "Read,Glob,Grep".into()]);
    }
    if let Some(m) = &cfg.model {
        cmd.extend(["--model".into(), m.clone()]);
    }
    if cfg.isolated {
        // Drop user-level settings (hooks, plugins, permission rules) so the
        // probe sees only the project's own config plus ours.
        cmd.extend(["--setting-sources".into(), "project,local".into()]);
    }
    if cfg.no_mcp {
        cmd.push("--strict-mcp-config".into());
    }
    Ok(cmd)
}

pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// Run a command with stdin closed, capturing output, killing it on timeout.
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start claude (is it on PATH?)")?;
    let mut out_pipe = child.stdout.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    let out_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out_pipe.read_to_string(&mut s);
        s
    });
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err_pipe.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break Some(st);
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let stdout = out_t.join().unwrap_or_default();
    let mut stderr = err_t.join().unwrap_or_default();
    if timed_out {
        stderr.push_str("\n[ggr] timed out");
    }
    Ok(Output { stdout, stderr, code: status.and_then(|s| s.code()).unwrap_or(-1) })
}

fn run_one(cfg: &ProbeConfig, argv: &[String], index: usize) -> Result<RunResult> {
    let run_dir = cfg.out_dir.join(format!("run-{index}"));
    std::fs::create_dir_all(&run_dir)?;
    let calls_path = run_dir.join("calls.jsonl");
    let _ = std::fs::remove_file(&calls_path);

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(&cfg.cwd)
        .env("GGR_LOG", &calls_path)
        .env("GGR_EXEC", &cfg.exec_mode)
        // Don't let a parent Claude Code session's state leak into the probe.
        .env_remove("CLAUDECODE");
    let out = run_with_timeout(cmd, Duration::from_secs(cfg.timeout))?;

    std::fs::write(run_dir.join("transcript.jsonl"), &out.stdout)?;
    std::fs::write(run_dir.join("stderr.txt"), &out.stderr)?;

    let calls: Vec<Call> = parse_jsonl(&std::fs::read_to_string(&calls_path).unwrap_or_default());
    let events: Vec<Value> = parse_jsonl(&out.stdout);
    let result = summarise_run(index, calls, &events, out.code);
    std::fs::write(run_dir.join("run.json"), serde_json::to_string_pretty(&result)?)?;
    Ok(result)
}

pub fn probe(cfg: &ProbeConfig) -> Result<Vec<RunResult>> {
    std::fs::create_dir_all(&cfg.out_dir)?;
    let argv = build_command(cfg)?;
    let meta = Meta {
        prompt: cfg.prompt.clone(),
        cwd: cfg.cwd.to_string_lossy().into(),
        runs: cfg.runs,
        model: cfg.model.clone(),
        exec_mode: cfg.exec_mode.clone(),
        isolated: cfg.isolated,
        no_mcp: cfg.no_mcp,
        command: argv.clone(),
    };
    std::fs::write(cfg.out_dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;

    let queue = Arc::new(Mutex::new((1..=cfg.runs).collect::<Vec<_>>().into_iter()));
    let results: Arc<Mutex<Vec<RunResult>>> = Arc::default();
    std::thread::scope(|scope| {
        for _ in 0..cfg.parallel.max(1).min(cfg.runs.max(1)) {
            let (queue, results, argv) = (queue.clone(), results.clone(), &argv);
            scope.spawn(move || loop {
                let Some(i) = queue.lock().unwrap().next() else { break };
                match run_one(cfg, argv, i) {
                    Ok(r) => {
                        let status = if r.exit_code == 0 { "ok".to_string() } else { format!("exit {}", r.exit_code) };
                        eprintln!("  run {}: {} tool calls, {status}", r.run, r.calls.len());
                        if !r.unguarded.is_empty() {
                            eprintln!("  WARNING run {}: {} tool calls bypassed the hook", r.run, r.unguarded.len());
                        }
                        results.lock().unwrap().push(r);
                    }
                    Err(e) => eprintln!("  run {i}: failed: {e:#}"),
                }
            });
        }
    });
    let mut results = Arc::try_unwrap(results).unwrap().into_inner().unwrap();
    results.sort_by_key(|r| r.run);
    Ok(results)
}

pub fn summarise_run(index: usize, calls: Vec<Call>, events: &[Value], code: i32) -> RunResult {
    let mut tool_uses = Vec::new();
    let mut texts = Vec::new();
    let mut init: Option<&Value> = None;
    let mut fin: Option<&Value> = None;
    for ev in events {
        match ev.get("type").and_then(Value::as_str) {
            Some("system") if ev.get("subtype").and_then(Value::as_str) == Some("init") => init = Some(ev),
            Some("assistant") => {
                let blocks = ev.pointer("/message/content").and_then(Value::as_array);
                for b in blocks.into_iter().flatten() {
                    match b.get("type").and_then(Value::as_str) {
                        Some("tool_use") => tool_uses.push(json!({
                            "id": b.get("id"), "name": b.get("name"), "input": b.get("input"),
                        })),
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                                texts.push(t.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("result") => fin = Some(ev),
            _ => {}
        }
    }
    let hooked: HashSet<&str> = calls.iter().filter_map(|c| c.tool_use_id.as_deref()).collect();
    let unguarded = tool_uses
        .into_iter()
        .filter(|u| !u.get("id").and_then(Value::as_str).is_some_and(|id| hooked.contains(id)))
        .collect();
    let get = |v: Option<&Value>, k: &str| v.and_then(|v| v.get(k)).cloned();
    RunResult {
        run: index,
        exit_code: code,
        calls,
        unguarded,
        assistant_text: texts,
        final_text: get(fin, "result").and_then(|v| v.as_str().map(String::from)),
        num_turns: get(fin, "num_turns").and_then(|v| v.as_u64()),
        cost_usd: get(fin, "total_cost_usd").and_then(|v| v.as_f64()),
        model: get(init, "model").and_then(|v| v.as_str().map(String::from)),
        available_tools: get(init, "tools").unwrap_or(Value::Null),
        skills: get(init, "skills").unwrap_or(Value::Null),
    }
}

pub fn parse_jsonl<T: serde::de::DeserializeOwned>(text: &str) -> Vec<T> {
    text.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect()
}

pub fn load_runs(out_dir: &Path) -> Result<(Meta, Vec<RunResult>)> {
    let meta: Meta = serde_json::from_str(
        &std::fs::read_to_string(out_dir.join("meta.json")).with_context(|| format!("no meta.json in {}", out_dir.display()))?,
    )?;
    let mut runs = Vec::new();
    for entry in std::fs::read_dir(out_dir)? {
        let p = entry?.path().join("run.json");
        if p.is_file() {
            runs.push(serde_json::from_str::<RunResult>(&std::fs::read_to_string(&p)?)?);
        }
    }
    runs.sort_by_key(|r| r.run);
    Ok((meta, runs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_tool_use_without_hook_record() {
        let events = vec![
            json!({"type": "system", "subtype": "init", "model": "m"}),
            json!({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "id": "a", "name": "Bash", "input": {}},
                {"type": "tool_use", "id": "b", "name": "Bash", "input": {}},
            ]}}),
            json!({"type": "result", "result": "done", "total_cost_usd": 0.1}),
        ];
        let calls = vec![Call { tool_use_id: Some("a".into()), ..Default::default() }];
        let r = summarise_run(1, calls, &events, 0);
        assert_eq!(r.unguarded.len(), 1);
        assert_eq!(r.final_text.as_deref(), Some("done"));
        assert_eq!(r.model.as_deref(), Some("m"));
    }

    #[test]
    fn quotes_paths() {
        assert_eq!(shell_quote("/usr/bin/ggr"), "/usr/bin/ggr");
        assert_eq!(shell_quote("/a b/it's"), r"'/a b/it'\''s'");
    }
}
