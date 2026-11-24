//! ggr: probe which IO operations a model reaches for on a prompt, then turn
//! that data into a Claude Code skill.

mod classify;
mod hook;
mod probe;
mod report;
mod synth;

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};

const DEFAULT_RUNS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/runs");
const DEFAULT_SKILLS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/skills");

#[derive(Parser)]
#[command(
    name = "ggr",
    about = "Probe which IO operations a model reaches for on a prompt, then generate a skill from it",
    after_help = "Example:\n  ggr run \"create a guard rail for deleting az\" --cwd ~/code/infra --runs 5"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Probe, report and generate a skill
    Run {
        #[command(flatten)]
        probe: ProbeArgs,
        #[command(flatten)]
        gen: GenArgs,
    },
    /// Probe and report only
    Probe {
        #[command(flatten)]
        probe: ProbeArgs,
    },
    /// Rebuild report.md/report.json for a probe dir and print it
    Report { probe_dir: PathBuf },
    /// Generate a skill from a recorded probe dir
    Generate {
        probe_dir: PathBuf,
        #[command(flatten)]
        gen: GenArgs,
    },
    /// PreToolUse hook used internally by probe runs
    #[command(hide = true)]
    Hook,
}

#[derive(Clone, Copy, ValueEnum)]
enum ExecMode {
    /// Let Read/Glob/Grep run, block everything else
    Readonly,
    /// Block every tool call
    None,
}

#[derive(Args)]
struct ProbeArgs {
    /// Prompt to give the model
    prompt: String,
    /// Working directory the model runs in
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    /// Number of independent runs
    #[arg(long, default_value_t = 3)]
    runs: usize,
    /// Runs to execute at once
    #[arg(long, default_value_t = 3)]
    parallel: usize,
    /// Model alias or id passed to claude --model
    #[arg(long)]
    model: Option<String>,
    /// What the hook lets through
    #[arg(long = "exec", value_enum, default_value = "readonly")]
    exec_mode: ExecMode,
    /// Ignore user-level Claude settings, hooks and plugins
    #[arg(long)]
    isolated: bool,
    /// Don't load any MCP servers
    #[arg(long)]
    no_mcp: bool,
    /// Seconds per run
    #[arg(long, default_value_t = 600)]
    timeout: u64,
    /// Where probe data goes
    #[arg(long, default_value = DEFAULT_RUNS_DIR)]
    runs_dir: PathBuf,
}

#[derive(Args)]
struct GenArgs {
    /// Skill name (default: slug of the prompt)
    #[arg(long)]
    name: Option<String>,
    /// Where skills are written
    #[arg(long, default_value = DEFAULT_SKILLS_DIR)]
    skills_dir: PathBuf,
    /// Use the deterministic template instead of asking Claude
    #[arg(long)]
    no_llm: bool,
    /// Model used to write the skill
    #[arg(long)]
    synth_model: Option<String>,
}

fn expand(p: &Path) -> PathBuf {
    match (p.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => p.to_path_buf(),
    }
}

fn do_probe(a: &ProbeArgs) -> Result<PathBuf> {
    let cwd = expand(&a.cwd);
    if !cwd.is_dir() {
        bail!("--cwd {} is not a directory", cwd.display());
    }
    let cwd = cwd.canonicalize()?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let runs_dir = expand(&a.runs_dir);
    std::fs::create_dir_all(&runs_dir)?;
    let out_dir = runs_dir.canonicalize()?.join(format!("{stamp}-{}", synth::slugify(&a.prompt, 32)));
    let cfg = probe::ProbeConfig {
        prompt: a.prompt.clone(),
        cwd: cwd.clone(),
        out_dir: out_dir.clone(),
        runs: a.runs,
        parallel: a.parallel,
        model: a.model.clone(),
        exec_mode: match a.exec_mode {
            ExecMode::Readonly => "readonly",
            ExecMode::None => "none",
        }
        .into(),
        isolated: a.isolated,
        no_mcp: a.no_mcp,
        timeout: a.timeout,
    };
    eprintln!("probing {}x in {}", a.runs, cwd.display());
    probe::probe(&cfg)?;
    do_report(&out_dir)?;
    eprintln!("probe data: {}", out_dir.display());
    Ok(out_dir)
}

fn do_report(out_dir: &Path) -> Result<(report::Report, String)> {
    let (meta, runs) = probe::load_runs(out_dir)?;
    let rep = report::build_report(&meta, &runs);
    let md = report::render_markdown(&rep);
    std::fs::write(out_dir.join("report.json"), serde_json::to_string_pretty(&rep)?)?;
    std::fs::write(out_dir.join("report.md"), &md)?;
    Ok((rep, md))
}

fn do_generate(out_dir: &Path, g: &GenArgs) -> Result<PathBuf> {
    let (rep, md) = do_report(out_dir)?;
    let name = g.name.clone().unwrap_or_else(|| synth::slugify(&rep.prompt, 48));
    let mut skill = None;
    if !g.no_llm {
        eprintln!("writing skill with claude...");
        skill = synth::synth_llm(&md, &rep, &name, g.synth_model.as_deref(), out_dir);
        if skill.is_none() {
            eprintln!("claude synthesis failed, falling back to template");
        }
    }
    let skill = skill.unwrap_or_else(|| synth::synth_template(&rep, &name));
    let path = synth::write_skill(&expand(&g.skills_dir).join(&name), &skill, &md, &rep)?;
    eprintln!("skill: {}", path.display());
    Ok(path)
}

fn main() -> Result<()> {
    // The hook path must fail closed, so it skips clap and anyhow's exit code.
    if std::env::args().nth(1).as_deref() == Some("hook") {
        hook::run();
    }
    match Cli::parse().cmd {
        Cmd::Run { probe, gen } => {
            let dir = do_probe(&probe)?;
            do_generate(&dir, &gen)?;
        }
        Cmd::Probe { probe } => {
            do_probe(&probe)?;
        }
        Cmd::Report { probe_dir } => print!("{}", do_report(&probe_dir)?.1),
        Cmd::Generate { probe_dir, gen } => {
            do_generate(&probe_dir, &gen)?;
        }
        Cmd::Hook => hook::run(),
    }
    Ok(())
}
