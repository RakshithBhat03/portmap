# Contributing to portmap

Thanks for your interest in improving portmap. This guide covers how to get set up, what the project
values, and what a good pull request looks like.

## Ground rules

portmap's whole point is being a tiny, quiet, local tool. Changes should keep it that way:

- **Stay light.** One small native binary, a few MB of RAM, ~0% CPU when idle. Avoid new dependencies
  unless they clearly pay for themselves, and mention the binary-size and memory impact in your PR
  if you add one.
- **Be polite to other servers.** Don't add scans or probes that hit users' dev servers more often.
  Keep the probe cache and adaptive scan intervals intact.
- **Stay local and read-only.** The server binds to `127.0.0.1`, keeps its `Host` / `Origin`
  guards, and never signals, kills, or starts other processes (apart from its own headless browser
  for thumbnails). Nothing is fetched from the internet.
- **No build step for the UI.** `assets/` is plain HTML, CSS, and JavaScript embedded into the
  binary. No frameworks, bundlers, or web fonts.

If you're planning a larger change (a new feature, a new dependency, or a change to the on-disk
state format), please open an issue first so we can agree on the approach.

## Getting started

You need a stable Rust toolchain (install via [rustup](https://rustup.rs)) and `lsof`. macOS is the
primary platform; Linux works for `portmap serve`.

```sh
git clone https://github.com/RakshithBhat03/portmap.git
cd portmap
cargo run                                # serves the UI on http://localhost:7878
```

To keep your development run separate from an installed portmap, use a different port and state
directory:

```sh
PORTMAP_HOME="$(mktemp -d)" cargo run -- --port 7979
```

Thumbnails need Chrome, Brave, or Edge. Set `PORTMAP_NO_THUMBS=1` if you don't need them.

Changes to `assets/` are embedded at compile time, so re-run `cargo run` to see them.

## Project layout

```
src/main.rs      CLI entry point, config, shared state
src/scan.rs      lsof / ps / cwd discovery, raw HTTP probe, project + runtime labels
src/scanner.rs   adaptive scan loop, probe cache, thumbnail scheduling
src/store.rs     entries, lifecycle (live → stopped → removed), persistence, view versioning
src/thumbs.rs    headless-browser capture worker
src/server.rs    HTTP API + static UI, host/origin guards
src/launchd.rs   install / uninstall the user agent
src/testutil.rs  shared test fixtures
assets/          index.html, style.css, app.js
```

## Tests and checks

CI runs these three commands on every pull request. Please run them locally before pushing:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Tests live next to the code they cover in a `#[cfg(test)] mod tests` block. When adding tests:

- Use the fixtures in `src/testutil.rs`: `TempDir` for throwaway state, and the scripted HTTP server
  for probes.
- Bind real sockets on ephemeral loopback ports (`127.0.0.1:0`), never fixed ports.
- Never touch `~/.portmap`, the network, or services running on the developer's machine.
- Keep tests fast and deterministic. Avoid sleeps where a channel or explicit signal will do.

Bug fixes should come with a test that fails without the fix. UI changes in `assets/` aren't covered
by tests, so describe how you checked them manually and include a screenshot.

## Pull requests

1. Fork the repo and create a branch from `main`.
2. Keep each PR focused on one change. Unrelated refactors belong in their own PR.
3. Write commit messages and PR titles in [Conventional Commits](https://www.conventionalcommits.org)
   style, matching the existing history: `feat: …`, `fix: …`, `docs: …`, `test: …`, `chore: …`.
4. Update `README.md` if you change user-visible behavior, keyboard shortcuts, CLI flags, or
   environment variables.
5. In the PR description, explain what changed and why, and how you verified it.

## Reporting bugs

Open an issue with:

- your OS and version, and how you run portmap (`portmap install` or `portmap serve`)
- what you expected and what happened
- relevant lines from `~/.portmap/portmap.log` (written when running under launchd), if any
- for discovery problems, the output of `lsof -nP -iTCP -sTCP:LISTEN` for the affected port

For security issues, please don't open a public issue. Contact the maintainer privately through
GitHub instead.

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](LICENSE).
