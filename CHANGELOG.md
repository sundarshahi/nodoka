# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- A command that needed under 512 MB last time (rubocop on a few files: 100 MB) starts at once
  instead of queuing behind builds and tests, and takes no slot. Seven agent lint runs had queued;
  an editor hook that lints each edited file gives up after 60 s, as long as a queued job waits.
- `bundle exec` and `mise x --` count as launchers: `bundle exec rubocop` is rubocop.
- Worktree cleaning watches free disk space: under 25 GB free it checks every 5 minutes and clears
  dependency folders in worktrees idle an hour, not a day. A disk had run down to 2.5 GB free while
  19 GB of `node_modules` sat in worktrees idle 6–22 hours.

### Fixed

- A job paused in its first moments sometimes kept running ("found running … in paused job", 12
  times in four days). macOS drops a SIGSTOP that arrives while a process execs, and `mise x`,
  `bundle exec` and `npm exec` exec just then (10 of 40 lost in a test). nodoka now checks for
  100 ms after pausing and stops again whatever still runs.
- A dev server woken by a request was logged as "waited the maximum time, started anyway".
- A type check in an agent's new worktree, or run as `npx tsc` where it ran as `npm exec tsc` before,
  counted as needing no memory, so two 5 GB type checks started side by side. With no exact record,
  a command now counts as needing what the same program usually needs.
- An idle Gradle, Kotlin or Bazel build server was paused like a dev server; paused, it kept 4.7 GB in
  swap for hours and never reached its own idle exit. It is now asked to exit; the next build starts
  a fresh one.
- `nodoka status | head` (or any reader that stops early) crashed with a broken-pipe panic; it now
  ends quietly.

## [0.3.0] - 2026-11-26

### Added

- Native Windows (x64 and ARM64): `irm …/install.ps1 | iex`. Everything but idle-server sleep; see
  [docs/windows.md](docs/windows.md). Releases include Windows builds.
- Heavy work no rule knows (a Java map-tile build, a data script) is found by its load when a coding
  agent starts it: more than 1.5 cores, or a core while holding 1 GB, for 30 seconds. It is then
  slowed, paused and resumed like a build, and never asked to exit.

### Fixed

- The memory-full notice never came: it waited for 2 unbroken minutes at critical, and memory
  flips critical → warn every few seconds (98 times one evening). Short dips no longer restart it.

## [0.2.0] - 2026-11-25

### Added

- A notification names the biggest apps when memory stays critically low for 2 minutes and no
  build, test or install is running, at most once an hour (`notify_after_minutes`, 0 turns it off).
  In WSL 2 it is a Windows notification.
- [Windows setup guide](docs/windows.md): nodoka inside WSL 2, step by step.
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
  each needed last time in that folder, and while memory is short the next run waits (at most a
  minute) until there's room, instead of starting and fighting your apps for memory.
- `nodoka status` lists the apps holding the most memory while memory is short.

- Disk cleaning for agent worktrees (`clean_dirs`): in a worktree idle for a day, rebuildable
  ignored folders (`node_modules`, `.next`, `.turbo`…) are deleted; after a week the worktree is
  removed if nothing would be lost (no changes, commit on a remote, no other ignored files).

- Stuck test runs end: a test run an agent started that used no CPU for 30 minutes (Jest with an
  open handle) is asked to exit (`stop_idle_tests_minutes`). Watch modes, headless browsers and runs
  started in a terminal are never stopped.

- Idle coding-agent sessions (Claude Code, Codex) are listed in `nodoka status` after 24 hours with
  the memory they hold; `close_idle_agents_after_hours` asks them to exit while memory is short.
- While memory is short, Jest and Vitest started with an explicit worker count (`--maxWorkers=4`,
  `-w 4`) get at most 2 workers, like tools that size their pool from the core count.
- An agent's test run still going after 30 minutes while memory is critical is asked to exit
  (`stop_long_tests_minutes`, 0 turns it off).
- Builds paused for memory stay paused across a restart of the helper (each update restarted it
  and resumed them all at once); `nodoka off` and uninstall still resume everything.
- Paused builds were resumed 5 seconds apart, each seeing memory the one before was about to take
  back (three 4 GB type checks in 11 seconds); a build resumed or started in the last minute now
  counts as holding its memory.
- Security and dependency scanners are recognised as heavy work: whole-repo `gitleaks`, `trufflehog`,
  `semgrep`, `trivy`, `osv-scanner`, `grype`, `syft` and `codeql database` (an agent's `gitleaks
  detect` used 5 cores). Pre-commit checks of staged changes are left alone.

### Changed

- While memory is critical (or was under 20 seconds ago) no new build or install starts for up to
  10 minutes, whatever `memory_brake` is and whether or not its size is known, and none starts while
  the machine freezes. Before, one started after `max_wait_seconds`, or on a 1-second blip to warn.
- What each command needed is kept in `~/.nodoka/peaks` across restarts; a restart made a known
  3.9 GB type check look new, and it started at critical memory.
- An agent's build held 10 minutes for memory (paused, or waiting to start while memory stays
  critical) is asked to exit instead of getting its turn; its agent can run it again. Builds started
  in a terminal still get their turn when the machine isn't freezing.

- While the machine is freezing (swapping 100+ MB/s on macOS, PSI full 40%+ on Linux), the newest
  busy build is paused even in the slow lane, until there's room for it.

- At critical memory, busy builds now keep running in a slow lane (spare CPU and disk time only:
  macOS background priority, Linux idle disk priority) instead of being paused, so they finish and
  free their memory while your apps stay responsive. `memory_brake = pause` brings pausing back.
  A job waiting for memory before it starts waits at most `max_wait_seconds`.

- A build paused for memory for 10 minutes now runs to the end at low priority, one at a time, even
  while memory stays critical. Before, it could stay paused for hours, holding its memory while its
  agent waited (two type checks held 7 GB for 3 hours).

- The memory brake resumes a paused build only once there's free memory for all of it, not just after
  20 calm seconds (a lone 5 GB type check flapped every 25). It pauses the last busy build too: a
  lone 9 GB `next build` froze a 16 GB Mac while nodoka let it run.
- Dev servers started without a terminal (by coding agents) get lower priority while every core is
  busy, like builds and tests.

### Fixed

- An idle agent session with a wait loop left running under it (`until …; do sleep 10; done`) was
  never closed: each new `sleep` counted as a tool call. Only programs the session starts itself count now.
- Bun-built tools, Claude Code's `claude` among them, refused to start in a shell with nodoka on:
  Bun kept the quotes around the preload path in `BUN_OPTIONS` (`preload not found "\"…\""`).

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

[Unreleased]: https://github.com/sundarshahi/nodoka/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/sundarshahi/nodoka/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/sundarshahi/nodoka/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/sundarshahi/nodoka/releases/tag/v0.1.0
