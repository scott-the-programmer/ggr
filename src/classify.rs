//! Turn raw tool calls into IO operations.
//!
//! Each tool call becomes one or more ops. A Bash call like
//! `az group list && az group delete -n rg1 --yes` becomes two ops, one per
//! command segment.
//!
//! `kind` is one of: fs.read, fs.list, fs.search, fs.write, fs.edit,
//! exec.read, exec.write, exec.delete, exec.script, exec.unknown,
//! net.fetch, net.search, agent, mcp, other.
//!
//! `area` (file ops only) is one of: claude-settings, claude-hook,
//! claude-skill, claude-md, script, other.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Call {
    #[serde(default)]
    pub ts: f64,
    #[serde(default)]
    pub tool: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub decision: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Op {
    pub tool: String,
    pub decision: String,
    pub kind: String,
    pub signature: String,
    pub target: String,
    pub area: Option<String>,
    pub destructive: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BashOp {
    pub kind: &'static str,
    pub signature: String,
    pub target: String,
    pub area: Option<&'static str>,
    pub destructive: bool,
}

const OPERATORS: &[&str] = &["&&", "||", "|", ";", ";;", "&", "|&", "\n"];
const WRAPPERS: &[&str] = &["sudo", "env", "time", "nohup", "command", "exec", "xargs", "watch"];
const REDIRECTS: &[&str] = &[">", ">>", "&>", "&>>", ">&", "<"];
const SHELL_PREFIX_KEYWORDS: &[&str] = &["do", "then", "else", "if", "elif", "while", "until", "!", "{"];
// Segments that are shell plumbing, not IO: loop headers, block ends, cd.
const SHELL_SKIP: &[&str] = &[
    "for", "case", "function", "select", "done", "fi", "esac", "}", "cd", "pushd", "popd", "export",
    "local", "set", "unset", "shopt", "true", "false", ":",
];
const SCRIPT_RUNNERS: &[&str] = &["bash", "sh", "zsh", "source", "."];

/// CLIs whose subcommand tree matters, with how many positional words to keep.
fn subcommand_depth(binary: &str) -> usize {
    match binary {
        "az" | "gcloud" => 4,
        "aws" | "gh" | "claude" => 3,
        "kubectl" | "helm" | "terraform" | "docker" | "git" | "npm" | "pip" | "brew" | "cargo" => 2,
        "python" | "python3" | "node" => 1,
        _ => 0,
    }
}

const DELETE_VERBS: &[&str] = &[
    "delete", "rm", "remove", "purge", "destroy", "del", "drop", "uninstall", "prune", "rmdir", "unlink",
    "shred", "truncate", "deallocate", "wipe",
];
const WRITE_VERBS: &[&str] = &[
    "create", "update", "set", "add", "apply", "deploy", "mv", "cp", "write", "put", "upload", "start", "stop",
    "restart", "assign", "install", "tee", "chmod", "chown", "mkdir", "touch", "init", "import", "commit", "push",
    "lock", "unlock", "swap", "scale", "patch", "tag", "move", "rename",
];
const READ_VERBS: &[&str] = &[
    "show", "list", "get", "ls", "cat", "head", "tail", "grep", "rg", "find", "describe", "which", "version",
    "account", "status", "log", "diff", "echo", "pwd", "whoami", "env", "printenv", "file", "stat", "wc", "jq",
    "--help", "-h", "help", "query", "exists", "tree", "view",
];

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|x| !x.is_empty())
}

