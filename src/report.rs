//! Aggregate classified ops across runs into a report.

use std::collections::{BTreeSet, HashMap};

use serde::Serialize;
use serde_json::Value;

use crate::classify::classify_call;
use crate::probe::{Meta, RunResult};

#[derive(Debug, Clone, Serialize)]
pub struct Count {
    pub name: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Signature {
    pub signature: String,
    pub kind: String,
    pub count: usize,
    pub runs: usize,
    pub share: f64,
    pub examples: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileWritten {
    pub path: String,
    pub area: Option<String>,
    pub runs: usize,
    pub preview: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Destructive {
    pub run: usize,
    pub signature: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Trajectory {
    pub run: usize,
    pub sequence: Vec<String>,
    pub final_text: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub prompt: String,
    pub cwd: String,
    pub model: Option<String>,
    pub exec_mode: String,
    pub runs: usize,
    pub total_cost_usd: f64,
    pub unguarded_calls: usize,
    pub kinds: Vec<Count>,
    pub signatures: Vec<Signature>,
    pub consensus: Vec<String>,
    pub first_moves: Vec<Count>,
    pub files_written: Vec<FileWritten>,
    pub destructive: Vec<Destructive>,
    pub trajectories: Vec<Trajectory>,
}

#[derive(Default)]
struct SigAcc {
    kind: String,
    count: usize,
    runs: BTreeSet<usize>,
    examples: Vec<String>,
    first_seen: usize,
}

fn preview(input: &Value) -> Option<String> {
    let s = input.get("content").or_else(|| input.get("new_string")).and_then(Value::as_str)?;
    Some(s.chars().take(1500).collect())
}

fn ranked(map: HashMap<String, (usize, usize)>) -> Vec<Count> {
    // (count, first_seen) -> sort by count desc, then first seen.
    let mut v: Vec<_> = map.into_iter().collect();
    v.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.1 .1.cmp(&b.1 .1)));
    v.into_iter().map(|(name, (count, _))| Count { name, count }).collect()
}

pub fn build_report(meta: &Meta, runs: &[RunResult]) -> Report {
    let n = runs.len();
    let mut sigs: HashMap<String, SigAcc> = HashMap::new();
    let mut kinds: HashMap<String, (usize, usize)> = HashMap::new();
    let mut files: Vec<(String, Option<String>, BTreeSet<usize>, Option<String>)> = Vec::new();
    let mut destructive = Vec::new();
    let mut trajectories = Vec::new();
    let mut unguarded = 0;
    let mut seen = 0usize;

    for run in runs {
        let mut seq = Vec::new();
        for call in &run.calls {
            for op in classify_call(call) {
                seen += 1;
                seq.push(op.signature.clone());
                let acc = sigs.entry(op.signature.clone()).or_insert_with(|| SigAcc { first_seen: seen, ..Default::default() });
                acc.kind = op.kind.clone();
                acc.count += 1;
                acc.runs.insert(run.run);
                if acc.examples.len() < 3 && !acc.examples.contains(&op.target) {
                    acc.examples.push(op.target.clone());
                }
                kinds.entry(op.kind.clone()).or_insert((0, seen)).0 += 1;
                if op.kind == "fs.write" || op.kind == "fs.edit" {
                    let idx = match files.iter().position(|f| f.0 == op.target) {
                        Some(i) => i,
                        None => {
                            files.push((op.target.clone(), op.area.clone(), BTreeSet::new(), None));
                            files.len() - 1
                        }
                    };
                    files[idx].2.insert(run.run);
                    if files[idx].3.is_none() {
                        files[idx].3 = preview(&call.input);
                    }
                }
                if op.destructive {
                    destructive.push(Destructive { run: run.run, signature: op.signature, target: op.target });
                }
            }
        }
        unguarded += run.unguarded.len();
        trajectories.push(Trajectory { run: run.run, sequence: seq, final_text: run.final_text.clone() });
    }

    let mut signatures: Vec<(usize, Signature)> = sigs
        .into_iter()
        .map(|(sig, a)| {
            let r = a.runs.len();
            let share = if n > 0 { (r as f64 / n as f64 * 100.0).round() / 100.0 } else { 0.0 };
            (a.first_seen, Signature { signature: sig, kind: a.kind, count: a.count, runs: r, share, examples: a.examples })
        })
        .collect();
    signatures.sort_by(|(_, a), (_, b)| {
        b.runs.cmp(&a.runs).then(b.count.cmp(&a.count)).then(a.signature.cmp(&b.signature))
    });
    let signatures: Vec<Signature> = signatures.into_iter().map(|(_, s)| s).collect();
    // Strict majority: with 2 runs, 1 run is not consensus.
    let consensus = signatures.iter().filter(|s| n > 0 && s.runs * 2 > n).map(|s| s.signature.clone()).collect();

    let mut first: HashMap<String, (usize, usize)> = HashMap::new();
    for (i, t) in trajectories.iter().enumerate() {
        if let Some(s) = t.sequence.first() {
            first.entry(s.clone()).or_insert((0, i)).0 += 1;
        }
    }

    // Stable sort keeps first-seen order among files with equal run counts.
    files.sort_by(|a, b| b.2.len().cmp(&a.2.len()));

    Report {
        prompt: meta.prompt.clone(),
        cwd: meta.cwd.clone(),
        model: meta.model.clone().or_else(|| runs.iter().find_map(|r| r.model.clone())),
        exec_mode: meta.exec_mode.clone(),
        runs: n,
        total_cost_usd: (runs.iter().filter_map(|r| r.cost_usd).sum::<f64>() * 10000.0).round() / 10000.0,
        unguarded_calls: unguarded,
        kinds: ranked(kinds),
        signatures,
        consensus,
        first_moves: ranked(first),
        files_written: files
            .into_iter()
            .map(|(path, area, r, preview)| FileWritten { path, area, runs: r.len(), preview })
            .collect(),
        destructive,
        trajectories,
    }
}

fn cell(s: &str, max: usize) -> String {
    let s = s.replace('|', "\\|").replace('\n', " ");
    if s.chars().count() > max {
        format!("{}...", s.chars().take(max - 3).collect::<String>())
    } else {
        s
    }
}

pub fn render_markdown(rep: &Report) -> String {
    let mut o: Vec<String> = vec![
        "# Probe report".into(),
        String::new(),
        format!("- Prompt: `{}`", rep.prompt),
        format!("- Working dir: `{}`", rep.cwd),
        format!("- Model: {}", rep.model.as_deref().unwrap_or("default")),
        format!("- Runs: {}, exec mode: {}, cost: ${}", rep.runs, rep.exec_mode, rep.total_cost_usd),
    ];
    if rep.unguarded_calls > 0 {
        o.push(format!(
            "- WARNING: {} tool calls bypassed the recording hook and may have executed",
            rep.unguarded_calls
        ));
    }
    o.extend(["".into(), "## Op kinds".into(), "".into(), "| kind | count |".into(), "|---|---|".into()]);
    o.extend(rep.kinds.iter().map(|k| format!("| {} | {} |", k.name, k.count)));

    o.extend([
        "".into(),
        "## Signatures".into(),
        "".into(),
        "| signature | kind | runs | calls | example |".into(),
        "|---|---|---|---|---|".into(),
    ]);
    for s in &rep.signatures {
        let ex = cell(s.examples.first().map(String::as_str).unwrap_or(""), 90);
        o.push(format!("| `{}` | {} | {}/{} | {} | `{}` |", s.signature, s.kind, s.runs, rep.runs, s.count, ex));
    }

    o.extend(["".into(), "## First move per run".into(), "".into()]);
    if rep.first_moves.is_empty() {
        o.push("- none".into());
    }
    o.extend(rep.first_moves.iter().map(|f| format!("- `{}` x{}", f.name, f.count)));

    if !rep.files_written.is_empty() {
        o.extend(["".into(), "## Files it tried to write".into(), "".into()]);
        for f in &rep.files_written {
            o.push(format!("### `{}` ({}, {}/{} runs)", f.path, f.area.as_deref().unwrap_or("other"), f.runs, rep.runs));
            if let Some(p) = &f.preview {
                o.extend(["".into(), "```".into(), p.clone(), "```".into(), "".into()]);
            }
        }
    }

    if !rep.destructive.is_empty() {
        o.extend(["".into(), "## Destructive ops attempted".into(), "".into()]);
        o.extend(rep.destructive.iter().map(|d| format!("- run {}: `{}`", d.run, d.target)));
    }

    o.extend(["".into(), "## Trajectories".into(), "".into()]);
    for t in &rep.trajectories {
        o.push(format!("### Run {}", t.run));
        if t.sequence.is_empty() {
            o.push("(no tool calls)".into());
        }
        o.extend(t.sequence.iter().enumerate().map(|(i, s)| format!("{}. `{s}`", i + 1)));
        if let Some(f) = &t.final_text {
            o.extend(["".into(), "Final answer:".into(), "".into()]);
            o.push(format!("> {}", f.trim().replace('\n', "\n> ")));
            o.push(String::new());
        }
    }
    o.join("\n") + "\n"
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::classify::Call;
    use serde_json::json;

    fn call(cmd: &str) -> Call {
        Call { tool: "Bash".into(), input: json!({"command": cmd}), decision: "deny".into(), cwd: Some("/r".into()), ..Default::default() }
    }

    pub fn sample() -> Report {
        let run = |i, calls| RunResult {
            run: i,
            exit_code: 0,
            calls,
            unguarded: vec![],
            assistant_text: vec![],
            final_text: Some("done".into()),
            num_turns: None,
            cost_usd: Some(0.1),
            model: None,
            available_tools: Value::Null,
            skills: Value::Null,
        };
        let runs = vec![
            run(1, vec![call("az account show"), call("az group delete -n a")]),
            run(2, vec![call("az account show")]),
        ];
        let meta = Meta {
            prompt: "create a guard rail for deleting az".into(),
            cwd: "/r".into(),
            runs: 2,
            model: None,
            exec_mode: "readonly".into(),
            isolated: false,
            no_mcp: false,
            command: vec![],
        };
        build_report(&meta, &runs)
    }

    #[test]
    fn consensus_is_strict_majority() {
        let rep = sample();
        assert_eq!(rep.consensus, vec!["az account show"]);
        assert_eq!(rep.first_moves[0].name, "az account show");
        assert_eq!(rep.first_moves[0].count, 2);
        assert_eq!(rep.destructive.len(), 1);
        assert_eq!(rep.total_cost_usd, 0.2);
        assert!(render_markdown(&rep).contains("az group delete"));
    }
}
