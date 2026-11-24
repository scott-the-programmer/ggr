//! Generate a SKILL.md from a probe report.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

use crate::probe::run_with_timeout;
use crate::report::Report;

const STOPWORDS: &[&str] = &["a", "an", "the", "for", "to", "of", "create", "make", "please", "me", "my", "and", "on", "in", "with"];

const SYNTH_INSTRUCTIONS: &str = r#"You are writing a Claude Code skill (a SKILL.md file) from observed behaviour.

We gave a model this prompt, in this working directory, several times:

  prompt: {prompt}
  cwd:    {cwd}

A PreToolUse hook recorded every tool call it attempted. Read-only file tools
ran; everything else (shell, writes, MCP, network) was recorded and blocked.
The report below shows which IO operations it reached for, how often, in what
order, and the files it tried to write.

Write a SKILL.md that makes the next run of this prompt go straight to the
right result. Rules:

1. Start with YAML frontmatter: `name: {name}` and a `description` that says
   what the skill does and lists the phrasings that should trigger it (include
   the original prompt). Keep the description under 600 characters.
2. Base every instruction on the observed data. Keep the operations most runs
   agreed on (the consensus), drop the dead ends and repeated retries, and
   name the concrete files, commands and paths it used.
3. If the report shows destructive operations (delete, purge, rm, ...), add a
   "Guard rails" section with explicit checks before each one.
4. If the runs wrote files (hooks, settings, scripts), include the best
   version inline, cleaned up, so the next run can write it in one step.
5. End with a short "Evidence" section: run count, the consensus ops, and the
   working directory the data came from.
6. Plain, direct prose. No marketing words, no emojis.

Output only the SKILL.md content, starting with `---`. No code fence around it.

=== REPORT ===
{report}
"#;

pub fn slugify(prompt: &str, limit: usize) -> String {
    let lower = prompt.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let kept: Vec<&str> = words.iter().copied().filter(|w| !STOPWORDS.contains(w)).collect();
    let chosen = if kept.is_empty() { words } else { kept };
    let slug: String = chosen.join("-").chars().take(limit).collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() { "probe-skill".into() } else { slug }
}

pub fn synth_llm(report_md: &str, rep: &Report, name: &str, model: Option<&str>, workdir: &Path) -> Option<String> {
    let prompt = SYNTH_INSTRUCTIONS
        .replace("{prompt}", &rep.prompt)
        .replace("{cwd}", &rep.cwd)
        .replace("{name}", name)
        .replace("{report}", report_md);
    let mut cmd = Command::new("claude");
    cmd.args(["-p", &prompt, "--output-format", "json", "--tools", "", "--permission-mode", "dontAsk"])
        .args(["--no-session-persistence", "--setting-sources", "project,local", "--strict-mcp-config"])
        .current_dir(workdir)
        .env_remove("CLAUDECODE");
    if let Some(m) = model {
        cmd.args(["--model", m]);
    }
    let out = run_with_timeout(cmd, Duration::from_secs(600)).ok()?;
    let parsed: Value = serde_json::from_str(&out.stdout).ok()?;
    let text = clean_skill_text(parsed.get("result")?.as_str()?)?;
    Some(force_name(&text, name))
}

/// Make the frontmatter `name:` match the skill directory. Claude Code
/// expects them to agree, and the model sometimes picks its own name.
pub fn force_name(skill: &str, name: &str) -> String {
    let mut in_front = false;
    let mut done = false;
    let mut out: Vec<String> = Vec::new();
    for (i, line) in skill.lines().enumerate() {
        if i == 0 && line == "---" {
            in_front = true;
        } else if in_front && line == "---" {
            if !done {
                out.push(format!("name: {name}"));
                done = true;
            }
            in_front = false;
        } else if in_front && line.starts_with("name:") && !done {
            out.push(format!("name: {name}"));
            done = true;
            continue;
        }
        out.push(line.to_string());
    }
    out.join("\n") + "\n"
}

/// Strip a wrapping code fence and any preamble before the frontmatter.
pub fn clean_skill_text(raw: &str) -> Option<String> {
    let mut text = raw.trim().to_string();
    if text.starts_with("```") && text.ends_with("```") && text.len() > 6 {
        let inner = &text[3..text.len() - 3];
        let inner = inner.split_once('\n').map(|(_, rest)| rest).unwrap_or("");
        text = inner.trim().to_string();
    }
    if !text.starts_with("---") {
        let idx = text.find("\n---")?;
        text = text[idx + 1..].to_string();
    }
    Some(text + "\n")
}

