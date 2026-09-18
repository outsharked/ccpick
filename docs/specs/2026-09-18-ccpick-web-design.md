# ccpick web: a local web portal for finding sessions

Date: 2026-09-18
Issue: [#3](https://github.com/outsharked/ccpick/issues/3)

`ccpick web` serves the session list as a page on localhost instead of a TUI. The
catalog, the search and the launchers are the same ones the TUI uses; only the front
end differs.

## Why

Two things a browser does better than a terminal UI:

- **Focusing a running session's terminal.** This is an out-of-band OS action that
  needs no terminal of its own, so a portal can raise any session's window. The TUI
  structurally cannot compete, because it has to be the thing you are looking at.
- **Staying open.** A page that refreshes shows what is running at a glance. The TUI
  is a modal you open, use and leave.

## Design rule: no adoption cost

ccpick stays a viewer, never an owner. Every decision below is subordinate to this.

- **Never required to be running.** The TUI, `--list` and `--sources` keep working
  unchanged. The portal is one more way in, not *the* way in.
- **No state of its own.** It reads the same on-disk transcripts every other tool
  reads and writes nothing but ccpick's existing metadata cache.
- **Sessions start through the user's own launcher** — `ccs <account> --resume <id>`,
  exactly what they would have typed. No wrapper process between the user and the
  agent, and nothing that must stay alive for a session to keep running.
- **No daemon to install or autostart.** You run it when you want it.

The server lives behind a default-on `web` cargo feature, so `--no-default-features`
builds the TUI with no HTTP dependency at all. That keeps the promise literal.

## Scope

In scope: browse, search, preview, focus a running session, resume a stopped one in a
new terminal, and the paste-a-command fallback when ccpick cannot launch it.

Out of scope, deliberately: starting brand-new sessions, remote access, HTTPS,
WebSockets, a config UI, persisted tokens or bookmarkable URLs, and any theme control
beyond `prefers-color-scheme`.

## CLI

```
ccpick web [--port <n>] [--refresh <secs>] [--no-open]
```

A subcommand, so the TUI's positional query argument stays unambiguous. The existing
global options — `--config`, `--config-dir`, `--no-cache` — apply unchanged, because
the portal builds its catalog exactly as the TUI does.

The port defaults to an ephemeral one, chosen by the OS, and the resulting URL is
printed. A token is minted per run, so a fixed port would not make the URL
bookmarkable anyway; `--port` exists for people who want a predictable one. The browser
is opened automatically unless `--no-open` is given, in which case the URL is only
printed.

## Architecture

```
src/web/
  mod.rs      lifecycle: bind, mint token, open the browser, serve
  server.rs   tiny_http wiring; the only module that knows the HTTP crate
  route.rs    route(&Req, &Portal) -> Res
  state.rs    Portal: catalog, generation, subscribers, refresh thread
  assets/     index.html, app.css, app.js (embedded with include_str!)
```

`route()` is pure over a `&Portal` and returns a status, headers and a body. It never
touches a socket, so almost the whole server is testable without binding a port.
`server.rs` translates a `tiny_http::Request` into that call and writes the result
back. Swapping the HTTP crate later means rewriting one file.

One addition outside `web/`: `launch::spawn_in_new_terminal`, beside the existing
`exec` (Unix) and `run_and_wait` (Windows). It is the same concern — running a
`LaunchPlan` — and belongs with its siblings rather than in the web module, so the TUI
could use it later.

### Why blocking HTTP

`tiny_http` is blocking and thread-per-connection. Every existing code path —
`Catalog::build`, the rayon full-text scan, `tasklist.exe`, `wsl.exe` — is
synchronous, so an async runtime would mean `spawn_blocking` around all of it for no
benefit at this concurrency. A handful of local tabs does not need an event loop.

## Endpoints

| Method | Path | Purpose |
|---|---|---|
| GET | `/` | the page (token arrives as `?t=`) |
| GET | `/api/sessions` | list, warnings, generation |
| GET | `/api/search?q=` | ranked session ids plus conversation hits |
| GET | `/api/messages?id=&agent=` | transcript for the preview pane |
| GET | `/api/events` | SSE: a generation number |
| POST | `/api/launch` | resume a session in a new terminal (body: `{id, agent}`) |
| POST | `/api/focus` | raise a running session's terminal (body: `{id, agent}`) |

Search runs server-side so the nucleo fuzzy match over metadata and the rayon
full-text scan over transcripts are the same code the TUI runs. Ranking cannot drift
between the two front ends, and conversation matches come for free. A local round trip
per debounced keystroke is negligible.

SSE carries only a generation number. The page refetches what it needs; the server
never diffs state or tracks what a client has seen.

## Refresh

A single loop, default every 10 seconds, `--refresh <secs>` to change, skipping a tick
if the previous one is still running.

It rebuilds the catalog through the existing mtime cache, so a quiet cycle is cheap.
Liveness probing is the real cost: from a Windows host it is a `tasklist.exe` call plus
one `wsl.exe` call per running distro, roughly 1.3 seconds. That is why this is a
periodic loop and never a per-request probe.

The generation bumps only when the payload actually changes, so idle tabs receive
nothing.

## Security

The portal has an endpoint that spawns processes. It is treated accordingly.

- **Bind `127.0.0.1` only.** There is no flag to bind elsewhere.
- **A 128-bit token, minted per run** (`getrandom`), delivered in the URL that gets
  opened, then required as an `X-CCPick-Token` header on every API call. The page
  removes it from the address bar with `history.replaceState`.
- **Mutating requests require that header**, which forces a CORS preflight that a
  cross-origin page cannot satisfy.
- **Any request carrying a foreign `Origin` is rejected**, and no CORS headers are
  ever sent.

The token reaches the browser as an element of the "open URL" process's argv (`xdg-open`,
`open`, or `cmd.exe start`). On Linux, `/proc/<pid>/cmdline` is world-readable, so it is
readable by *any* local user, not just other processes owned by the same account —
wider than the transcripts themselves, which the filesystem restricts to the owning
user. It is not the same trust boundary, and the token check compares byte-for-byte
without short-circuiting so guessing it can't be sped up by timing a partial match
either.

