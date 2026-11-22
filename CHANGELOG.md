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

- CPU budget for agent work: builds, installs and dev servers started without a terminal share at
  most half your cores (`agent_cpu_percent`), running in slices of each second past that. Test runs
  and anything started in a terminal are exempt.

- Builds, type checks and installs wait for memory before they start: nodoka remembers how much
  each needed last time in that folder, and while memory is short the next run waits (at most 10
  minutes) until there's room, instead of growing and then being paused still holding it all.
- `nodoka status` lists the apps holding the most memory while memory is short.

### Changed

- A build paused for memory for 10 minutes now runs to the end at low priority, one at a time, even
  while memory stays critical. Before, it could stay paused for hours, holding its memory while its
  agent waited (two type checks held 7 GB for 3 hours).

- The memory brake resumes a paused build only once there's free memory for all of it, not just after
  20 calm seconds (a lone 5 GB type check flapped every 25). It pauses the last busy build too: a
  lone 9 GB `next build` froze a 16 GB Mac while nodoka let it run.
- Dev servers started without a terminal (by coding agents) get lower priority while every core is
  busy, like builds and tests.

### Fixed

- Running `nodoka install` again on macOS (to upgrade) no longer leaves the helper off: starting the
  new one is retried until the old one has finished resuming what it paused.
- A server that opens its port a moment after starting (Vite does) is recognised within seconds,
  not after 20: nodoka looks every second while a job is new.
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
