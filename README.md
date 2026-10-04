# portmap

A bookmark page for everything listening on localhost. portmap finds your running dev servers,
labels them by project and page title, takes a small screenshot of each, and gives you one page
to click through. It lives at **http://localhost:7878**.

- **Auto-discovers** HTTP services on every port above 1024 (via `lsof`). Labels come from the
  process's working directory (nearest `package.json` / `Cargo.toml` / `.git` …) and the page `<title>`.
- **Keeps itself clean.** New services appear within seconds. Stopped ones are marked
  *stopped*, then removed along with their cached thumbnail after 10 minutes. Pinned services
  stay as bookmarks and show as *offline*.
- **Thumbnails** come from the Chrome / Brave / Edge you already have, run headless one capture at a
  time, then shrunk to a ~30 KB JPEG. They are retaken when a service restarts, its title changes,
  you press refresh, or they are 15 minutes old while you're looking.
- **Pin, rename, hide** any entry. Databases, desktop-app internals (Raycast, editors, …) and APIs
  without a project that answer `/` with 4xx go under a collapsed *Other listeners* list, and you can
  pin any of them to make a card.
- **Manual refresh** button (or press `r`) rescans and re-probes everything immediately.
- Grid or list view, thumbnails on or off, and dark (default) or light theme, all remembered per browser.

## Why it's light

- One ~800 KB native binary (Rust, `tiny_http` + `serde`), using **~3–4 MB RAM and ~0% CPU** when idle.
- Scans every 3 s only while a portmap tab is visible, and every 30 s otherwise. The page stops polling
  when hidden, and unchanged state is a `304` with an empty body.
- HTTP probes are cached per (port, pid), so your dev servers aren't flooded with requests. A port is
  re-probed only when its process changes, or once a minute while you're watching.
- No Docker, no Node runtime, no web fonts, nothing fetched from the internet.

## Install (macOS)

```sh
cargo install --path .     # builds and puts `portmap` in ~/.cargo/bin
portmap install            # launchd user agent: starts at login, restarts on crash
open http://localhost:7878
```

`portmap install` writes `~/Library/LaunchAgents/dev.portmap.plist`, which runs the binary at a lower CPU
priority. Run `portmap uninstall` to remove it. After rebuilding, run `portmap install` again to restart
the agent on the new binary.

To run it in the foreground without launchd:

```sh
portmap            # same as `portmap serve`
```

Linux works too (needs `lsof`). Run `portmap serve` from a systemd user unit; `install` is macOS-only.

## Usage

| Action | How |
| --- | --- |
| Open a service | click its card |
| Filter | `/`, then type; `Enter` opens the first match, `Esc` clears |
| Rescan now | refresh button or `r` |
| Grid / list view | header toggle, or `g` / `l` |
| Show / hide all thumbnails | header toggle or `t`. While off, portmap launches no browser at all |
| Set a layout from a bookmark | `http://localhost:7878/?view=list&thumbs=off&theme=light` |
| Pin / rename / retake thumbnail / hide | hover a card |
| Reorder pinned items | drag and drop (grid or list), or focus one and press `Alt` + arrow keys |
| Forget a stopped service | hover it, then **×** |
| Clear all stopped services | **Clear** beside *Recently stopped* |

## Configuration

| Env var | Default | |
| --- | --- | --- |
| `PORTMAP_PORT` / `--port` | `7878` | UI port |
| `PORTMAP_HOME` | `~/.portmap` | state (`state.json`), thumbnails, log |
| `PORTMAP_STALE_MINUTES` | `10` | how long stopped services linger |
| `PORTMAP_BROWSER` | auto | Chromium-family binary for thumbnails |
| `PORTMAP_NO_THUMBS=1` | off | disable thumbnails entirely |

To use env vars with the launchd agent, add an `EnvironmentVariables` dict to the plist.

## Security

The server binds to `127.0.0.1` only and rejects requests whose `Host` header isn't
`localhost`/`127.0.0.1`/`[::1]` on its own port, which blocks DNS rebinding. Mutations require a JSON
content type and a same-origin `Origin` header. portmap only reads the processes it observes and
never signals, kills, or starts them (apart from its own headless browser for thumbnails).

## Layout

```
src/scan.rs      lsof / ps / cwd discovery, raw HTTP probe, project + runtime labels
src/scanner.rs   adaptive scan loop, probe cache, thumbnail scheduling
src/store.rs     entries, lifecycle (live → stopped → removed), persistence, view versioning
src/thumbs.rs    headless-browser capture worker
src/server.rs    HTTP API + static UI, host/origin guards
src/launchd.rs   install / uninstall the user agent
assets/          index.html, style.css, app.js (no build step, embedded into the binary)
```