pub fn classify_call(call: &Call) -> Vec<Op> {
    let tool = call.tool.as_str();
    let inp = &call.input;
    let cwd = call.cwd.as_deref().unwrap_or("");
    let mk = |kind: &str, signature: String, target: String, area: Option<String>, destructive: bool| Op {
        tool: tool.to_string(),
        decision: if call.decision.is_empty() { "unknown".into() } else { call.decision.clone() },
        kind: kind.to_string(),
        signature,
        target,
        area,
        destructive,
    };

    match tool {
        "Bash" => classify_bash(s(inp, "command").unwrap_or(""))
            .into_iter()
            .map(|b| mk(b.kind, b.signature, b.target, b.area.map(String::from), b.destructive))
            .collect(),
        "Read" | "NotebookRead" | "Glob" | "LS" | "Grep" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            let kind = match tool {
                "Read" | "NotebookRead" => "fs.read",
                "Glob" | "LS" => "fs.list",
                "Grep" => "fs.search",
                "Write" => "fs.write",
                _ => "fs.edit",
            };
            let shown = if tool == "Glob" || tool == "Grep" {
                let pat = s(inp, "pattern").unwrap_or("");
                match s(inp, "path") {
                    Some(p) => format!("{pat} in {p}"),
                    None => pat.to_string(),
                }
            } else {
                let path = s(inp, "file_path").or_else(|| s(inp, "notebook_path")).or_else(|| s(inp, "path")).unwrap_or("");
                rel(path, cwd)
            };
            let writes = kind == "fs.write" || kind == "fs.edit";
            let area = (writes || kind == "fs.read").then(|| file_area(&shown).to_string());
            let sig = if writes { format!("{tool} {shown}") } else { tool.to_string() };
            vec![mk(kind, sig, shown, area, false)]
        }
        "WebFetch" | "WebSearch" => {
            let kind = if tool == "WebFetch" { "net.fetch" } else { "net.search" };
            let target = s(inp, "url").or_else(|| s(inp, "query")).unwrap_or("").to_string();
            vec![mk(kind, tool.to_string(), target, None, false)]
        }
        "Agent" | "Task" | "Skill" | "AskUserQuestion" => {
            let target = s(inp, "skill")
                .or_else(|| s(inp, "subagent_type"))
                .or_else(|| s(inp, "description"))
                .unwrap_or("")
                .to_string();
            let sig = format!("{tool} {target}").trim().to_string();
            vec![mk("agent", sig, target, None, false)]
        }
        t if t.starts_with("mcp__") => {
            let verb = t.rsplit("__").next().unwrap_or("");
            let destructive = verb.split('_').any(|w| DELETE_VERBS.contains(&w));
            vec![mk("mcp", t.to_string(), json_short(inp, 160), None, destructive)]
        }
        t => vec![mk("other", t.to_string(), json_short(inp, 160), None, false)],
    }
}

