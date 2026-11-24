# ggr

Give a model a prompt in a real working directory, record every IO operation it reaches for, and turn that data into a Claude Code skill.

```sh
cargo install --path .
ggr run "create a guard rail for deleting az" --cwd ~/code/infra --runs 5
```

That runs `claude -p` five times inside `~/code/infra`, writes the probe data to `runs/<timestamp>-guard-rail-deleting-az/`, and writes the skill to `skills/guard-rail-deleting-az/SKILL.md`.

## How it works

1. **Probe.** Each run starts headless Claude Code in `--cwd` with a `PreToolUse` hook on every tool, MCP and subagent tools included. The hook is the same binary (`ggr hook`), referenced by absolute path. It appends the call to `calls.jsonl` and then decides:
   - `--exec readonly` (default): `Read`, `Glob` and `Grep` run, so the model can look at the directory. Everything else is blocked.
   - `--exec none`: everything is blocked.

   A blocked call never executes. The deny message tells the model the call was recorded, so it carries on and shows the next thing it would do. The hook fails closed: any error or panic exits 2, which blocks the call. As a second layer the session runs in `dontAsk` permission mode, and ggr compares the transcript's `tool_use` blocks with the hook log. Any call missing from the log is reported as unguarded.

2. **Classify.** `src/classify.rs` turns each call into IO ops. Bash commands get split on `&&`, `|`, `;` and newlines, `$(...)` substitutions become their own command, and each command gets a signature such as `az group delete` or `bash .claude/hooks/guard.sh`, plus a kind: `exec.read`, `exec.write`, `exec.delete`, `exec.script`, `fs.write`, `fs.edit`, `mcp`, and so on. File writes are tagged by area (`claude-hook`, `claude-settings`, `claude-skill`, `script`).

3. **Report.** `report.md` and `report.json` show op counts, how many runs used each signature, the ops a majority of runs agreed on, each run's first move, the files it tried to write (with content), any destructive ops, and every run's full trajectory and final answer.

4. **Generate.** By default a second `claude -p` call with no tools reads the report and writes `SKILL.md`. It keeps the consensus ops, drops the dead ends, inlines the best version of any file the runs wrote, and adds guard rails for destructive ops. `--no-llm` uses a deterministic template instead. The frontmatter `name` is forced to match the skill directory. The report is copied into `skills/<name>/references/` as evidence.

## Commands

```sh
ggr run PROMPT --cwd PATH [options]    # probe, report, generate
ggr probe PROMPT --cwd PATH [options]  # probe and report only
ggr report runs/<id>                   # rebuild the report from recorded data
ggr generate runs/<id> [--no-llm]      # (re)generate the skill from recorded data
```

Probe options:

| flag | default | |
|---|---|---|
| `--cwd` | `.` | directory the model runs in |
| `--runs` | 3 | independent runs; more runs give a better consensus |
| `--parallel` | 3 | runs at once |
| `--model` | CLI default | passed to `claude --model` |
| `--exec` | `readonly` | `readonly` or `none` |
| `--isolated` | off | ignore user-level settings, hooks and plugins, to see the model without your personal setup |
| `--no-mcp` | off | load no MCP servers |
| `--timeout` | 600 | seconds per run |

Generate options: `--name`, `--skills-dir`, `--no-llm`, `--synth-model`.

## Comparing working directories

The working directory changes what the model does. In a repo with `scripts/teardown.sh` it hardens that script. In an empty directory it writes only a hook. Probe the same prompt in several directories and diff the reports:

```sh
ggr probe "create a guard rail for deleting az" --cwd ~/code/infra
ggr probe "create a guard rail for deleting az" --cwd ~/code/api
diff runs/*infra*/report.md runs/*api*/report.md
```

By default the probe runs with your user settings, so your global CLAUDE.md, plugins and hooks shape the result. Add `--isolated` to take them out.

Default `--runs-dir` and `--skills-dir` point at `runs/` and `skills/` in this repo, wherever the binary was built from.

## Layout

| file | |
|---|---|
| `src/main.rs` | CLI |
| `src/hook.rs` | recording PreToolUse hook (`ggr hook`) |
| `src/probe.rs` | runs `claude -p`, parses the stream-json transcript, detects unguarded calls |
| `src/classify.rs` | shell tokenizer and IO op classifier |
| `src/report.rs` | aggregation and markdown report |
| `src/synth.rs` | SKILL.md generation, LLM and template |
| `tests/hook.rs` | runs the built binary as a hook |

## Requirements

Rust (tested on 1.97) and the `claude` CLI on `PATH`, logged in. Each run costs about the same as one normal Claude Code turn. On Opus, the smoke test cost $0.52 for one run and $0.93 for two.

## Known gap

Models often explore with `cat`, `ls` and `head` through Bash instead of the Read tool. Readonly mode blocks those, and the model doesn't always fall back to Read. In one smoke-test run it said it couldn't read `scripts/` and planned without that context. An allowlist of read-only shell commands would close the gap, at some safety cost.

## Tests

```sh
cargo test
```