## Picking a session

- **Running** → `focus::focus_session`, unchanged from the TUI.
- **Stopped** → `spawn_in_new_terminal`, which resolves an argv per platform:
  - Windows: `wt.exe -d <cwd> <launcher>`. When `wt.exe` isn't on `PATH`, the command is
    returned unprefixed and `spawn_in_new_terminal` asks Windows for a new console itself
    (`CREATE_NEW_CONSOLE`) rather than routing through `cmd.exe`, which would re-parse an
    argument line already quoted for `CreateProcessW` under its own, different rules — a
    project directory or account name containing `&`, `%`, `^` or `|` would break the launch.
  - macOS: `osascript` driving `Terminal.app`'s `do script`.
  - Linux: `$TERMINAL`, then `x-terminal-emulator`, then a short list.
- **Neither** → the response carries the ready-to-paste command and the page shows it
  with a copy button, the same fallback the TUI already offers for cross-environment
  sessions.

A session in an environment other than the host's own (Windows ↔ WSL, or either of
those reaching macOS) always falls back to the paste command: cross-environment
launching is out of scope for this project, on the portal exactly as on the TUI.

Linux desktops have no reliable answer here. When nothing matches, fall back to the
paste command rather than guessing at a terminal that may not exist.

## Errors

Handlers build a `Res` directly — `Res::json` for a payload, `Res::error` for a message plus
a status — rather than a separate `Result`/error type. Catalog warnings ride along in the
sessions payload exactly as they surface in the TUI's footer today. A failed launch returns
the paste command so the user is never left with nothing.

## Testing

- **Handlers**: `route()` is pure over a catalog, so tests use `fake_catalog()` and
  assert on parsed JSON. This covers auth, `Origin` rejection, 404s and every endpoint.
- **Terminal spawning**: `terminal_command(plan, target, host, terminal_env, on_path) ->
  Option<Vec<String>>` returns argv without spawning anything; `on_path` is injected rather
  than checking the real `PATH`, so every platform's behaviour is testable on every platform.
- **SSE**: the event encoder is a pure function over a generation number.
- **Integration**: one test binds `127.0.0.1:0` and makes a real request, asserting it
  succeeds with the token and returns 401 without it.

No test may depend on a real browser, a real terminal emulator, or the developer's own
session data.
