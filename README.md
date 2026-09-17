# ccpick

Fast TUI to find and resume Claude Code sessions across every config directory you use —
multiple [ccs](https://github.com/kaitranntt/ccs) accounts, `~/.claude`, `$CLAUDE_CONFIG_DIR`,
or any directory you configure. It doesn't manage sessions; it finds one and hands off to the
right launcher.

## Install

Linux (x86_64, arm64) and macOS (Intel, Apple Silicon):

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/outsharked/ccpick/releases/latest/download/ccpick-installer.sh | sh
```

Or build from source with a Rust toolchain:

```bash
cargo install --git https://github.com/outsharked/ccpick
```

Native Windows isn't supported; use it inside WSL. Running-session detection needs Linux
(`/proc`), so on macOS sessions are listed and resumable but never shown as running.

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
| `→` / `←` | Focus the conversation preview / the session list (`Tab` toggles) |
| `↑` / `↓` | Move selection (list) or scroll one line (preview) |
| `PgUp` / `PgDn` | Move 10 sessions (list) or scroll a page (preview) |
| `Home` / `End` | First/last session (list) or top/bottom of the conversation (preview) |
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

## Development

Tasks are managed with [mise](https://mise.jdx.dev) (`mise tasks` lists them):

```bash
mise dev                      # run the TUI from source
mise dev -- --list docker     # args after -- pass through to ccpick
mise check                    # fmt check, clippy (warnings as errors), tests
mise test                     # tests only
mise format                   # format
mise install-bin              # install the release binary to ~/.cargo/bin
```

Run the TUI from a plain shell rather than inside a Claude Code session, since resuming
a session replaces the ccpick process.

### Releasing

```bash
mise release 0.2.0         # bump version, run checks, commit, tag v0.2.0
git push --follow-tags     # CI builds binaries and publishes the GitHub release + installer
```

## Design

See `docs/superpowers/specs/2026-09-16-ccpick-design.md`. Agent-specific code lives behind a
`Provider` trait in `src/providers/`, so other agents (e.g. Codex CLI) can be added later.

## License

MIT — see [LICENSE](LICENSE).