pub fn synth_template(rep: &Report, name: &str) -> String {
    let n = rep.runs;
    let is_read = |k: &str| matches!(k, "fs.read" | "fs.list" | "fs.search");
    let top: Vec<_> = rep.signatures.iter().filter(|s| !is_read(&s.kind)).take(3).collect();
    let triggers: Vec<String> = std::iter::once(rep.prompt.clone())
        .chain(top.iter().map(|s| s.signature.clone()))
        .map(|t| format!("\"{t}\""))
        .collect();
    let description = format!(
        "Handles requests like \"{}\". Built from {n} recorded probe runs. Use when the user says {}.",
        rep.prompt,
        triggers.join(", ")
    );
    let title = {
        let t = name.replace('-', " ");
        let mut c = t.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    };
    let mut l: Vec<String> = vec![
        "---".into(),
        format!("name: {name}"),
        format!("description: {}", serde_json::to_string(&description).unwrap()),
        "---".into(),
        String::new(),
        format!("# {title}"),
        String::new(),
        format!("Generated by ggr from {n} probe runs of `{}` in `{}`.", rep.prompt, rep.cwd),
        String::new(),
        "## Steps".into(),
        String::new(),
    ];
    let consensus: Vec<_> = rep.signatures.iter().filter(|s| rep.consensus.contains(&s.signature)).collect();
    if consensus.is_empty() {
        l.push("No operation appeared in a majority of runs. See the evidence table.".into());
    }
    for (i, s) in consensus.iter().enumerate() {
        let ex = s.examples.first().unwrap_or(&s.signature);
        l.push(format!("{}. `{}` ({}, {}/{n} runs). Example: `{ex}`", i + 1, s.signature, s.kind, s.runs));
    }

    if !rep.destructive.is_empty() {
        l.extend(["".into(), "## Guard rails".into(), "".into()]);
        let mut seen = Vec::new();
        for d in &rep.destructive {
            if seen.contains(&d.signature) {
                continue;
            }
            seen.push(d.signature.clone());
            l.push(format!(
                "- Before `{}`: list the target first, confirm the subscription or path, show the user exactly \
                 what will be removed, and get explicit confirmation. Never pass `--yes`/`-y`/`--force` without it.",
                d.signature
            ));
        }
    }

    if !rep.files_written.is_empty() {
        l.extend(["".into(), "## Files to write".into(), "".into()]);
        for f in &rep.files_written {
            l.push(format!("### `{}` ({}, {}/{n} runs)", f.path, f.area.as_deref().unwrap_or("other"), f.runs));
            if let Some(p) = &f.preview {
                l.extend(["".into(), "```".into(), p.clone(), "```".into(), "".into()]);
            }
        }
    }

    l.extend(["".into(), "## Evidence".into(), "".into(), "| signature | kind | runs |".into(), "|---|---|---|".into()]);
    l.extend(rep.signatures.iter().map(|s| format!("| `{}` | {} | {}/{n} |", s.signature, s.kind, s.runs)));
    l.join("\n") + "\n"
}

pub fn write_skill(skill_dir: &Path, skill_md: &str, report_md: &str, rep: &Report) -> Result<PathBuf> {
    let refs = skill_dir.join("references");
    std::fs::create_dir_all(&refs)?;
    let path = skill_dir.join("SKILL.md");
    std::fs::write(&path, skill_md)?;
    std::fs::write(refs.join("probe-report.md"), report_md)?;
    let mut slim = serde_json::to_value(rep)?;
    if let Some(obj) = slim.as_object_mut() {
        obj.remove("trajectories");
    }
    std::fs::write(refs.join("probe-report.json"), serde_json::to_string_pretty(&slim)?)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::tests::sample;

    #[test]
    fn slug() {
        assert_eq!(slugify("create a guard rail for deleting az", 48), "guard-rail-deleting-az");
        assert_eq!(slugify("the a", 48), "the-a");
        assert_eq!(slugify("!!!", 48), "probe-skill");
    }

    #[test]
    fn template_has_guard_rails() {
        let rep = sample();
        let skill = synth_template(&rep, &slugify(&rep.prompt, 48));
        assert!(skill.starts_with("---\nname: guard-rail-deleting-az\n"));
        assert!(skill.contains("## Guard rails"));
        assert!(skill.contains("1. `az account show`"));
    }

    #[test]
    fn cleans_fenced_and_preamble_output() {
        assert_eq!(clean_skill_text("```markdown\n---\nname: x\n---\nbody\n```").unwrap(), "---\nname: x\n---\nbody\n");
        assert_eq!(clean_skill_text("Here you go:\n---\nname: x\n---").unwrap(), "---\nname: x\n---\n");
        assert!(clean_skill_text("no frontmatter").is_none());
    }

    #[test]
    fn forces_frontmatter_name() {
        assert_eq!(force_name("---\nname: other\ndescription: d\n---\n# T\nname: keep\n", "mine"), "---\nname: mine\ndescription: d\n---\n# T\nname: keep\n");
        assert_eq!(force_name("---\ndescription: d\n---\nbody\n", "mine"), "---\ndescription: d\nname: mine\n---\nbody\n");
    }
}
