# nodoka

**Your computer stays responsive while dev servers, builds, installs and AI coding agents run. Any stack. No new commands.**

*Nodoka* (のどか) is Japanese for calm, peaceful weather — what your machine gets to keep while the work runs.

[![ci](https://github.com/sundarshahi/nodoka/actions/workflows/ci.yml/badge.svg)](https://github.com/sundarshahi/nodoka/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

A machine rarely freezes because of one heavy program. It freezes when five builds, three dev servers
and a test run all start at once — which is exactly what coding agents do when they work in several
git worktrees. nodoka is a small background helper (a 500 KB program using about 3 MB of memory)
that keeps that from happening:

- **Fewer workers per tool.** Next.js, Turbopack, Vite, Jest, Vitest, Cargo, Go, pytest, Bundler and
  others are told about half your cores, so one build can't take all of them — and only 2 while memory
  is short. Node and Bun processes also use their leaner memory settings.
- **A few heavy jobs at a time.** Builds and installs beyond the limit wait their turn (at most a
  minute) instead of all fighting for memory. Test runs start right away (a paused test fails its
  timeouts) but count toward the limit.
- **Idle servers sleep.** A dev server nobody has used for 10 minutes is paused; the next request — or
  the next file you save in its project — wakes it in under a second, so hot reload keeps working.
- **Big jobs wait for memory before they start.** nodoka remembers how much memory each build, type
  check or install needed the last time it ran in that folder. While memory is short, the next run
  waits for room (at most a minute) instead of starting, growing and fighting your apps for memory.
  Test runs never wait.
- **A slow lane for low memory.** When memory gets critically low — or the machine keeps swapping to
  disk — busy builds keep running but only on spare CPU and disk time, so your editor and browser get
  their memory back from disk first and stay responsive. The build finishes, which frees its memory.
  Only if the machine is freezing (swapping 100+ MB/s) is the newest build paused until there's room
  for it. `memory_brake = pause` pauses at critical memory instead.
- **Stuck test runs end.** A test run an agent started that has used no CPU for 30 minutes finished
  but never exited (Jest with an open handle), and only holds memory: nodoka asks it to exit. Watch
  modes, headless browsers and runs you started in a terminal are never stopped.
- **Idle worktrees give their disk back.** Coding agents make a git worktree per task and install
  1–2 GB of dependencies into each. List the folders that hold them in `clean_dirs`, and in a worktree
  idle for a day nodoka deletes what an install or build recreates (`node_modules`, `.next`,
  `.turbo`…, only if git ignores it). After a week the worktree itself goes, but only when nothing
  would be lost: no changes, its commit is on a remote, no other ignored files such as `.env`.
- **Agent work yields to you.** While every core is busy, builds, tests, headless browsers and dev
  servers that coding agents started get lower priority; ones you started in a terminal keep theirs.
- **A CPU budget for agents.** Builds, installs and dev servers that agents started share at most half
  your cores. Past that they run in slices of each second, so the other half stays free for you and
  the machine runs cooler. Test runs are never sliced.

You keep typing `npm run dev`, `bin/rails server` and `cargo build` exactly as before. Nothing is
turned off, and the only thing ever stopped is an agent's test run that has sat idle for 30 minutes.

## Install

macOS and Linux (including WSL 2):

```sh
curl -fsSL https://raw.githubusercontent.com/sundarshahi/nodoka/main/install.sh | sh
```

or build it yourself (Rust 1.85+):

```sh
cargo install --git https://github.com/sundarshahi/nodoka
nodoka install
```

Then open a new terminal and restart any coding agents. `nodoka install` adds one marked line to your
shell startup file, starts the helper at login (a LaunchAgent on macOS, a systemd user service on Linux)
and stops VS Code-style editors from watching `node_modules`, `target`, `.venv` and similar folders.
`nodoka uninstall` removes all of it.

## What you'll see

Mostly nothing. When you're curious:

```console
$ nodoka status
memory pressure: warn   load: 7.9 on 10 cores   builds/tests/installs at once: 2
  59101  build    running                 887 MB  next-build (v16.4.5)
  59280  build    waiting for a slot        6 MB  cargo build -q
  59487  build    waiting for memory        6 MB  npm exec tsc --noEmit
  48810  dev      asleep: idle            212 MB  rails server -p 3000
biggest apps (nodoka leaves these alone; closing one frees memory): firefox 7.5 GB, Code 6.1 GB
```

While memory is short, the last line shows which apps hold the most of it.

If something you started in a terminal has to wait, that terminal gets one line saying why:

```text
[nodoka] `next build` is waiting for a free slot. It starts on its own within 60s. See `nodoka status`.
[nodoka] `next build` continues.
```

## Works with

nodoka recognises dev work from the command line, for every common stack:

| Stack | Examples it recognises |
|---|---|
| React | Vite, Next.js (`next dev`/`build`/`start`), React Router and Remix (`react-router dev`, `react-router-serve`), Create React App (`react-scripts`, `craco`), webpack (`webpack serve`, `--watch`), Gatsby, Expo and React Native (`expo start`, Metro, `run-ios`), Storybook, Docusaurus |
| Vue, Angular, Svelte and more | Vite (any framework), Nuxt, Vue CLI, Angular (`ng serve`/`build`/`test`), SvelteKit, Astro, Solid, Qwik, Ember, Electron Forge, electron-vite, Tauri |
| JavaScript / TypeScript tools | `npm`/`pnpm`/`yarn`/`bun install`, `rspack`, `rsbuild`, `parcel`, `esbuild`, `rollup`, `tsc`, `eslint`, `biome`, `jest`, `vitest`, `playwright`, `cypress run`, `turbo`, `nx`, `nodemon`, `tsx watch` |
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
$ nodoka explain bin/rails server
`rails server` is a dev server or watcher. Never queued. If it listens on a port and sits idle, it sleeps; the next request wakes it.
```

Your own tools can be added in the config file (below), and new rules are the easiest way to
[contribute](CONTRIBUTING.md).

### Tested on real servers

Each was started the way a coding agent starts it and left idle until nodoka put it to sleep. Then a
file in the project was saved, and once it slept again, a real HTTP request was sent:

| Server | Saved file woke it in | Request answered in |
|---|---|---|
| Vite 8 + React 19 (`npm run dev`) — the hot update reached the open tab 0.95 s after the save | 0.72 s | 0.57 s |
| Next.js 16.4 `next dev` (Turbopack) | 0.60 s | 0.80 s |
| React Router 8.5 (`react-router dev`) | 0.84 s | 0.54 s |
| webpack 5 + React (`webpack serve`, the Create React App setup) | 0.59 s | 0.57 s |
| Vue 3 on Vite | 0.61 s | 0.53 s |
| Angular 21 (`ng serve`) | 0.60 s | 0.50 s |
| Rails 8.1 (`bin/rails server`, Puma 8) | 0.59 s | 0.62 s |
| Django 6.1 (`manage.py runserver`, with autoreload) | 0.81 s | 0.56 s |
| FastAPI (`uvicorn --reload`, polling reloader) | not tried | 0.60 s |
| Python `http.server` — macOS / Linux | not tried | 0.99 s / 0.63 s |
| Go `net/http` (`go run`) | not tried | 0.65 s |
| Java `HttpServer` (custom rule) | not tried | 0.63 s |

## Platforms

| | macOS | Linux and WSL 2 | Windows |
|---|---|---|---|
| Fewer workers per tool, 2 while memory is short | ✓ | ✓ | planned |
| Leaner memory settings for Node and Bun | ✓ | ✓ (and glibc's `MALLOC_ARENA_MAX=2`) | planned |
| A few heavy jobs at a time | ✓ | ✓ | planned |
| Idle servers sleep, wake on a request or saved file | ✓ (FSEvents) | ✓ (inotify) | planned |
| Low-memory brake | ✓ (kernel memory-pressure level and swap-in rate) | ✓ (pressure-stall information and available memory) | planned |
| Lower priority when every core is busy | ✓ (nice 10, kept until the job ends) | ✓ (nice 10, kept until the job ends) | planned |
| Starts at login | LaunchAgent | systemd user service | |

Tested on macOS 27 (Apple silicon) and Linux 6.10 (arm64). Release builds cover x86_64 and arm64 for
both. Windows support is the most-wanted contribution — see [CONTRIBUTING.md](CONTRIBUTING.md).

## Commands

```text
nodoka status            what is running, waiting or asleep, and why
nodoka explain <cmd…>    how nodoka treats a command (or a running pid)
nodoka off | on          pause nodoka (everything it paused resumes) / turn it back on
nodoka install           set up for every terminal, editor and agent on this machine
nodoka uninstall         remove everything nodoka added
nodoka daemon            run the background helper in the foreground (for debugging)
```

## Configuration

Everything works without configuration. `~/.nodoka/config` (created by `nodoka install`) holds
optional settings; changes apply within a few seconds:

```ini
heavy_jobs = 3            # builds/tests/installs at once; test runs count but never wait (default: 1 per 5 GB of memory, at most half your cores)
cpus = 4                  # worker/thread count each tool is told about (default: half your cores, at least 2; 2 while memory is short, explicit Jest/Vitest counts too)
idle_sleep_minutes = 10   # 0 turns idle sleep off
max_wait_seconds = 60     # a waiting job starts anyway after this long
leftover_busy_minutes = 10  # a busy process left with no parent gets lower priority after this long; 0 turns it off
agent_cpu_percent = 50      # agents' builds, installs and servers share at most this much CPU; 100 turns it off
memory_brake = slow         # at critical memory: slow lane (default) or `pause`
stop_idle_tests_minutes = 30      # an agent's test run idle this long is asked to exit (0: never)
stop_long_tests_minutes = 30      # …or still running this long while memory is critical (0: never)
close_idle_agents_after_hours = 24  # close agent sessions (Claude Code, Codex) idle this long while memory is short (off when unset)
clean_dirs = ~/worktrees    # folders of git worktrees to clean when idle (off when unset)
clean_caches_after_hours = 24     # idle this long: delete node_modules, .next… (0: never)
remove_worktrees_after_days = 7   # idle this long: remove the worktree if nothing is lost (0: never)

# teach nodoka about your own tools (program names, comma-separated)
dev = my-server
build = my-compiler
ignore = some-tool        # never touch these
```

For a single command, `NODOKA_SKIP=1 <command>` leaves it alone. To turn off the worker caps in one
shell, set `NODOKA_OFF=1`.

**Lowest memory.** `heavy_jobs = 1` and `cpus = 2` make builds and installs run one at a time with the
fewest workers. Three agents building at once then peaked at 0.9 GB instead of 3.5–3.8 GB (−75%), and
took 12 s instead of 7–9 s. The more jobs pile up, the bigger the saving.

## Safety

nodoka is designed to be safe to leave running:

- **It kills nothing you're using.** It pauses (`SIGSTOP`), resumes (`SIGCONT`) and lowers priority.
  The exceptions are things an agent started and stopped waiting for, each sent one polite `SIGTERM`:
  a test run idle 30 minutes (`stop_idle_tests_minutes`) or still running after 30 minutes while
  memory is critical (`stop_long_tests_minutes`), and a build held 10 minutes for memory. Its agent
  can run it again. If you set `close_idle_agents_after_hours`,
  coding-agent sessions that did nothing for that long get the same `SIGTERM` while memory is short;
  their chat is saved, so they can be resumed. Off by default.
- **It only touches your own processes,** needs no root and makes no network connections.
- **It leaves alone anything it doesn't recognise,** including databases, editors and GUI apps. The one
  exception is a leftover: a process whose parent is gone (a killed agent, an interrupted test run) that
  keeps a core busy for 10 minutes. It gets lower priority, never a pause, and is listed in
  `nodoka status` with the command that stops it. Apps and system services are never counted.
- **Test runs are never paused, and installs never for memory.** Clocks keep running while a process
  is paused: a held test run fails its per-test timeouts, and a held download times out.
- **It never pauses the process an interactive shell is waiting on,** so your terminal never prints
  "suspended" or loses its prompt.
- **It is crash-safe.** Before pausing anything it writes down what it is about to pause. If the helper
  is turned off or uninstalled, everything it paused is resumed. A restart (an update, a crash) resumes
  everything too, except builds paused for low memory: the new helper takes those over.
- **Every change is reversible.** `nodoka off` resumes everything at once; `nodoka uninstall` removes
  each line it added, exactly.

## Measured

On a 10-core, 16 GB MacBook, two runs of each:

- **One real `next build`** (Next.js 16.4.5, 41 pages): peak memory 1.56 GB → 1.03–1.11 GB (−30%, 9 → 4
  workers, same time). While memory is short, tools get 2 workers: 0.57–0.61 GB (−62%).
- **Three agents start `next build` at the same moment:** machine-wide peak 3.5–3.8 GB without nodoka.
  With the defaults 2.0–3.3 GB, depending on how short memory already was, and done sooner (6 s vs
  7–9 s: fewer workers fight for the cores). With the lowest-memory settings, 0.9 GB (−75%) in 12 s.
  All builds exited 0.
- **Builds and installs take turns:** `next build` and two `cargo build`s with one slot ran one after
  another in arrival order; all three exited 0.
- **Test runs are never held:** a real Vitest suite (60 tests reading files and waiting on timers)
  started while a build held the only slot passed 3 times out of 3. While test runs could still be held,
  one run in three failed 9 tests on their 5 s timeouts.
- **Lower priority is plain `nice`:** macOS's background priority made three parallel builds take 40 s
  instead of 8 s, because it confines work to the efficiency cores.
- **The helper:** a 470 KB program (macOS; 600 KB on Linux) using about 3 MB of memory and under 1% of
  one core while watching a busy machine.
- **The Node preload:** adds about 1 ms to each Node start (33.7 ms vs 32.6 ms for `node -e 0`).

## Limits

- **Docker:** containers run inside Docker's own virtual machine, where nodoka can't see them. Pair it
  with a lightweight runtime such as [Kiri](https://runkiri.com/), [OrbStack](https://orbstack.dev/) or
  [Colima](https://github.com/abiosoft/colima).
- **A single process typed straight at your prompt** (`pytest`, `bin/rails server`) is never paused,
  because the shell would notice. Commands that start helpers (`npm run build`, `cargo build`, `go test`)
  are paused through their helper processes.
- **zsh's `cmd & wait $!`** reports exit status 145 if `cmd` is paused while zsh waits on it (bash and sh
  don't). Plain foreground commands — what coding agents run — are unaffected.
- **webpack renames itself to a bare `webpack`**, wiping its arguments. Started from a package script
  (`npm start`, `yarn dev`) nodoka reads them back; started straight from a shell, `webpack serve` looks
  like a build and never sleeps. Add `dev = webpack` to the config if you start it that way.
- **Lowered priority stays** until the job ends: neither system lets an ordinary user raise it again. It
  only matters while every core is busy.
- **Memory can't go much below a build's working set.** Every remaining knob is a hard cap that crashes
  the build or a feature switched off. About −60% is the ceiling for one build; the big savings come from
  jobs taking turns. Projects whose scripts set their own `NODE_OPTIONS` replace nodoka's Node settings
  for those commands.
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
nodoka uninstall
```

Your settings stay in `~/.nodoka/config` in case you come back; delete the folder to remove them too.

## Contributing

Bug reports, new tool rules and platform ports are very welcome. Start with
[CONTRIBUTING.md](CONTRIBUTING.md). Please follow the [code of conduct](CODE_OF_CONDUCT.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at
your option. Unless you explicitly state otherwise, any contribution you submit for inclusion in this
project shall be dual licensed as above, without any additional terms or conditions.
