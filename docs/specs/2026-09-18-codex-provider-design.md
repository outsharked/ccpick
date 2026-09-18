# Codex CLI as a second provider

Date: 2026-09-18

ccpick lists and resumes Claude Code sessions. This adds OpenAI's Codex CLI alongside it,
through the `Provider` trait that was built for exactly this, so a Codex session appears in
the same list, the same search, the same preview and the same web portal as a Claude one.

## Why it fits without new abstractions

The `Provider` trait needs no changes. Codex differs from Claude in where its data lives and
how it records liveness, not in what a session *is*. The one piece of genuinely shared
machinery this adds — "which processes hold this file open" — belongs in `src/process.rs`
with the rest of the liveness code, not in a provider.

## What Codex stores

Verified against a real `~/.codex` (95 threads, Codex CLI 0.155.0), not from documentation.

- **`~/.codex/`** is the config root. There are no profiles or accounts — a single home, plus
  whatever `CODEX_HOME` points at. None of the ccs multi-account machinery applies.
- **`~/.codex/sessions/YYYY/MM/DD/rollout-<ISO-timestamp>-<uuid>.jsonl`** holds the
  transcripts, nested by date rather than flat. The filename carries the session UUID.
- **`~/.codex/state_<n>.sqlite`** is a complete index of those files: one `threads` row per
  rollout, 95 of each in the sample. Columns used here: `id`, `rollout_path`, `cwd`, `title`,
  `name`, `first_user_message`, `git_branch`, `created_at_ms`, `recency_at_ms`, `source`.
- **Rollout records** come in several `type`s. `response_item / message` is the raw model
  conversation and includes injected context — the environment block, AGENTS.md — so it reads
  as noise. `event_msg / user_message` and `event_msg / agent_message` are what the human
  actually typed and saw. Those are the conversation.

### Reading the index rather than the files

The catalog comes from SQLite. One query yields every field the list needs — title, cwd,
branch, timestamps — with no file parsing, so a Codex source lists faster than a Claude one,
which has to open every transcript.

This costs a `rusqlite` dependency and couples ccpick to a schema that is versioned in its own
filename: `state_5.sqlite` implies four earlier shapes. That is accepted deliberately. The
database is opened **read-only**, and Codex holds it open in WAL mode while running, which is
safe for concurrent readers. The store is discovered by globbing `state_*.sqlite` and taking
the highest number, so a `state_6` is picked up without a code change; a schema that moves
underneath us is a maintenance event, not a design flaw.

The rollout files are still read for the preview pane and for full-text search, exactly as the
Claude provider reads its transcripts.

## What is listed

`source` distinguishes how a thread began. In the sample, 62 of 95 threads are subagents
spawned by a parent — not sessions anyone resumes. Only `cli` and `vscode` threads are listed,
which is 33 of 95. Subagent transcripts stay on disk and stay Codex's business; ccpick simply
does not offer them as sessions.

Threads with neither a `name` nor a `title` and no messages are dropped as stillborn.

## Discovery

A Codex source is `~/.codex`, or `$CODEX_HOME` when set. `.codex` joins the provider's home
markers, so the existing cross-environment scan finds a Codex home in a WSL distro from Windows
and under `/mnt/c/Users/<user>` from WSL, exactly as it already does for `.claude`. The source
is named `codex`, and `<label>:codex` where a home needs distinguishing, following the naming
the Claude provider already uses for `win:` and `wsl:` homes.

`[codex] home = true` in `~/.config/ccpick/config.toml` toggles auto-discovery, mirroring
`[claude]`. The config schema's `agent` field is per-provider, not shared infrastructure: each
provider's `discover_sources` only picks up `[[source]]` entries whose `agent` matches its own
id, the same way `src/providers/claude/sources.rs` already does for `agent = "claude"`. The
Codex provider mirrors that filter for `agent = "codex"`, so a configured entry naming Codex is
honoured; before this, such an entry was silently dropped, and once a Codex provider is
registered it does not even earn the "unknown agent" warning, since some provider now claims
that id. `--config-dir` (`settings.cli_config_dirs`) stays claimed by the Claude provider only —
it carries no `agent`, and letting Codex claim it too would mint two sources for one directory.
Naming a Codex directory outside the home requires an explicit `[[source]]` entry.

## Session fields

| ccpick | Codex |
|---|---|
| `id` | `threads.id` (UUID) |
| `title` | `name` if set, else `title`, else the first user message — the same precedence the Claude provider uses for custom title, generated title, then summary |
| `cwd` | `threads.cwd` |
| `branch` | `threads.git_branch` |
| `first_ts` / `last_ts` | `created_at_ms` / `recency_at_ms` |
| `first_prompt` | `threads.first_user_message` |
| `path` | `threads.rollout_path` — what the preview and search read |

## Liveness

A running Codex process holds its rollout file open, and that filename carries the session
UUID. So a live session is discoverable without any registry: find processes whose command
line is `codex`, read their open file descriptors, and map the rollout paths back to ids.

`ProcessProbe` gains an agent-neutral capability for this — given a process name and a
predicate over open files, return the matching (pid, path) pairs — because AGENTS.md puts
process liveness in `src/process.rs` and because a future Windows implementation should land
in one place rather than inside a provider.

**Platform reach:** Linux and WSL, via `/proc/<pid>/fd`. Windows-native would need handle
enumeration through `NtQuerySystemInformation`, which has no equivalent in this codebase and
cannot be tested from WSL, so Codex sessions on a Windows host list and resume but never show
as live — the position macOS is already in for Claude. Probing a WSL distro from a Windows
host reuses the `wsl.exe --exec` path already built for Claude.

## Focus and resume

Focusing needs no new work: it takes a pid and a tab title, and Codex sets its terminal title
from the thread's `name`, which is the string to match.

Resuming is `codex resume <uuid>`, run in the thread's `cwd`. Cross-environment sessions fall
back to the paste-ready command, as they already do.

## What this forces elsewhere

The web portal looks sessions up by bare `meta.id`, while the catalog's own maps key on
`(provider, id)`. A second provider makes that reachable: two sessions could share an id and
a click could act on the wrong one. `Catalog::find_by_id` already carries a comment saying so.
It is fixed as part of this work, not after it.

## Out of scope

Windows-native liveness. Listing or navigating subagent threads. Anything that writes to
Codex's data — the database is opened read-only and the rollout files are never modified.

## Testing

- The SQLite reader is tested against a fixture database built in the test, not against the
  developer's own `~/.codex`.
- Rollout parsing is tested against small synthetic `.jsonl` fixtures in
  `tests/fixtures/codex/`, mirroring how the Claude transcript tests work.
- The liveness capability is tested with an injected fd-reader, so no test enumerates real
  processes.
- No test may read the developer's real Codex data, spawn `codex`, or focus a window.
