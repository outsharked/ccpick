# ccpick — Claude Code session picker

Date: 2026-09-16
Status: Draft, pending review

## Problem

Claude Code stores session transcripts under its config directory (`~/.claude` by default, or
`$CLAUDE_CONFIG_DIR`). People who run more than one config directory — multiple accounts via
[ccs](https://github.com/kaitranntt/ccs) (`~/.ccs/instances/<account>/`), or hand-rolled setups like
`CLAUDE_CONFIG_DIR=~/.claude-work` — have sessions scattered across several directories. With ccs,
each account's `projects/` directory may also be a symlink into a shared context group.

Existing session finders hardcode `~/.claude/projects`, resume with bare `claude` (wrong account), or
want to own the session lifecycle (tmux-based managers).

Goal: a small, fast TUI that finds any past or running Claude Code session across every configured
config directory, and resumes it with the right launcher and account — without becoming the entry
point for Claude.

Non-goals: starting new sessions, tmux integration, editing or deleting transcripts, remote
machines, persistent full-text index.

Future (designed for, not built in v1): other coding agents with local session history, starting
with OpenAI Codex CLI. See **Agent providers**.

## Stack

Rust, single binary. Crates: `ratatui` + `crossterm` (TUI), `nucleo-matcher` (fuzzy),
`serde`/`serde_json`/`serde_yaml`/`toml` (parsing), `memchr` (substring search), `rayon` (parallel
scan), `clap` (CLI), `dirs` (paths), `chrono` (timestamps). Linux-first: liveness via `/proc`, launch via
`std::os::unix::process::CommandExt::exec`. Repo: `~/code/ccpick`,
`github.com/jamietre/ccpick`.

## Agent providers

Everything agent-specific lives behind one trait; the rest of the app (cache, search, UI, launch
execution) only sees agent-neutral types. v1 ships exactly one implementation, `ClaudeProvider`.

```rust
trait Provider: Send + Sync {
    /// Stable id, used in config (`agent = "claude"`), cache keys and the UI badge.
    fn id(&self) -> &'static str;
    /// Auto-detected sources for this agent (ccs accounts, env var, default home).
    fn discover_sources(&self, settings: &Settings) -> Vec<Source>;
    /// Transcript store for a source (dir that holds session files); None if absent.
    fn store_for(&self, source: &Source) -> Option<PathBuf>;
    /// Enumerate session files in a store.
    fn list_session_files(&self, store: &Path) -> Vec<PathBuf>;
    /// Parse metadata from one session file.
    fn scan_file(&self, path: &Path) -> Option<SessionMeta>;
    /// Displayable/searchable messages (conversation text only, no tool payloads).
    fn messages(&self, path: &Path) -> Vec<Message>;
    /// Cheap pre-filter for full-text search: false only if the file certainly can't contain
    /// `needle_lower` in its messages (e.g. raw-byte scan). Default: true.
    fn may_contain(&self, path: &Path, needle_lower: &str) -> bool { true }
    /// Launch records for a source (who started which session when, and whether it's still alive).
    fn launch_records(&self, source: &Source) -> Vec<LaunchRecord>;
    /// Command/env to resume a session through a source. Pure.
    fn launch_plan(&self, source: &Source, session: &SessionMeta) -> LaunchPlan;
}
```

Agent-neutral types: `Source`, `SessionMeta`, `Message {role: Role, text, ts}`,
`LaunchRecord {pid, session_id, started_at_ms, alive}`, `LaunchSpec {argv_prefix, env_set, env_remove}`, `LaunchPlan {cwd, argv, env_set, env_remove}`. Every
`SessionMeta` and `Source` carries its `agent` id; session identity is `(agent, id)`.

Provider-specific configuration (ccs detection, launcher kinds) is owned by the provider and read
from its own config table, so adding a provider never changes shared config structure.

**Codex (future), for scoping only — verify formats when implementing:** sources come from
`$CODEX_HOME` / `~/.codex`; transcripts are dated `rollout-*.jsonl` files under `sessions/`
(nested by date, not by project — so `list_session_files` must walk deeper, and `cwd` comes from
file contents, not the path); resume via `codex resume <id>`; live-session detection may not have
a pid registry, in which case `launch_records` returns empty and the running badge is simply absent
for Codex. The design accommodates each of these: file enumeration, cwd extraction and liveness
are all provider methods.

UI impact when a second provider exists: a small agent badge per row (`claude`/`codex`), and an
agent filter key. Not shown in v1 while there is only one provider.

## Sources (Claude provider)

A **source** is one agent config directory plus how to launch the agent against it:

```rust
struct Source {
    agent: &'static str,   // provider id, "claude" in v1
    name: String,          // shown in UI: "c1", "claude", "work"
    config_dir: PathBuf,   // canonicalized
    launch: LaunchSpec,    // agent-neutral: argv prefix + env changes
}
```

The Claude provider fills `LaunchSpec` per source kind; `launch_plan` appends `--resume <id>`:

| Source kind | `argv_prefix` | env |
|---|---|---|
| ccs account | `["ccs", "<account>"]` | — |
| `$CLAUDE_CONFIG_DIR` / configured dir / `--config-dir` | `["claude"]` | set `CLAUDE_CONFIG_DIR=<dir>` |
| `~/.claude` | `["claude"]` | remove `CLAUDE_CONFIG_DIR` |
| configured with `command` | the command argv | — |

Sources are assembled in this order (earlier = higher priority for default launch):

1. **ccs accounts** (if `~/.ccs/config.yaml` exists and `[claude] ccs = true`): ccs `default` account first,
   then the rest in config order. `config_dir = ~/.ccs/instances/<name>`.
2. **`$CLAUDE_CONFIG_DIR`**, if set, `[claude] home = true`, and not already a source. Name = directory basename.
3. **`~/.claude`** (if it exists and `[claude] home = true`). Name `claude`. Launched with
   `CLAUDE_CONFIG_DIR` **removed** from the environment so an inherited value doesn't redirect it.
4. **Configured sources** from `~/.config/ccpick/config.toml`.
5. **`--config-dir <DIR>`** CLI flags (repeatable). Name = basename.

Sources whose canonical `config_dir` duplicates an earlier one are dropped.

Config file (all optional; missing file = defaults):

```toml
[claude]
ccs = true       # auto-detect ccs accounts
home = true      # include ~/.claude and $CLAUDE_CONFIG_DIR

[[source]]
agent = "claude" # default; future: "codex"
name = "work"
config_dir = "~/.claude-work"
# command = ["my-claude-wrapper", "--profile", "work"]   # optional; default: claude with CLAUDE_CONFIG_DIR
```

### Shared transcript stores

Several sources of the same agent can resolve to the same **store** — the canonicalized `<config_dir>/projects`
directory (e.g. all ccs accounts in one context group share one). Transcripts are scanned once per
store. Each session records `store_sources: Vec<SourceId>`: every source that can see it, in source
priority order. A session can only be resumed through a source in that list (a source whose store
lacks the transcript can't resume it).

**Default launch source** for a session: the source whose `sessions/*.json` most recently recorded
it (by `startedAt`), else the first entry of `store_sources`.

## Claude data formats

Observed with Claude Code 2.1.x:

- **Transcripts:** `<store>/<encoded-cwd>/<session-id>.jsonl`. Relevant line types:
  - `user` / `assistant`: have `cwd`, `gitBranch`, `timestamp`, `sessionId`, `message.content`
    (string, or array of blocks: `text`, `tool_use`, `tool_result`, ...). `isMeta: true` lines are
    not real prompts.
  - `ai-title` (`aiTitle`, may appear several times — last wins), `last-prompt` (`lastPrompt`);
    `custom-title` (`customTitle`) / `summary` (`summary`) handled if present (not observed in 2.1.x data).
  - Subdirectories under a project dir (subagent/tool-result storage) are ignored.
- **Live sessions:** `<config_dir>/sessions/<pid>.json` →
  `{pid, sessionId, cwd, startedAt, kind}`. Stale files remain after exit; a session is live only if
  `/proc/<pid>/cmdline` is readable **and** contains `claude` (guards against pid reuse). Newer
  records also carry `procStart`; not used in v1.
- **ccs config:** `~/.ccs/config.yaml` → `default:` account name and `accounts:` map.

## Architecture

One crate, modules with single responsibilities:

| Module | Responsibility | Depends on |
|---|---|---|
| `model` | Agent-neutral types: `Source`, `LaunchSpec`, `SessionMeta`, `Message`, `LaunchRecord`, `LaunchPlan`; the `Provider` trait. | — |
| `config` | Load `~/.config/ccpick/config.toml` + CLI flags into `Settings` (shared keys + raw per-provider tables). | fs |
| `providers/mod` | Provider registry (`Vec<Box<dyn Provider>>`; v1: Claude only). | `model` |
| `providers/claude/sources` | Build ordered, deduped Claude sources (ccs config, env, `~/.claude`, config, flags). | `config` |
| `providers/claude/transcript` | Parse Claude JSONL: metadata for `scan_file`, messages for `messages`. | fs |
| `providers/claude/live` | `sessions/*.json` → `LaunchRecord`s with `/proc` liveness. | fs |
| `providers/claude/launch` | `LaunchSpec` per source kind; `launch_plan`. | `model` |
| `stores` | Ask each provider for sources' stores; group sources by `(agent, canonical store)`; compute `store_sources` and default launch source per session (using live data). | providers |
| `scan` | Enumerate files per store via provider, parallel `scan_file` with cache. | `cache`, providers |
| `cache` | `~/.cache/ccpick/meta.json`: `(agent, path) → (size, mtime, SessionMeta)`. Reuse unchanged, rescan changed/new, drop missing, write atomically (tmp + rename). Versioned; mismatch or parse error → rebuild. | fs |
| `search` | Tier 1: nucleo fuzzy over `title + cwd + branch + first_prompt`, synchronous per keystroke. Tier 2: case-insensitive substring (`memchr::memmem`) over `provider.messages()` text, on a worker thread with ~150 ms debounce; generation counter cancels stale runs; results via channel with snippet + message index. | `scan`, providers |
| `ui` | App state, rendering, key handling. Agent-neutral. | all above |
| `launch` | Execute a `LaunchPlan`: restore terminal, `chdir`, adjust env, `exec`. | std |

Data flow: `config` → providers discover sources → `stores` (+ live sessions) → `scan` per store in
parallel, cached → in-memory sessions → `ui` loop with `search` worker → on Enter,
`provider.launch_plan` → `launch`.

Rule for v1 code: nothing outside `providers/claude/` may reference Claude file formats, ccs, or
`CLAUDE_CONFIG_DIR`.

## UI

```
┌ ccpick ─ 292 sessions ─ 4 sources ─ 2 running ─────────────────┐
│ > docker▏                                                      │
├───────────────────────────────────┬────────────────────────────┤
│ ● Migrate compose stack to v2     │ ~/code/infra · main · c1   │
│   ~/code/infra · c1 · 2d [running]│ 142 msgs · 2d ago          │
│   Fix flaky docker build cache    │ ─────────────────────────  │
│   ~/code/api · claude · 1mo       │ You: Let's move the stack  │
│ ── in conversation text ───────── │ to compose v2              │
│   Reverse proxy TLS renewal       │ Claude: …                  │
│   …"restart the «docker» service…"│                            │
└───────────────────────────────────┴────────────────────────────┘
 ↵ resume  ^A source  ^R running only  ^S sort  tab preview  esc quit
```

- **List:** two sections — fuzzy/metadata matches (ranked by score, ties by recency), then
  full-text-only matches under a divider with a highlighted snippet. Empty query: all sessions,
  sorted by last activity. Row: title, `~`-shortened cwd, launch source name, relative time,
  running badge.
- **Title:** `custom-title` → `ai-title` → `summary` → first real prompt (truncated) → `(untitled)`.
- **Preview:** header (cwd, branch, launch source, msg count, first/last time), then messages.
  Default scroll: last messages. For a full-text hit: scrolled to the matching message, term
  highlighted.
- **Keys:** `Enter` resume · `Ctrl-A` cycle launch source for the selected session, among its
  `store_sources` · `Ctrl-R` running-only filter · `Ctrl-S` sort last-activity / created ·
  `↑↓ PgUp PgDn` move · `Tab` focus preview (scroll with arrows) · `Esc`/`Ctrl-C` quit.
- **Running session + Enter:** does not launch; status line shows `running in c1 (pid 12345)`.
- **Missing cwd** (project dir deleted): row dimmed; Enter is blocked with status line
  `project dir no longer exists: <path>` (resuming from a different cwd would change Claude's
  project context).

CLI:
- `ccpick [QUERY]` — open TUI with query prefilled.
- `ccpick --list [QUERY]` — no TUI; print TSV `last_ts  source  running  id  cwd  title`, filtered
  by the same two-tier search.
- `ccpick --config-dir <DIR>` (repeatable) — add a source for this run.
- `ccpick --sources` — print resolved sources and stores (name, config dir, launcher, store path),
  for debugging configuration.

## Error handling

- Config file unparseable → error message on stderr and exit 2 (explicit config should not be
  silently ignored).
- `~/.ccs/config.yaml` unparseable → skip ccs sources, status line notes it.
- A source whose `config_dir` or store doesn't exist → dropped; listed with a reason in `--sources`.
- No sources at all → message on stderr and exit 1.
- Unreadable/corrupt JSONL line → skip the line; file with zero parseable lines → skip the file.
  Partially-written last line (active session) is normal and silently ignored.
- Cache failures never fatal: fall back to full scan.
- Launcher binary (`ccs`, `claude`, or custom `argv[0]`) not on PATH → do not exit the TUI; show an
  error in the status line.
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
  ranking, full-text case-insensitivity, snippet extraction, stale-generation cancellation).
- **Sources (temp-dir fixtures):** ccs config with default ordering; symlinked `projects/` from
  several sources collapsing into one store; `$CLAUDE_CONFIG_DIR` equal to a ccs instance dir is
  deduped; config-file and `--config-dir` sources; nonexistent dirs dropped.
- **Live:** stale vs live `sessions/*.json`; default launch source by latest `startedAt`.
- **Provider boundary:** a test-only fake provider (in-memory sessions) drives `stores`, `scan`,
  `search` and `App` tests, proving the shared layers don't depend on Claude specifics.
- **Launch:** `LaunchPlan` for each source kind — ccs argv, `claude` with and without
  `CLAUDE_CONFIG_DIR` (including removal of an inherited value), `Command` argv; the exec itself is
  not tested.
- **UI:** state-transition tests on `App` (key → state) without a terminal; one render snapshot
  test with ratatui's `TestBackend`.
- **Manual acceptance:** against real data — a full-text query for a term known to appear only
  mid-conversation finds that session; sessions from both a ccs account and `~/.claude` appear; a
  running session is badged and blocked; Enter resumes via `ccs <account>` for ccs sessions and via
  `claude` for `~/.claude` sessions, in the right cwd.