pub fn classify_bash(command: &str) -> Vec<BashOp> {
    let mut ops = Vec::new();
    for seg in split_segments(command) {
        let mut redirect: Option<String> = None;
        let mut words: Vec<String> = Vec::new();
        let mut i = 0;
        while i < seg.len() {
            let tok = &seg[i];
            if REDIRECTS.contains(&tok.as_str()) {
                if words.last().is_some_and(|w| is_digits(w)) {
                    words.pop(); // the "2" in "2>/dev/null"
                }
                let target = seg.get(i + 1).cloned().unwrap_or_default();
                if tok != "<" && !target.is_empty() && !is_digits(&target) && !target.starts_with('&') {
                    redirect = Some(target);
                }
                i += 2;
                continue;
            }
            words.push(tok.clone());
            i += 1;
        }
        // Drop leading VAR=value assignments, wrappers like sudo/env, and
        // shell keywords that prefix a real command ("do az ...", "if az ...").
        while let Some(first) = words.first() {
            let assignment = first.contains('=') && !first.starts_with('-');
            if assignment || WRAPPERS.contains(&first.as_str()) || SHELL_PREFIX_KEYWORDS.contains(&first.as_str()) {
                words.remove(0);
            } else {
                break;
            }
        }
        match words.first() {
            None => continue,
            Some(w) if SHELL_SKIP.contains(&w.as_str()) => continue,
            _ => {}
        }
        let sig = command_signature(&words);
        let kind = exec_kind(&words, &sig);
        ops.push(BashOp {
            kind,
            signature: sig,
            target: seg.join(" "),
            area: None,
            destructive: kind == "exec.delete",
        });
        if let Some(r) = redirect {
            if r != "/dev/null" {
                ops.push(BashOp {
                    kind: "fs.write",
                    signature: format!("redirect > {r}"),
                    area: Some(file_area(&r)),
                    target: r,
                    destructive: false,
                });
            }
        }
    }
    ops
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// Split a command line into segments of words. Operators (`&&`, `|`, `;`,
/// newlines) and parens end a segment, so `X=$(az account show)` yields the
/// inner `az account show` as its own segment.
pub fn split_segments(command: &str) -> Vec<Vec<String>> {
    let tokens = tokenize(command).unwrap_or_else(|| command.split_whitespace().map(String::from).collect());
    let mut segments = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    for tok in tokens {
        let is_sep = OPERATORS.contains(&tok.as_str()) || (!tok.is_empty() && tok.chars().all(|c| c == '(' || c == ')'));
        if is_sep {
            if !cur.is_empty() {
                segments.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(tok);
        }
    }
    if !cur.is_empty() {
        segments.push(cur);
    }
    segments
}

/// POSIX-ish shell tokenizer. Runs of `();<>|&` become one punctuation token,
/// newlines are their own token, quotes and backslashes are resolved, and `#`
/// at the start of a word begins a comment. Returns None on an unclosed quote.
pub fn tokenize(cmd: &str) -> Option<Vec<String>> {
    const PUNCT: &str = "();<>|&";
    fn flush(toks: &mut Vec<String>, cur: &mut String, in_word: &mut bool) {
        if *in_word {
            toks.push(std::mem::take(cur));
            *in_word = false;
        }
    }
    let chars: Vec<char> = cmd.chars().collect();
    let n = chars.len();
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut i = 0;
    while i < n {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\r' => {
                flush(&mut toks, &mut cur, &mut in_word);
                i += 1;
            }
            '\n' => {
                flush(&mut toks, &mut cur, &mut in_word);
                toks.push("\n".into());
                i += 1;
            }
            '#' if !in_word => {
                while i < n && chars[i] != '\n' {
                    i += 1;
                }
            }
            '\'' => {
                in_word = true;
                i += 1;
                loop {
                    if i >= n {
                        return None;
                    }
                    if chars[i] == '\'' {
                        i += 1;
                        break;
                    }
                    cur.push(chars[i]);
                    i += 1;
                }
            }
            '"' => {
                in_word = true;
                i += 1;
                loop {
                    if i >= n {
                        return None;
                    }
                    let d = chars[i];
                    if d == '"' {
                        i += 1;
                        break;
                    }
                    if d == '\\' && i + 1 < n && matches!(chars[i + 1], '"' | '\\' | '$' | '`' | '\n') {
                        cur.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    cur.push(d);
                    i += 1;
                }
            }
            '\\' => {
                in_word = true;
                if i + 1 < n && chars[i + 1] != '\n' {
                    cur.push(chars[i + 1]);
                }
                i += 2;
            }
            c if PUNCT.contains(c) => {
                flush(&mut toks, &mut cur, &mut in_word);
                let mut p = String::new();
                while i < n && PUNCT.contains(chars[i]) {
                    p.push(chars[i]);
                    i += 1;
                }
                toks.push(p);
            }
            c => {
                in_word = true;
                cur.push(c);
                i += 1;
            }
        }
    }
    flush(&mut toks, &mut cur, &mut in_word);
    Some(toks)
}

pub fn command_signature(words: &[String]) -> String {
    // A script run by path: ./deploy.sh, .claude/hooks/guard.sh
    if is_script_path(&words[0]) {
        return words[0].clone();
    }
    let binary = basename(&words[0]);
    if SCRIPT_RUNNERS.contains(&binary) {
        return match words[1..].iter().find(|w| !w.starts_with('-')) {
            Some(script) => format!("{binary} {script}"),
            None => binary.to_string(),
        };
    }
    let depth = subcommand_depth(binary);
    let mut parts = vec![binary.to_string()];
    for w in &words[1..] {
        if parts.len() > depth {
            break;
        }
        if w.starts_with('-') || w.contains(['/', '=', '.', '$']) {
            break;
        }
        parts.push(w.clone());
    }
    parts.join(" ")
}

fn is_script_path(w: &str) -> bool {
    w.starts_with("./") || (w.contains('/') && matches!(file_area(w), "script" | "claude-hook"))
}

fn exec_kind(words: &[String], sig: &str) -> &'static str {
    let sig_words: Vec<&str> = sig.split(' ').collect();
    if SCRIPT_RUNNERS.contains(&sig_words[0]) || is_script_path(&words[0]) {
        return "exec.script";
    }
    if sig_words.iter().any(|w| DELETE_VERBS.contains(w)) {
        return "exec.delete";
    }
    if sig_words[0] == "sed" && words.iter().any(|w| w.starts_with("-i")) {
        return "exec.write";
    }
    if sig_words.iter().any(|w| WRITE_VERBS.contains(w)) {
        return "exec.write";
    }
    if sig_words.iter().any(|w| READ_VERBS.contains(w)) || words.iter().any(|w| w == "--help") {
        return "exec.read";
    }
    "exec.unknown"
}

pub fn file_area(path: &str) -> &'static str {
    let p = path.replace('\\', "/");
    let name = basename(&p);
    if p.contains("/.claude/hooks/") || p.starts_with(".claude/hooks/") {
        "claude-hook"
    } else if p.contains("/.claude/skills/") || p.starts_with(".claude/skills/") || name == "SKILL.md" {
        "claude-skill"
    } else if p.contains(".claude/") && name.starts_with("settings") && name.ends_with(".json") {
        "claude-settings"
    } else if name == "CLAUDE.md" {
        "claude-md"
    } else if [".sh", ".py", ".ps1", ".js", ".ts", ".rs"].iter().any(|e| name.ends_with(e)) {
        "script"
    } else {
        "other"
    }
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

