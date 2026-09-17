# Agent guidelines for ccpick

ccpick is a Rust TUI (ratatui) that finds and resumes Claude Code sessions across multiple
config directories (ccs accounts, `~/.claude`, `$CLAUDE_CONFIG_DIR`, configured dirs). Design:
`docs/specs/2026-09-16-ccpick-design.md`.

## Builds and checks

Use mise tasks, not ad-hoc cargo commands (`mise tasks` lists them):

| Task | What it does |
|---|---|
| `mise dev -- <args>` | Run from source; args after `--` go to ccpick |
| `mise check` | `cargo fmt --check`, clippy with `-D warnings`, tests |
| `mise test` | Tests only |
| `mise lint` | Clippy only |
| `mise test-windows` | WSL only: run the test suite as Windows binaries via interop |
| `mise lint-windows` | Clippy for the Windows target |
| `mise format` | `cargo fmt` |
| `mise build` | Release build |
| `mise install-bin` | Install the release binary to `~/.cargo/bin` |
| `mise release <version>` | Bump version, run checks, commit and tag (does not push) |

- **Run `mise check` before every commit.** It must pass: no fmt diff, no clippy warnings, all
  tests green, no compiler warnings.
- After touching any `cfg(windows)` or `cfg(unix)` code, also run `mise test-windows` and
  `mise lint-windows` (WSL only).
- New mise task names must not collide with built-in mise commands (e.g. `fmt`, `install`,
  `run`, `use`), or `mise <task>` runs the built-in instead. Check with `mise <name> --help`.

## Releases

- Releases are built by [dist](https://opensource.axo.dev/cargo-dist/) (pinned in `mise.toml`,
  config in `dist-workspace.toml`). `.github/workflows/release.yml` is generated: after changing
  dist config run `mise exec -- dist generate` instead of editing the workflow by hand.
- Targets: Linux x86_64/aarch64, macOS x86_64/aarch64, Windows x86_64; shell and PowerShell
  installers.
- Pushing a `v*` tag publishes a public GitHub release — only do it when asked.
- `.github/workflows/ci.yml` runs `mise run check` on Linux, macOS and Windows for every push and
  PR.

## Running the binary as an agent

- Never run the TUI (`ccpick` or `mise dev` without `--list`/`--sources`) from an agent or any
  non-interactive shell: it takes over the terminal, and Enter `exec`s a real Claude session.
- For smoke tests use `mise dev -- --sources` and `mise dev -- --list <query>`. Both read the
  real session data; neither writes anything except ccpick's own cache.
- Never modify anything under `~/.ccs` or `~/.claude`.

## Architecture rules

- **Provider boundary:** all agent-specific code lives behind the `Provider` trait in
  `src/providers/`. Nothing outside `src/providers/claude/` may know Claude file formats, ccs, or
  `CLAUDE_CONFIG_DIR`. Allowed elsewhere: the registry in `src/providers/mod.rs`, user-facing
  help/docs text, `default_agent()` in `src/config.rs`, and sample data in tests. This keeps
  other agents (e.g. Codex CLI) addable without touching shared code.
- **Platforms:** process liveness goes through `src/process.rs`; launching uses `exec` on Unix
  and spawn-and-wait on Windows (`src/launch.rs`). Don't add the `nix` crate.
- **ratatui:** use its re-exported `ratatui::crossterm`; don't add a separate crossterm
  dependency.
- **Rendered heights:** measure wrapped text with `Paragraph::line_count` (ratatui feature
  `unstable-rendered-line-info`), never with char-count / width estimates, which under-count
  word wrapping and wide characters.
- **Resolve paths before storing them:** anything passed to a launched process (e.g. a
  `CLAUDE_CONFIG_DIR` value) must be absolute, because the launch changes directory first.

## Windows and WSL

- Environment logic (`src/env.rs`, `src/homes.rs`, `src/process.rs`, `src/shell.rs`,
  `src/clipboard.rs`) is agent-neutral; keep Claude specifics in `src/providers/claude/`.
- Windows-only code behind `cfg(windows)`, Unix-only behind `cfg(unix)`. After touching either,
  run `mise lint-windows` and `mise test-windows` as well as `mise check`.
- Canonicalize with `dunce::canonicalize`. Compare Windows paths case-insensitively.
- Never access a stopped WSL distro (`\\wsl.localhost\<distro>` boots it) unless it's listed in
  `[environments] wsl_distros`.
- Commands shown for another environment use that environment's own path form and shell syntax
  (PowerShell on Windows, POSIX elsewhere).

## Testing

- Write tests first (red, then green) for behaviour changes.
- Tests must not depend on the real environment: build `Settings` with an empty `env` map and a
  tempdir `home`; use tempdirs for filesystem fixtures; pin "now" for anything time-based.
- Shared layers (catalog, search, UI state) are tested through `providers::fake::FakeProvider`
  and `catalog::fake_catalog()`, not the Claude provider.
- Claude transcript parsing is tested against small synthetic files in `tests/fixtures/claude/`.
- UI state is tested terminal-free via `App::handle_key`; rendering via ratatui's `TestBackend`.
- Performance targets on ~300 transcripts / ~300 MB: warm start < 100 ms, cold start < 1 s,
  full-text search < 500 ms. Re-measure (`time mise dev -- --list <word>`) after touching
  scanning or search.

## Commits and docs

- Commit only when asked; never push, tag, or publish a release unless asked.
- This is a public repo: keep README, docs, examples and test data generic — no personal paths,
  hostnames, usernames or private infrastructure names.
- Design specs go in `docs/specs/` and implementation plans in `docs/plans/`, named
  `YYYY-MM-DD-<topic>-design.md` / `YYYY-MM-DD-<topic>.md`. Never use `docs/superpowers/` (or any
  other tool-specific directory), even if a skill or template suggests it.
