# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Leftover guard: a process whose parent is gone that keeps a core busy for 10 minutes gets lower
  priority and is listed in `nodoka status` with the command that stops it
  (`leftover_busy_minutes`, 0 turns it off).
- Headless browsers (Playwright's `chrome-headless-shell`, Chrome, Chromium, Edge or Firefox with
  `--headless`) are recognised as heavy work, even when a plain script drives them.
- On macOS, sustained swapping counts as memory pressure. Apple's level stayed at *warn* while a Mac
  swapped 150 MB/s, so the memory brake never acted.

### Changed

- Dev servers started without a terminal (by coding agents) get lower priority while every core is
  busy, like builds and tests.

### Fixed

- Running `nodoka install` again on macOS (to upgrade) no longer leaves the helper off: starting the
  new one is retried until the old one has finished resuming what it paused.
- Test helpers never outlive their test run: the fake jobs and `nodoka daemon --test` exit once the
  process that started them is gone. A busy child of a killed fake job had kept two cores busy for 36 hours.

## [0.1.0] - 2026-11-20

First release, for macOS and Linux (including WSL 2).

### Added

- Background helper that recognises installs, builds, tests and dev servers for JavaScript/TypeScript
  (React, Vue, Angular, Svelte and their tools), Ruby, Python, Go, Rust, JVM, PHP, Elixir/Erlang/Gleam,
  .NET, Swift, Dart and native build tools — including tools that rename their own process (webpack,
  Next.js, npm, Puma, gunicorn).
- Admission: a few builds and installs at a time; the rest wait their turn, at most 60 seconds. Test runs
  never wait (a paused test fails its timeouts) but take a slot.
- Idle sleep for dev servers. A sleeping server wakes on the next connection or request — including
  servers that wait in a blocking `accept()` — or on the next file saved in its project, so hot reload
  keeps working.
- Low-memory brake that pauses the newest busy build at critical memory pressure and resumes it when
  memory recovers.
- Lower priority (nice 10) for heavy background jobs while every core is busy.
- Fewer workers per tool: a Node/Bun preload plus caps for Cargo, Go, Turbopack, Rolldown (Vite), pnpm,
  Python, pytest-xdist, uv, Bundler, Rails parallel tests and .NET. While memory is short, Node tools that
  start get 2 workers.
- Leaner memory settings: Node's young generation back to Node 22's size (`--max-semi-space-size=16`),
  Bun's `--smol`, and `MALLOC_ARENA_MAX=2` on Linux.
- One-line notices in the terminal when a job started there has to wait or is paused.
- `nodoka status`, `explain`, `off`/`on`, `install`/`uninstall` and `daemon --dry-run`.
- Optional `~/.nodoka/config`, including rules for your own tools.
- Crash-safe ledger: everything paused is resumed if the helper stops or crashes.
- Editor setting that stops VS Code, Cursor, Windsurf and VSCodium from watching `node_modules`,
  `target`, `.venv` and similar folders.

[Unreleased]: https://github.com/sundarshahi/nodoka/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/sundarshahi/nodoka/releases/tag/v0.1.0
