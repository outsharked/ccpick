# ccs-finder — Claude Code session picker for ccs

Date: 2026-09-16
Status: Draft, pending review

## Problem

Claude Code sessions are launched through [ccs](https://github.com/kaitranntt/ccs), which gives each
account (`c1`, `c2`, `c3`) its own `CLAUDE_CONFIG_DIR` under `~/.ccs/instances/<account>/`. Their
`projects/` directories are symlinks into a shared context group
(`~/.ccs/shared/context-groups/default/projects`). Existing session finders hardcode
`~/.claude/projects`, resume with bare `claude`, or (agent-deck) want to own session lifecycle.

Goal: a small, fast TUI that finds any past or running session across all ccs accounts and resumes it
through `ccs`, without becoming the entry point for Claude.

Non-goals: managing/starting new sessions, tmux integration, editing or deleting transcripts,
remote machines, persistent full-text index.

## Stack

Rust, single binary. Crates: `ratatui` + `crossterm` (TUI), `nucleo-matcher` (fuzzy),
`serde`/`serde_json`/`serde_yaml` (parsing), `memchr` (substring search), `rayon` (parallel scan),
`clap` (CLI), `dirs` (paths), `nix` (exec + pid liveness). Repo: `~/code/ccs-finder`.

## Data sources

Observed with Claude Code 2.1.x:

- **Transcripts:** `<config_dir>/projects/<encoded-cwd>/<session-id>.jsonl`. Relevant line types:
  - `user` / `assistant`: have `cwd`, `gitBranch`, `timestamp`, `sessionId`, `message.content`
    (string, or array of blocks: `text`, `tool_use`, `tool_result`, ...). `isMeta: true` lines are
    not real prompts.
  - `ai-title` (`aiTitle`), `last-prompt` (`lastPrompt`); `custom-title` / `summary` if present.
  - Subdirectories under a project dir (subagent/tool-result storage) are ignored.
- **Live sessions:** `<config_dir>/sessions/<pid>.json` →
  `{pid, sessionId, cwd, startedAt, kind}`. Stale files remain after exit; liveness is decided by
  checking the pid (`kill(pid, 0)`) **and** that `/proc/<pid>/cmdline` looks like claude (guards
  against pid reuse).
- **ccs config:** `~/.ccs/config.yaml` → `default:` account and `accounts:` map. Account dir is
  `~/.ccs/instances/<name>`.

## Architecture

One crate, modules with single responsibilities:

| Module | Responsibility | Depends on |
|---|---|---|
| `discover` | Read ccs config; list accounts; resolve each account's `projects/` via canonicalize and dedupe; add `~/.claude/projects` if it exists and is distinct; read `sessions/*.json` + liveness → `LiveSession {pid, session_id, account}`; map session id → last account seen (most recent `startedAt`). | fs |
| `scan` | For each `*.jsonl` (depth 2 only), produce `SessionMeta {id, path, title, title_source, cwd, branch, first_ts, last_ts, first_prompt, msg_count, size, mtime}`. Streams lines, parses only needed fields. Uses cache. | `cache` |
| `cache` | `~/.cache/ccs-finder/meta.json`: `path → (size, mtime, SessionMeta)`. Load, reuse unchanged, rescan changed/new, drop missing, write atomically (tmp + rename). Versioned; version mismatch or parse error → discard and rebuild. | fs |
| `search` | Tier 1: nucleo fuzzy over `title + cwd + branch + first_prompt`, synchronous per keystroke. Tier 2: full-text substring (case-insensitive, `memchr::memmem` on lowercased text of user/assistant `text` blocks) on a worker thread with ~150 ms debounce; generation counter cancels stale runs; results arrive via channel with a snippet + matching message index. | `scan` output |
| `transcript` | Load a session's displayable messages (user/assistant text only, skip tool blocks and `isMeta`) for the preview; lazily, for the selected row only. | fs |
| `ui` | App state, rendering, key handling. | all above |
| `launch` | Restore terminal, `chdir(cwd)`, `execvp("ccs", [account, "--resume", id])`. | `nix` |

Data flow: `discover` → `scan` (parallel, cached) → in-memory `Vec<SessionMeta>` + `LiveSession`
map → `ui` loop, with `search` worker thread → on Enter, `launch`.

## UI

```
┌ ccs-finder ─ 292 sessions ─ 2 running ────────────────────────┐
│ > docker▏                                                      │
├───────────────────────────────────┬────────────────────────────┤
│ ● Migrate compose stack to v2     │ ~/code/infra · main · c1   │
│   ~/code/infra · c1 · 2d [running]│ 142 msgs · 2d ago          │
│   Fix flaky docker build cache    │ ─────────────────────────  │
│   ~/code/api · 1mo                │ You: Let's move the stack  │
│ ── in conversation text ───────── │ to compose v2              │
│   Reverse proxy TLS renewal       │ Claude: …                  │
│   …"restart the «docker» service…"│                            │
└───────────────────────────────────┴────────────────────────────┘
 ↵ resume  ^A account  ^R running only  ^S sort  tab preview  esc quit
```

- **List:** two sections — fuzzy/metadata matches (ranked by score, ties by recency), then
  full-text-only matches under a divider with a highlighted snippet. Empty query: all sessions,
  sorted by last activity. Row: title, `~`-shortened cwd, relative time, account, running badge.
- **Title:** `custom-title` → `ai-title` → `summary` → first real prompt (truncated) → `(untitled)`.
- **Preview:** header (cwd, branch, account, msg count, first/last time), then messages. Default
  scroll: last messages. For a full-text hit: scrolled to the matching message, term highlighted.
- **Keys:** `Enter` resume · `Ctrl-A` cycle account for the selected session (default: account that
  last ran it, else ccs `default`) · `Ctrl-R` running-only filter · `Ctrl-S` sort last-activity /
  created · `↑↓ PgUp PgDn` move · `Tab` focus preview (scroll with arrows) · `Esc`/`Ctrl-C` quit.
- **Running session + Enter:** does not launch; status line shows
  `running in c1 (pid 12345)`.
- **Missing cwd** (project dir deleted): row dimmed; Enter is blocked with status line
  `project dir no longer exists: <path>` (resuming from a different cwd would change Claude's
  project context).

CLI:
- `ccs-finder [QUERY]` — open TUI with query prefilled.
- `ccs-finder --list [QUERY]` — no TUI; print TSV `last_ts  account  running  id  cwd  title`,
  filtered by the same two-tier search.

## Error handling

- `~/.ccs/config.yaml` missing/unparseable → fall back to `~/.claude` only (and
  `CLAUDE_CONFIG_DIR` if set); status line notes it.
- Unreadable/corrupt JSONL line → skip the line; file with zero parseable lines → skip the file.
  Partially-written last line (active session) is normal and silently ignored.
- Cache failures never fatal: fall back to full scan.
- `ccs` not on PATH at launch → do not exit TUI; show error in status line.
- Terminal is always restored (panic hook + drop guard) before exec or exit.

## Performance targets

Reference dataset: ~300 transcripts, ~300 MB:
- Warm start (cache hit) to first frame: < 100 ms.
- Cold start (no cache): < 1 s.
- Full-text query results: < 500 ms (page cache warm).

Full-text scan reads files in parallel with rayon; only the text of user/assistant `text` blocks is
matched (not tool payloads), so results reflect the conversation, not file dumps.

## Testing

- **Unit (fixture-driven):** `tests/fixtures/` holds small synthetic JSONL files covering: string vs
  block content, `isMeta`, tool_result-only user lines, each title source, truncated final line,
  missing `cwd`. Tests for `scan` (metadata extraction, title precedence), `cache` (reuse on
  unchanged size+mtime, rescan on change, drop on delete, version mismatch), `search` (fuzzy
  ranking, full-text case-insensitivity, snippet extraction, stale-generation cancellation),
  `discover` (symlinked `projects/` dedupe via temp dirs, stale vs live `sessions/*.json`, account
  attribution by latest `startedAt`).
- **UI:** state-transition tests on `App` (key → state) without a terminal; one render snapshot
  test with ratatui's `TestBackend`.
- **Launch:** argv/cwd construction is a pure function and tested; the exec itself is not.
- **Manual acceptance:** against real data — a full-text query for a term known to appear only mid-conversation finds that session; a running session
  is badged and blocked; Enter on a past session resumes it via `ccs` in the right cwd.
