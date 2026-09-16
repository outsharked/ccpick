# ccpick

Fast TUI to find and resume Claude Code sessions across every config directory you use —
multiple [ccs](https://github.com/kaitranntt/ccs) accounts, `~/.claude`, `$CLAUDE_CONFIG_DIR`,
or any directory you configure. It doesn't manage sessions; it finds one and hands off to the
right launcher.

## Install

```bash
cargo install --path .
```

## Use

```bash
ccpick                 # browse everything
ccpick docker cache    # start with a query
ccpick --list migration  # non-interactive TSV
ccpick --sources       # show discovered sources and stores
ccpick --config-dir ~/.claude-work
```

Type to filter titles, project paths, branches and first prompts instantly; matches inside
conversation text appear below a divider shortly after.

| Key | Action |
|---|---|
| `Enter` | Resume (blocked if already running or project dir is gone) |
| `Ctrl-A` | Cycle which source/account resumes the session |
| `Ctrl-R` | Running sessions only |
| `Ctrl-S` | Sort by last activity / created |
| `Tab` | Focus preview (arrows scroll messages) |
| `↑` / `↓` | Move selection (list) or scroll one message (preview) |
| `PgUp` / `PgDn` | Move/scroll 10 at a time |
| `Esc` / `Ctrl-C` | Quit |

## Sources

Discovered in order (earlier wins as the default launcher):

1. ccs accounts from `~/.ccs/config.yaml` (default account first) → `ccs <account> --resume <id>`
2. `$CLAUDE_CONFIG_DIR` → `CLAUDE_CONFIG_DIR=<dir> claude --resume <id>`
3. `~/.claude` → `claude --resume <id>` (with `CLAUDE_CONFIG_DIR` unset)
4. `[[source]]` entries in `~/.config/ccpick/config.toml`
5. `--config-dir` flags

Sources whose `projects/` resolve to the same directory share one transcript store, so each
session is listed once and can be resumed through any of them.

## Config

`~/.config/ccpick/config.toml` (optional):

```toml
[claude]
ccs = true    # auto-detect ccs accounts
home = true   # include ~/.claude and $CLAUDE_CONFIG_DIR

[[source]]
name = "work"
config_dir = "~/.claude-work"
# command = ["my-wrapper", "--profile", "work"]   # default: claude with CLAUDE_CONFIG_DIR
```

Metadata is cached in `~/.cache/ccpick/meta.json` (`--no-cache` to bypass).

## Design

See `docs/superpowers/specs/2026-09-16-ccpick-design.md`. Agent-specific code lives behind a
`Provider` trait in `src/providers/`, so other agents (e.g. Codex CLI) can be added later.
