# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0] - 2026-11-20

First release, for macOS and Linux (including WSL 2).

### Added

- Background helper that recognises installs, builds, tests and dev servers for JavaScript/TypeScript,
  Ruby, Python, Go, Rust, JVM, PHP, Elixir/Erlang/Gleam, .NET, Swift, Dart and native build tools.
- Admission: a few builds, tests and installs at a time; the rest wait their turn, at most 60 seconds.
- Idle sleep for dev servers with wake on the next connection or request, including servers that
  wait in a blocking `accept()`.
- Low-memory brake that pauses the newest build or test at critical memory pressure and resumes it
  when memory recovers.
- Lower priority for heavy background jobs while every core is busy.
- Fewer workers per tool: a Node/Bun preload plus caps for Cargo, Go, Turbopack, Rolldown, pnpm, Python,
  pytest-xdist, uv, Bundler, Rails parallel tests and .NET.
- One-line notices in the terminal when a job started there has to wait or is paused.
- `loadline status`, `explain`, `off`/`on`, `install`/`uninstall` and `daemon --dry-run`.
- Optional `~/.loadline/config`, including rules for your own tools.
- Crash-safe ledger: everything paused is resumed if the helper stops or crashes.
- Editor setting that stops VS Code, Cursor, Windsurf and VSCodium from watching `node_modules`,
  `target`, `.venv` and similar folders.

[Unreleased]: https://github.com/sundarshahi/loadline/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/sundarshahi/loadline/releases/tag/v0.1.0
