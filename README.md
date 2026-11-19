# loadline

**Your computer stays responsive while dev servers, builds, installs and AI coding agents run. Any stack. No new commands.**

[![ci](https://github.com/sundarshahi/loadline/actions/workflows/ci.yml/badge.svg)](https://github.com/sundarshahi/loadline/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

A machine rarely freezes because of one heavy program. It freezes when five builds, three dev servers
and a test run all start at once — which is exactly what coding agents do when they work in several
git worktrees. loadline is a small background helper (a 500 KB program using about 3 MB of memory)
that keeps that from happening:

- **Fewer workers per tool.** Next.js, Turbopack, Jest, Vitest, Cargo, Go, pytest, Bundler and others
  are told about half your cores, so one build can't take all of them.
- **A few heavy jobs at a time.** Builds, tests and installs beyond the limit wait their turn (at most a
  minute) instead of all fighting for memory.
- **Idle servers sleep.** A dev server nobody has used for 10 minutes is paused; the next request wakes it
  in under a second.
- **A brake for low memory.** When memory gets critically low, the newest build or test is paused and
  resumed as soon as memory recovers — before the machine starts swapping itself to a standstill.

You keep typing `npm run dev`, `bin/rails server` and `cargo build` exactly as before. Nothing is
turned off and nothing is killed.

## Install

macOS and Linux (including WSL 2):

```sh
curl -fsSL https://raw.githubusercontent.com/sundarshahi/loadline/main/install.sh | sh
```

or build it yourself (Rust 1.85+):

```sh
cargo install --git https://github.com/sundarshahi/loadline
loadline install
```

Then open a new terminal and restart any coding agents. `loadline install` adds one marked line to your
shell startup file, starts the helper at login (a LaunchAgent on macOS, a systemd user service on Linux)
and stops VS Code-style editors from watching `node_modules`, `target`, `.venv` and similar folders.
`loadline uninstall` removes all of it.

## What you'll see

Mostly nothing. When you're curious:

```console
$ loadline status
memory pressure: warn   load: 7.9 on 10 cores   builds/tests/installs at once: 2
  59101  build    running                 887 MB  next-build (v16.4.5)
  59280  build    waiting for a slot        6 MB  cargo build -q
  59487  build    waiting for a slot        6 MB  cargo build -q
  48810  dev      asleep: idle            212 MB  rails server -p 3000
```

If something you started in a terminal has to wait, that terminal gets one line saying why:

```text
[loadline] `next build` is waiting for a free slot. It starts on its own within 60s. See `loadline status`.
[loadline] `next build` continues.
```

## Works with

loadline recognises dev work from the command line, for every common stack:

| Stack | Examples it recognises |
|---|---|
| JavaScript / TypeScript | `npm`/`pnpm`/`yarn`/`bun install`, `next dev`/`build`/`start`, `vite`, `nuxt`, `astro`, `remix`, `gatsby`, `expo`, `webpack`, `rspack`, `parcel`, `esbuild`, `tsc`, `eslint`, `jest`, `vitest`, `playwright`, `turbo`, `nx` |
| Ruby | `rails server`/`test`/`assets:precompile`, `puma`, `bin/dev` (foreman, overmind), `bundle install`, `rspec`, `rake test`, `rubocop`, `jekyll` |
| Python | `manage.py runserver`/`test`, `flask run`, `uvicorn`, `gunicorn`, `fastapi dev`, `streamlit run`, `pytest`, `tox`, `pip`/`uv`/`poetry`/`pdm install`, `mypy`, `ruff`, `mkdocs serve` |
| Go | `go run`/`build`/`test`/`mod download`, `air`, `hugo server`, `golangci-lint`, `wails dev` |
| Rust | `cargo build`/`check`/`test`/`run`/`watch`, `cargo leptos`, `cargo tauri`, `trunk serve`, `dx serve`, `bacon` |
| JVM | Maven (`spring-boot:run`, `quarkus:dev`, `test`, `package`…), the Gradle and Kotlin daemons, Bazel's server, `sbt`, Leiningen |
| PHP | `php artisan serve`/`test`, `php -S`, `composer install`, `phpunit`, `pest`, `symfony serve` |
| Elixir / Erlang / Gleam | `mix phx.server`/`test`/`compile`/`deps.get`, `rebar3`, `gleam` |
| .NET, Swift, Dart | `dotnet watch`/`run`/`build`/`test`, `swift build`/`test`/`run`, `xcodebuild`, `flutter run`/`build` |
| Native and task runners | `make`, `ninja`, `cmake --build`, `meson`, `just`, `task`, `mise run` |

Not sure about a command? Ask:

```console
$ loadline explain bin/rails server
`rails server` is a dev server or watcher. Never queued. If it listens on a port and sits idle, it sleeps; the next request wakes it.
```

Your own tools can be added in the config file (below), and new rules are the easiest way to
[contribute](CONTRIBUTING.md).

### Tested on real servers

Each was started the way a coding agent starts it, left idle until loadline put it to sleep, then sent a
real HTTP request:

| Server | Woke and answered in |
|---|---|
| Rails 8.1 (`bin/rails server`, Puma 8) | 0.66 s |
| Django 6.1 (`manage.py runserver`, with autoreload) | 0.57 s |
| FastAPI (`uvicorn --reload`, polling reloader) | 0.60 s |
| Python `http.server` — macOS / Linux | 0.99 s / 0.63 s |
| Go `net/http` (`go run`) | 0.65 s |
| Java `HttpServer` (custom rule) | 0.63 s |
| Next.js 16.4 `next dev` (Turbopack) | 0.70 s |

## Platforms

| | macOS | Linux and WSL 2 | Windows |
|---|---|---|---|
| Fewer workers per tool | ✓ | ✓ | planned |
| A few heavy jobs at a time | ✓ | ✓ | planned |
| Idle servers sleep, wake on request | ✓ | ✓ | planned |
| Low-memory brake | ✓ (kernel memory-pressure level) | ✓ (pressure-stall information and available memory) | planned |
| Lower priority when every core is busy | ✓ (background priority, undone when load drops) | ✓ (nice 10, kept until the job ends) | planned |
| Starts at login | LaunchAgent | systemd user service | |

Tested on macOS 27 (Apple silicon) and Linux 6.10 (arm64). Release builds cover x86_64 and arm64 for
both. Windows support is the most-wanted contribution — see [CONTRIBUTING.md](CONTRIBUTING.md).

## Commands

```text
loadline status            what is running, waiting or asleep, and why
loadline explain <cmd…>    how loadline treats a command (or a running pid)
loadline off | on          pause loadline (everything it paused resumes) / turn it back on
loadline install           set up for every terminal, editor and agent on this machine
loadline uninstall         remove everything loadline added
loadline daemon            run the background helper in the foreground (for debugging)
```

## Configuration

Everything works without configuration. `~/.loadline/config` (created by `loadline install`) holds
optional settings; changes apply within a few seconds:

```ini
heavy_jobs = 3            # builds/tests/installs at once (default: 1 per 5 GB of memory, at most half your cores)
cpus = 4                  # worker/thread count each tool is told about (default: half your cores, at least 2)
idle_sleep_minutes = 10   # 0 turns idle sleep off
max_wait_seconds = 60     # a waiting job starts anyway after this long

# teach loadline about your own tools (program names, comma-separated)
dev = my-server
build = my-compiler
ignore = some-tool        # never touch these
```

For a single command, `LOADLINE_SKIP=1 <command>` leaves it alone. To turn off the worker caps in one
shell, set `LOADLINE_OFF=1`.

## Safety

loadline is designed to be safe to leave running:

- **It never kills anything.** It only pauses (`SIGSTOP`), resumes (`SIGCONT`) and lowers priority.
- **It only touches your own processes,** needs no root and makes no network connections.
- **It leaves alone anything it doesn't recognise,** including databases, editors and GUI apps.
- **Installs are never paused for memory,** because their downloads would time out.
- **It never pauses the process an interactive shell is waiting on,** so your terminal never prints
  "suspended" or loses its prompt.
- **It is crash-safe.** Before pausing anything it writes down what it is about to pause. If the helper
  stops, crashes or is turned off, everything it paused is resumed.
- **Every change is reversible.** `loadline off` resumes everything at once; `loadline uninstall` removes
  each line it added, exactly.

## Measured

On a 10-core, 16 GB MacBook:

- **A real `next build`** (Next.js 16.4.5, 41 pages): workers 9 → 4, threads 179 → 77, peak memory
  1.47–1.52 GB → 1.05–1.17 GB, same build time.
- **Three agent builds at once** (`next build` and two `cargo build`s) with one slot: they ran one after
  another in arrival order; all three exited 0.
- **The helper:** a 470 KB program (macOS; 600 KB on Linux) using about 3 MB of memory and 0.3% of one
  core while watching a busy machine.
- **The Node preload:** adds about 0.7 ms to each Node start (36.2 ms vs 35.5 ms for `node -e 0`).

## Limits

- **Docker:** containers run inside Docker's own virtual machine, where loadline can't see them. Pair it
  with a lightweight runtime such as [Kiri](https://runkiri.com/), [OrbStack](https://orbstack.dev/) or
  [Colima](https://github.com/abiosoft/colima).
- **A single process typed straight at your prompt** (`pytest`, `bin/rails server`) is never paused,
  because the shell would notice. Commands that start helpers (`npm run build`, `cargo build`, `go test`)
  are paused through their helper processes.
- **zsh's `cmd & wait $!`** reports exit status 145 if `cmd` is paused while zsh waits on it (bash and sh
  don't). Plain foreground commands — what coding agents run — are unaffected.
- **Node tools that rename themselves** (`next build`, `npm`) overwrite the memory their environment was
  read from, so `LOADLINE_SKIP=1` set only on such a command may not be seen. Use `ignore =` in the
  config, or `export LOADLINE_SKIP=1` in the shell.
- **On Linux, lowered priority stays** until the job ends: Linux doesn't let a normal user raise it again.
  It only matters while every core is busy.
- **A server that polls for file changes** keeps the CPU busy, so it only sleeps after nobody has visited
  it for three times the idle time.

## How it works

Two parts. A tiny Node preload and a few environment variables (`CARGO_BUILD_JOBS`, `GOMAXPROCS`,
`PYTHON_CPU_COUNT`, `TURBO_TASKS_AVAILABLE_PARALLELISM`…) make each tool start fewer workers. The
background helper looks at your processes once a second, recognises dev work, groups each job with its
child processes, and decides what may run. [docs/how-it-works.md](docs/how-it-works.md) explains each
decision and the research behind it.

## Uninstall

```sh
loadline uninstall
```

Your settings stay in `~/.loadline/config` in case you come back; delete the folder to remove them too.

## Contributing

Bug reports, new tool rules and platform ports are very welcome. Start with
[CONTRIBUTING.md](CONTRIBUTING.md). Please follow the [code of conduct](CODE_OF_CONDUCT.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at
your option. Unless you explicitly state otherwise, any contribution you submit for inclusion in this
project shall be dual licensed as above, without any additional terms or conditions.