pub fn rel(path: &str, cwd: &str) -> String {
    let cwd = cwd.trim_end_matches('/');
    if !cwd.is_empty() {
        if let Some(rest) = path.strip_prefix(&format!("{cwd}/")) {
            return rest.to_string();
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = path.strip_prefix(&format!("{home}/")) {
            return format!("~/{rest}");
        }
    }
    path.to_string()
}

pub fn json_short(v: &Value, limit: usize) -> String {
    let s = v.to_string();
    if s.chars().count() <= limit {
        s
    } else {
        format!("{}...", s.chars().take(limit - 3).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sigs(cmd: &str) -> Vec<(String, &'static str)> {
        classify_bash(cmd).into_iter().map(|o| (o.signature, o.kind)).collect()
    }

    fn pair(s: &str, k: &'static str) -> (String, &'static str) {
        (s.to_string(), k)
    }

    #[test]
    fn az_chain() {
        assert_eq!(
            sigs("az group list -o table && az group delete -n rg1 --yes"),
            vec![pair("az group list", "exec.read"), pair("az group delete", "exec.delete")]
        );
    }

    #[test]
    fn deep_az_subcommand() {
        assert_eq!(sigs("az storage account delete --name x"), vec![pair("az storage account delete", "exec.delete")]);
    }

    #[test]
    fn pipe_and_env_prefix() {
        assert_eq!(
            sigs("AZURE_X=1 az resource list | jq '.[]'"),
            vec![pair("az resource list", "exec.read"), pair("jq", "exec.read")]
        );
    }

    #[test]
    fn command_substitution_is_its_own_segment() {
        assert_eq!(
            sigs("SUB=$(az account show --query name -o tsv)"),
            vec![pair("az account show", "exec.read")]
        );
    }

    #[test]
    fn redirect_becomes_write() {
        let ops = classify_bash("cat hook.json > .claude/settings.json");
        assert_eq!(ops[1].kind, "fs.write");
        assert_eq!(ops[1].area, Some("claude-settings"));
    }

    #[test]
    fn rm_is_destructive() {
        assert!(classify_bash("rm -rf build")[0].destructive);
    }

    #[test]
    fn shell_loop_keeps_real_commands() {
        let cmd = "export X=1\nfor c in a b; do\n  printf \"%s\" \"$c\" | jq . 2>/dev/null\ndone";
        assert_eq!(sigs(cmd), vec![pair("printf", "exec.unknown"), pair("jq", "exec.read")]);
    }

    #[test]
    fn fd_redirect_is_not_a_file() {
        assert_eq!(sigs("ls -la scripts 2>/dev/null"), vec![pair("ls", "exec.read")]);
        assert_eq!(sigs("az group list 2>&1"), vec![pair("az group list", "exec.read")]);
    }

    #[test]
    fn script_run() {
        assert_eq!(sigs("bash .claude/hooks/guard.sh < in.json"), vec![pair("bash .claude/hooks/guard.sh", "exec.script")]);
        assert_eq!(sigs("echo '{}' | .claude/hooks/guard.sh"), vec![pair("echo", "exec.read"), pair(".claude/hooks/guard.sh", "exec.script")]);
        assert_eq!(sigs("./deploy --dry-run"), vec![pair("./deploy", "exec.script")]);
        assert_eq!(sigs("/usr/bin/az group list"), vec![pair("az group list", "exec.read")]);
    }

    #[test]
    fn comments_and_multiline() {
        assert_eq!(sigs("# setup\nmkdir -p .claude/hooks\nchmod +x .claude/hooks/x.sh").len(), 2);
    }

    #[test]
    fn unclosed_quote_falls_back() {
        assert_eq!(sigs("echo 'oops"), vec![pair("echo", "exec.read")]);
    }

    #[test]
    fn write_hook_relative_path() {
        let call = Call {
            tool: "Write".into(),
            cwd: Some("/repo".into()),
            input: json!({"file_path": "/repo/.claude/hooks/block-az-delete.sh", "content": "#!/bin/sh"}),
            ..Default::default()
        };
        let op = &classify_call(&call)[0];
        assert_eq!(op.signature, "Write .claude/hooks/block-az-delete.sh");
        assert_eq!(op.area.as_deref(), Some("claude-hook"));
    }

    #[test]
    fn mcp_delete() {
        let call = Call { tool: "mcp__x__delete_contact".into(), input: json!({"id": 1}), ..Default::default() };
        assert!(classify_call(&call)[0].destructive);
    }
}
