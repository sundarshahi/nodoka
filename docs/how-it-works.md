# How nodoka works

This is the design document: what nodoka does, why each decision was made, and the evidence behind
it. Read it before changing behaviour.

## The problem

A development machine hangs when too much heavy work runs at once, not usually because of one program.
Coding agents make this common: they start installs, builds, type checks and dev servers in several git
worktrees in parallel. Each tool sizes its worker pool from the core count, so five builds on a 10-core
laptop can mean 45 worker processes. Next.js alone reports `next dev` processes of 7–30 GB in
[vercel/next.js#91396](https://github.com/vercel/next.js/issues/91396).

nodoka's promise: **the machine stays responsive; work may run slower or wait its turn; nothing is
killed and no feature is lost; nobody has to learn a new command.**

## Approaches considered

1. **A layer around the real tools (chosen).** The real Node, Ruby, Python, Go… run unchanged
   underneath; nodoka only adjusts worker counts and decides when processes run.
2. **Replace the runtime** (for example Bun for Node). Lighter per process, but parts of Next.js and
   native add-ons break, and it only helps one ecosystem.
3. **Run everything inside a small VM**, the way [Kiri](https://github.com/libracoder/kiri) and
   [OrbStack](https://docs.orbstack.dev/architecture) make Docker light. This gives hard memory limits,
   but brings back VM overhead and slow file sharing on macOS (hot reload suffers).

From Kiri and OrbStack we kept the parts that make them feel invisible: a tiny always-on helper,
idle sleep with sub-second wake, and integration that is opt-in and reversible.

## Part 1: fewer workers per tool

Most tools pick their worker count from `os.cpus()`, `os.availableParallelism()` or the CPU count.
nodoka lowers what they see, to half the cores (at least 2, because many tools compute `cpus - 1`).

- **Node and Bun:** [`preload/preload.cjs`](../preload/preload.cjs) is loaded into every Node process
  through `NODE_OPTIONS=--require=…` (Bun: `BUN_OPTIONS=--preload=…`). It replaces both
  `os.cpus()` and `os.availableParallelism()` — Next.js 16.4 reads the first, Next.js canary the second
  ([config-shared.ts](https://github.com/vercel/next.js/blob/b7679898640022b18e4780f54b7e8e73e5c9987c/packages/next/src/server/config-shared.ts#L2518-L2521)).
  It runs in worker threads, forked children and ES modules too
  ([Node docs](https://github.com/nodejs/node/blob/108cbef0df492e8fb790942f608e8ca802564af3/doc/api/cli.md#L2980-L3001)),
  and Next keeps `--require` when it rebuilds `NODE_OPTIONS` for its workers. Measured cost: 0.7 ms per
  Node start. It is skipped inside Electron apps, which reject `NODE_OPTIONS`.
- **Native tools** read environment variables, set by `~/.nodoka/env.sh` only when unset (each one can
  only lower a default, never raise it):

| Variable | Affects |
|---|---|
| `TURBO_TASKS_AVAILABLE_PARALLELISM` | Turbopack's thread pool ([source](https://github.com/vercel/next.js/blob/b7679898640022b18e4780f54b7e8e73e5c9987c/turbopack/crates/turbo-tasks/src/parallel.rs#L23-L42)); `TOKIO_WORKER_THREADS` is ignored there |
| `ROLLDOWN_WORKER_THREADS`, `TOKIO_WORKER_THREADS`, `RAYON_NUM_THREADS` | Rolldown, Rspack, uv and other Rust tools |
| `GOMAXPROCS` | Go: `go build`, esbuild, Hugo |
| `CARGO_BUILD_JOBS` | Cargo |
| `PNPM_MAX_WORKERS` | pnpm's install workers |
| `PYTHON_CPU_COUNT` | Python 3.13+: `os.cpu_count()` (multiprocessing pools, pytest-xdist) |
| `PYTEST_XDIST_AUTO_NUM_WORKERS` | `pytest -n auto` on older Pythons |
| `UV_CONCURRENT_BUILDS` | uv source builds |
| `BUNDLE_JOBS`, `PARALLEL_WORKERS` | Bundler installs, Rails parallel tests |
| `DOTNET_PROCESSOR_COUNT` | .NET: MSBuild and test parallelism |

Deliberately **not** set: `MAKEFLAGS` and `CMAKE_BUILD_PARALLEL_LEVEL` (they would *raise* parallelism
for builds that run serially by default), `JAVA_TOOL_OPTIONS` (the JVM prints a notice on every start,
which breaks tools that parse `java -version`), `ERL_FLAGS` (a mistake would stop every Erlang VM from
starting), and V8 heap limits (`--max-old-space-size` only moves the crash: Next.js drops it for build
workers ([#95745](https://github.com/vercel/next.js/issues/95745)) and too low a value causes
`heap out of memory` in `tsc`).

Measured on a real `next build` (Next.js 16.4.5): 9 → 4 workers, 179 → 77 threads, 30% lower peak
memory, same build time.

## Part 2: the background helper

[`src/daemon.rs`](../src/daemon.rs) runs once a second.

### Finding dev work

It lists the user's own processes, reads each new one's command line once
([`classify.rs`](../src/classify.rs)) and sorts it into **install**, **build**, **test** or **dev**
(servers and watchers). Everything else is left alone. A **job** is a recognised process with no
recognised ancestor, plus all of its descendants, so `npm run dev` → `next dev` → workers is one job.

Things that make recognition hard, and how they are handled:

- **Interpreters and wrappers:** `node …/next build`, `ruby bin/rails server`, `python -m pytest`,
  `bundle exec`, `npx`, `uv run`, `java … org.codehaus.plexus.classworlds.launcher.Launcher` (Maven),
  `beam.smp … -extra mix phx.server`. The resolver looks through them to the tool doing the work.
- **Processes that rename themselves:** Puma becomes `puma 8.0.3 (tcp://…) [app]`, Next.js becomes
  `next-build (v16.4.5)`, npm becomes `npm ci`, gunicorn becomes `gunicorn: master [app]`. Renamed
  forms have their own rules. A process that isn't recognised is looked at again during its first 10
  seconds, because renaming happens at startup.
- **Renaming to a bare name wipes the arguments.** webpack-cli sets its title to just `webpack`, so
  `webpack serve` (a dev server) would look like `webpack` (a build). Node writes the title over the
  argument strings and fills the rest with zero bytes; the environment after them survives. When a
  package script started the tool, npm, yarn, pnpm and bun leave the command in `npm_lifecycle_script`,
  and nodoka takes the tool's arguments from there. (On macOS the zero fill also hid the environment
  until nodoka learned to skip it, so `NODOKA_SKIP` and `NODE_ENV` are read from renamed tools too.)
- **A shell that becomes the tool:** `zsh -c 'npm ci && next build'` execs into `next build` at the end,
  keeping the shell's pid and start time. The cache key includes the program name, which changes on exec,
  so the process is looked at again; and a job's age counts from when nodoka first saw it, not from
  process start.
- **Gradle, Kotlin and Bazel** do their work in a long-lived server process, not in the command you
  typed. nodoka governs that server.

### Deciding

[`src/policy.rs`](../src/policy.rs) is pure — no system calls — so every rule has a unit test.

- **Admission.** At most `heavy_jobs` builds, tests and installs run at once (default: one per 5 GB of
  memory, at most half the cores); at memory-pressure *warn* the limit is halved, rounding up. A build or
  install that starts when the slots are full is paused within its first second and resumed first-in,
  first-out. Test runs never wait (see below) but take a slot.
- **Waiting for memory.** When a build, type check or install ends, nodoka remembers its peak memory
  under its folder and command line (in memory only; a restart forgets). While pressure is *warn* or
  worse, the next run of that command in that folder is held at start if it needed more than the
  system can hand out now, and starts once there's room, oldest first, each start promising its
  amount so two can't take the same room. Held at start it holds next to nothing; paused later by
  the brake it would keep everything it had. It waits at most 10 minutes, like a job paused for
  memory (with `memory_brake = pause`; otherwise at most `max_wait_seconds`). At normal pressure the
  record is ignored: macOS and Linux make room by compressing. No job waits longer than `max_wait_seconds` (60): coding agents have command timeouts, and a
  job that times out gets retried, which adds load. Jobs already running when the helper starts are
  left as they are.
- **Low-memory slow lane** (the default). Only at *critical* pressure: on a busy 16 GB Mac, *warn* can
  be the normal state for hours. Every 5 seconds the newest busy build that isn't in it yet moves to the
  slow lane: on macOS background priority (efficiency cores and throttled disk, so other apps'
  swap-ins go first; a scheduler priority of 4 instead of 31), on Linux the idle disk-priority class.
  New processes in the job join it. Nothing is paused: a paused job keeps every byte it holds and its
  agent waits on it (two paused type checks once held 7 GB for 3 hours), while a slowed one finishes
  and gives its memory back. After 20 calm seconds builds leave the lane one by one. Builds only:
  tests would run into their timeouts and installs into their download timeouts (background priority
  throttles the network too). Jobs waiting for memory before they start wait at most
  `max_wait_seconds` at *warn*. While memory is *critical*, or was under 20 seconds ago (it flips to
  *warn* for a second or two), no new build or install starts for up to 10 minutes, known size or
  not, and while the machine is freezing none starts at all — a third 3.9 GB type check was about to
  start after a minute while a 16 GB Mac swapped 240 MB/s. Waiting before the start holds next to
  nothing. What each command needed in each folder is kept in `~/.nodoka/peaks`, so a restart
  doesn't make a known 4 GB type check look new.
  While the machine is *freezing* (macOS: swap-ins of 100 MB/s or more, smoothed over ~10 seconds;
  Linux: PSI memory `full avg10` of 40% or more) the newest busy build is paused, slowed or not: a
  slowed build still touches all its memory, and a 4 GB type check kept a 16 GB Mac swapping
  205 MB/s. Paused builds follow the brake's rules below (room to resume, a turn after 10 minutes),
  except that an agent's build held 10 minutes — paused, or waiting to start while memory stays
  critical — is asked to exit (SIGTERM) instead of getting its turn, one every 5 seconds. Its agent
  stopped waiting long ago (agents give up on a command after about 10 minutes) and can run it again;
  paused, it keeps all its memory (three type checks held 12.6 GB), and given its turn a 3.8 GB type
  check ran through a 240 MB/s freeze. Builds started in a terminal still get their turn when the
  machine isn't freezing: someone is watching those.
  Builds paused for memory stay paused across a restart of the helper (an update resumed three type
  checks at once): the ledger marks them, and the next start takes them over with their pause time.
  `nodoka off`, uninstalling and a test helper whose test run died still resume everything.
  A build resumed or started in the last minute counts as already holding its memory when the next
  resume is weighed: a resumed job pulls its memory back in over seconds, and three 4 GB type checks
  were resumed 5 seconds apart, each seeing the room the one before was about to take.
- **Stuck test runs** (`stop_idle_tests_minutes`, 30). A test run started without a terminal that has
  used no CPU (under ~2% of a core) for that long has finished but never exited — Jest with an open
  handle — and only holds memory: it gets one SIGTERM. Watch modes count as dev servers, and headless
  browsers (anything with `headless` on its command line) wait on their agent, so neither is stopped;
  nor is a run started in a terminal. Four such Jest runs once held 2.9 GB for up to 78 minutes.
  One still running after `stop_long_tests_minutes` (30) while memory is critical gets the same
  SIGTERM, busy or not: an agent gives up on a command after about 10 minutes, and a Jest run kept
  4 workers busy for 43 minutes on a Mac swapping 38 MB/s.
- **Heavy work no rule knows.** Rules name the usual builds, tests and installs; an agent can run
  anything else (a Java map-tile build took 2 cores and 4 GB for 20 minutes while nodoka paused a
  type check next to it). A program started by a coding agent that no rule matches becomes a build
  once it keeps more than 1.5 cores busy, or a whole core while holding 1 GB, for 30 seconds. From
  then on it is slowed, paused and resumed like one, but never asked to exit: it has run a while and
  may run long, so when it has waited 10 minutes it gets its turn instead. It counts as already
  started, so it is never queued. Programs started in a terminal, and agent sessions themselves,
  are left alone.
- **Idle agent sessions** (`close_idle_agents_after_hours`, off). Editors keep coding-agent sessions
  (`claude`, `codex`) open for days: each holds 200–300 MB plus its MCP helpers, and 32 sessions idle
  for over a day once held 8 GB of a 16 GB Mac. A session is working when it or anything it started
  used more than 2% of a core over a minute, or it started a program of its own (a tool call); an idle
  session's own timers use under 1%. Only its direct children count as new programs: a wait loop an
  agent left running (`until …; do sleep 10; done`) starts a new `sleep` every few seconds, and one that
  could never finish kept a session idle for 36 hours looking busy. Sessions idle 24 hours (or the set
  limit) are listed in `nodoka status`. With the setting on, one idle that long gets one SIGTERM while memory is at *warn* or worse; it exits
  within a second and takes its helpers with it, and its chat is already saved. Sessions in a terminal
  are never closed. A Claude Code session's idle time starts at its last chat write (its
  `sessions/<pid>.json` names the chat file), so sessions open before nodoka started are judged
  right away; for others it counts from when nodoka first saw them.
- **Apps notice** (`notify_after_minutes`, 2). With no build, test or install running and memory
  critical for 2 minutes, nodoka has nothing to hold back: the apps hold the memory (a 16 GB Mac
  lagged with VS Code at 13 GB and Firefox at 7 GB while no dev job ran). nodoka never touches apps,
  so it posts a notification naming the three biggest (`osascript` on macOS, `notify-send` on Linux),
  at most once an hour. The notice is also written to the log.
- **Low-memory brake** (`memory_brake = pause`). One job is paused every 5 seconds, newest first,
  busy builds only — a paused test fails its timeouts, a paused install would have its downloads time
  out, and pausing an idle job (a `webpack --watch` between rebuilds) stops nothing from growing. The
  last busy build is paused too: a lone 9 GB `next build` froze a 16 GB Mac. Paused jobs are resumed one
  by one, oldest first, after 20 calm seconds *and* once the system can hand out as much memory as the
  job holds (free, inactive and purgeable pages on macOS, `MemAvailable` on Linux). Calm alone isn't
  enough: pausing the build is what calmed memory, and a lone 5 GB `tsc` resumed on calm went critical
  again 3 seconds later, flapping every 25 seconds. A job paused for 10 minutes gets its turn, even
  while memory stays critical: it runs to the end at low priority and isn't paused again, one such
  job at a time. A paused job keeps its memory, and its agent waits on it; before this, two type
  checks held 7 GB for 3 hours.
- **Disk cleaning** (`clean_dirs`, off when unset). A separate thread, hourly, so the 1-second loop
  never waits on it. For each linked git worktree (a `.git` *file*; main checkouts are never touched)
  in those folders: idle means no git activity (the worktree's `index`, `HEAD` and `logs/HEAD` in its
  git folder) for `clean_caches_after_hours` (24) and no process of yours working in it or naming it on
  its command line. Then the ignored entries `git status --ignored` lists whose name is a rebuildable
  folder (`node_modules`, `.next`, `.turbo`, `.nuxt`, `.svelte-kit`, `.parcel-cache`, `.vite`) are
  deleted. After `remove_worktrees_after_days` (7) the worktree is removed with plain
  `git worktree remove` when there are no changes, `HEAD` is contained in a remote-tracking branch, and
  no other ignored file is left (a `.env`); otherwise it stays, and a dry run says why. The branch is
  kept. On a 16 GB Mac with 125 agent worktrees, `node_modules` alone took 20 GB.
- **Idle sleep.** A job whose processes listen on a TCP port is a server. It counts as busy while it uses
  more than about 2% of a core or a visitor connects. After `idle_sleep_minutes` (10) without either, it
  is paused. A server that keeps the CPU busy with nobody visiting (a polling file watcher, a runaway
  loop) sleeps after three times that. Watchers without a port (`tsc --watch`) never sleep, because
  nothing could wake them. Servers started with `NODE_ENV=production` (and the Rails, Rack, Mix and
  ASP.NET equivalents) never sleep.
- **Busy CPU.** When the 1-minute load is above the core count, heavy jobs move to low priority, and back
  when the load drops. Jobs started from a terminal are left at normal priority: a person is watching.

### Waking a sleeping server

A paused process can't accept connections, but the kernel still completes TCP handshakes into the
listening socket's accept queue. Every second, nodoka reads the TCP sockets of sleeping servers:

- **macOS:** `proc_pidinfo(PROC_PIDLISTFDS)` + `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)`; for a listening
  socket, `soi_qlen + soi_incqlen` is the queue length.
- **Linux:** `/proc/net/tcp` and `/proc/net/tcp6`; for a listening socket `rx_queue` is the accept-queue
  length ([tcp_ipv4.c](https://github.com/torvalds/linux/blob/master/net/ipv4/tcp_ipv4.c)), matched to the
  process through the socket inodes in `/proc/<pid>/fd`.

A finding from testing on macOS: servers that wait in a blocking `accept()` (Tomcat- and Jetty-style
acceptor threads) take the connection off the queue even while paused, so the queue reads 0. The request
bytes are then waiting on a new connected socket instead. So a server is woken when a listening socket has
a queue, **or** a connection has unread data that wasn't there when it went to sleep. Measured wake: 0.57–0.99 s.

**Saving a file wakes it too.** Found by testing a real Vite + React server: a request wakes a sleeping
server, but saving a file doesn't send one. The server's own file watcher is paused, so hot reload never
reached the open tab (nothing after 45 s), and a Django or Rails server woken later by a request could
answer once with old code before its reloader caught up. So while a server sleeps, nodoka watches its
project — the git checkout around its working folder, so edits in a sibling package count, or the working
folder itself; never the home folder — and wakes it on the first change. Changes inside dependency,
build-output, cache and log folders (`node_modules`, `.git`, `target`, `dist`, `.next`, `tmp`, `log`…)
don't count, so another job's build can't keep waking it. The watch exists only while the server sleeps:

- **macOS:** one FSEvents stream per sleeping server (recursive, kernel-fed, no polling).
- **Linux:** inotify, one watch per folder — the skipped folders are also never walked, so a
  `node_modules` with thousands of folders costs nothing; at most 4096 folders per server.

Measured on real servers: the hot update reached a Vite + React tab 0.95 s after the save; Vue, React
Router, webpack, Angular, Next.js, Rails and Django woke 0.59–0.84 s after a save.

### Acting

- **Pause and resume:** `SIGSTOP`/`SIGCONT` to every process in the job. Children forked while a job
  is paused are paused on the next tick. On macOS there is no way for an ordinary user to cap a
  process's memory (`RLIMIT_RSS` is not enforced), so pausing is the only tool.
- **Never the process a shell waits on.** An interactive shell (one with a terminal) reports a stopped
  child as "suspended" and takes the prompt back, so nodoka never stops a direct child of an
  interactive shell. It pauses that child's own children instead (for `npm run build`, the build process
  itself). If nothing else is left, the job is not paused at all. Measured: non-interactive zsh, bash and
  sh running a command in the foreground — what coding agents do — don't notice a pause at all (exit
  status 0, correct timing). The one exception found is zsh's `cmd & wait $!`, which returns 145.
- **Terminal notices.** When a job started from a terminal has to wait or is paused for memory, one line
  is written to that terminal, and another when it continues.
- **Lower priority:** for every job started without a terminal (what coding agents run), dev servers
  and headless browsers included: an agent's demo recorder held a Rails server at 100% of a core.
  nice 10, on both systems. It yields to your apps only while cores are contested
  and costs nothing otherwise; an ordinary user can't raise it back, so the job keeps it until it ends.
  macOS's background priority (`PRIO_DARWIN_BG`) was used first because it can be undone, but it confines
  work to the efficiency cores: three `next build`s run at once took 40–42 s under it, against 8–9 s at
  normal priority and 7 s at nice 10.
- **CPU budget for agent work.** Builds, installs and dev servers started without a terminal share
  `agent_cpu_percent` (50) of all cores. Each second nodoka measures what they used; over budget,
  they're all stopped for part of the next second (`SIGSTOP`, then `SIGCONT`), the way
  [cpulimit](https://github.com/opsengine/cpulimit) works, so together they get the budget. Each still
  runs at least a quarter of every second: slower, never hung. Test runs (including headless
  browsers) are exempt, because a stopped test still loses time against its timeouts, and so is
  anything started in a terminal. Slices are in the crash ledger before the first one starts.
- **Leftovers.** A process whose parent died is handed to launchd (macOS) or init or the user's
  `systemd` (Linux). One that keeps at least a quarter of a core busy for `leftover_busy_minutes` (10)
  gets nice 10 once, a log line and a line in `nodoka status` with `kill <pid>`. Apps, app extensions
  and system services (`/System`, `/usr/libexec`, `/usr/sbin`, `/Library`, anything inside an `.app`)
  run parentless by design and are skipped. Found the hard way: two busy children of a killed test job
  ran at full speed for 36 hours. Lowering priority keeps the machine responsive but doesn't stop the
  heat, so stopping it is left to you: the only process nodoka ever asks to exit is a stuck test run.
- **Crash safety.** Before any signal, the processes about to be paused or lowered are written to
  `~/.nodoka/paused`. On start, stop, `nodoka off` and `nodoka uninstall`, everything in that file
  is resumed. Processes are identified by pid *and* start time, so a reused pid is never touched.

### Memory pressure

- **macOS:** `sysctl kern.memorystatus_vm_pressure_level` (1 normal, 2 warn, 4 critical), the same level
  the system uses ([xnu](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_memorystatus_notify.c)).
  That level alone isn't enough: a 16 GB Mac lagging with 12 GB in swap, swapping 150 MB/s in and
  150 MB/s out, still reported *warn*. So the swap-in rate (`host_statistics64`, smoothed over about
  10 s) raises it: 8 MB/s or more is warn, 32 MB/s or more is critical.
- **Linux:** critical when less than 8% of memory is available or `full avg10` in
  `/proc/pressure/memory` reaches 10%; warn under 20% available or `some avg10` at 10%. This acts before
  [earlyoom](https://github.com/rfjakob/earlyoom) (kills at 10% available and 10% free swap) and
  [systemd-oomd](https://www.freedesktop.org/software/systemd/man/latest/oomd.conf.html) (kills at 60%
  pressure for 30 s) — and pauses rather than kills.
- **Tools adapt to it.** The helper writes the current level to `~/.nodoka/pressure` every few seconds.
  A Node tool that starts while the level is *warn* or worse is told about 2 CPUs instead of half the
  cores, so it starts the fewest workers: a real `next build` peaked at 0.57–0.69 GB instead of
  1.0–1.1 GB. Jest and Vitest given an explicit worker count (`--maxWorkers=4`, `-w 4`) get at most 2
  then too; an agent's `jest --maxWorkers=4` held 1.6 GB while a Mac froze. Percentages already follow
  the core count, and other tools' flags are left alone. The preload ignores the file once it is 30 s
  old, so a stopped helper changes nothing.
- **Why only builds are paused, never tests.** Clocks keep running while a process is paused. A test run
  paused mid-test fails its per-test timeouts on resume (5 s in [Jest](https://jestjs.io/docs/configuration#testtimeout-number)
  and [Vitest](https://vitest.dev/config/testtimeout)). Builds survive it: Next.js retries a static page
  that took too long ([export/worker.ts](https://github.com/vercel/next.js/blob/v16.4.5/packages/next/src/export/worker.ts#L489-L531)).
  Holding test runs only at start isn't safe either: the helper looks once a second, and a real Vitest
  suite (60 tests, 1.8 s) held behind a build failed 9 tests in one run out of three. So test runs never
  wait; they still take a slot, so builds and installs wait for them. Measured after the change: 3 out of
  3 runs passed.

### Less memory per process

Set in the shell startup file, only when you haven't set them yourself:

- **`--max-semi-space-size=16` in `NODE_OPTIONS`.** Node 24 made V8's young generation four times
  bigger: 64 MB semi-spaces instead of 16 ([Node 22](https://github.com/nodejs/node/blob/v22.21.1/deps/v8/src/flags/flag-definitions.h#L808),
  [Node 24](https://github.com/nodejs/node/blob/v24.13.0/deps/v8/src/flags/flag-definitions.h#L913-L928)),
  so each busy Node process can hold 96 MB more. 16 is Node 22's default: on an allocation-heavy test it
  cut peak memory 35% at the same speed. It is one of the few V8 flags Node accepts in `NODE_OPTIONS`
  ([allowlist](https://github.com/nodejs/node/blob/v24.13.0/doc/api/cli.md#L3567-L3592)); Next.js passes it on
  to its workers. Machines under 8 GB get 8 so it never raises V8's own default. The heap limit is not
  touched: a hard cap (`--max-old-space-size`) would crash big builds with "JavaScript heap out of memory".
- **`--smol` in `BUN_OPTIONS`.** Bun's documented low-memory mode: 27% less on the same test.
- **`MALLOC_ARENA_MAX=2` on Linux.** glibc gives threads up to 8 malloc arenas per core; 2 is the usual
  fix for Ruby, Python and other many-threaded programs ([glibc](https://sourceware.org/glibc/manual/latest/html_node/Memory-Allocation-Tunables.html)).

What isn't possible: going much below a build's working set. Every remaining knob is a hard cap that
crashes the build or a feature switched off. For one `next build`, nodoka's ceiling is about −60%;
across the machine, when several jobs pile up, turn-taking is what takes the peak down 70–90%.

## Part 3: editors

VS Code's default `files.watcherExclude` no longer excludes `node_modules`
([files.contribution.ts](https://github.com/microsoft/vscode/blob/main/src/vs/workbench/contrib/files/browser/files.contribution.ts)),
so an `npm ci` inside an open folder floods the file watcher. Agent worktrees inside the project make it
worse: one project reported a 2.7 GB dev-server heap from watching `.claude/worktrees`, down to 819 MB
after excluding it ([datagouv/cdata#1305](https://github.com/datagouv/cdata/pull/1305)).
`nodoka install` adds a marked `files.watcherExclude` entry to VS Code, Cursor, Windsurf and VSCodium
settings when they don't have one; `nodoka uninstall` removes exactly that entry.

## Prior art

- [Kiri](https://runkiri.com/) and [OrbStack](https://orbstack.dev/): lightweight Docker runtimes; the
  model for "tiny, invisible, reversible".
- [puma-dev](https://github.com/puma/puma-dev) and [portless](https://github.com/vercel-labs/portless):
  start dev servers on demand behind a proxy. nodoka needs no proxy: the kernel queues connections
  for a paused server.
- [earlyoom](https://github.com/rfjakob/earlyoom), [systemd-oomd](https://www.freedesktop.org/software/systemd/man/latest/systemd-oomd.service.html),
  [nohang](https://github.com/hakavlad/nohang): kill processes under memory pressure. nodoka pauses
  instead, and only dev work.
- [cpulimit](https://github.com/opsengine/cpulimit): limits CPU with `SIGSTOP`/`SIGCONT` cycles.
- [Volta](https://github.com/volta-cli/volta) and [mise](https://mise.jdx.dev/): transparent tool shims;
  nodoka uses a Node preload and environment variables instead, so it works whichever version
  manager installed Node.

## Platform notes

| | macOS | Linux |
|---|---|---|
| Processes | `proc_listallpids`, `proc_pidinfo(PROC_PIDTBSDINFO)`, `sysctl KERN_PROCARGS2` | `/proc/<pid>/stat`, `cmdline`, `environ` |
| Memory per process | physical footprint (`proc_pid_rusage`), Activity Monitor's "Memory" | resident minus shared pages (`/proc/<pid>/statm`) |
| CPU time | `proc_pid_rusage`, scaled by `mach_timebase_info` on Apple silicon | `/proc/<pid>/stat`, clock ticks |
| Start at login | LaunchAgent `nodoka` | systemd user service `nodoka` |

On Windows: processes from a Toolhelp snapshot (the user's own logon session), command line through
`ProcessCommandLineInformation`, environment and working folder from the process's parameters block,
CPU and private bytes from its own counters; pressure from free memory, the commit limit and the
page-in rate (`\\Memory\\Pages Input/sec`); pausing with `NtSuspendProcess` and a full per-thread
resume (Windows counts suspensions); priority classes for the background and slow lanes. A program
counts as yours unless a coding agent started it (Windows has no controlling terminal). Idle servers
don't sleep: Windows has no API for another process's accept queue or unread socket data, so waking
one would need a small port proxy.

