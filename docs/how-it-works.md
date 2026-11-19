# How loadline works

This is the design document: what loadline does, why each decision was made, and the evidence behind
it. Read it before changing behaviour.

## The problem

A development machine hangs when too much heavy work runs at once, not usually because of one program.
Coding agents make this common: they start installs, builds, type checks and dev servers in several git
worktrees in parallel. Each tool sizes its worker pool from the core count, so five builds on a 10-core
laptop can mean 45 worker processes. Next.js alone reports `next dev` processes of 7–30 GB in
[vercel/next.js#91396](https://github.com/vercel/next.js/issues/91396).

loadline's promise: **the machine stays responsive; work may run slower or wait its turn; nothing is
killed and no feature is lost; nobody has to learn a new command.**

## Approaches considered

1. **A layer around the real tools (chosen).** The real Node, Ruby, Python, Go… run unchanged
   underneath; loadline only adjusts worker counts and decides when processes run.
2. **Replace the runtime** (for example Bun for Node). Lighter per process, but parts of Next.js and
   native add-ons break, and it only helps one ecosystem.
3. **Run everything inside a small VM**, the way [Kiri](https://github.com/libracoder/kiri) and
   [OrbStack](https://docs.orbstack.dev/architecture) make Docker light. This gives hard memory limits,
   but brings back VM overhead and slow file sharing on macOS (hot reload suffers).

From Kiri and OrbStack we kept the parts that make them feel invisible: a tiny always-on helper,
idle sleep with sub-second wake, and integration that is opt-in and reversible.

## Part 1: fewer workers per tool

Most tools pick their worker count from `os.cpus()`, `os.availableParallelism()` or the CPU count.
loadline lowers what they see, to half the cores (at least 2, because many tools compute `cpus - 1`).

- **Node and Bun:** [`preload/preload.cjs`](../preload/preload.cjs) is loaded into every Node process
  through `NODE_OPTIONS=--require=…` (Bun: `BUN_OPTIONS=--preload=…`). It replaces both
  `os.cpus()` and `os.availableParallelism()` — Next.js 16.4 reads the first, Next.js canary the second
  ([config-shared.ts](https://github.com/vercel/next.js/blob/b7679898640022b18e4780f54b7e8e73e5c9987c/packages/next/src/server/config-shared.ts#L2518-L2521)).
  It runs in worker threads, forked children and ES modules too
  ([Node docs](https://github.com/nodejs/node/blob/108cbef0df492e8fb790942f608e8ca802564af3/doc/api/cli.md#L2980-L3001)),
  and Next keeps `--require` when it rebuilds `NODE_OPTIONS` for its workers. Measured cost: 0.7 ms per
  Node start. It is skipped inside Electron apps, which reject `NODE_OPTIONS`.
- **Native tools** read environment variables, set by `~/.loadline/env.sh` only when unset (each one can
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
- **Renaming wipes the environment view.** On macOS and Linux, Node's process title is written over the
  memory where the environment strings were, so `LOADLINE_SKIP` and `NODE_ENV` can't be read from a
  renamed process. loadline falls back to the nearest parent's environment.
- **A shell that becomes the tool:** `zsh -c 'npm ci && next build'` execs into `next build` at the end,
  keeping the shell's pid and start time. The cache key includes the program name, which changes on exec,
  so the process is looked at again; and a job's age counts from when loadline first saw it, not from
  process start.
- **Gradle, Kotlin and Bazel** do their work in a long-lived server process, not in the command you
  typed. loadline governs that server.

### Deciding

[`src/policy.rs`](../src/policy.rs) is pure — no system calls — so every rule has a unit test.

- **Admission.** At most `heavy_jobs` builds, tests and installs run at once (default: one per 5 GB of
  memory, at most half the cores); at memory-pressure *warn* the limit is halved, rounding up. A heavy
  job that starts when the slots are full is paused within its first seconds and resumed first-in,
  first-out. No job waits longer than `max_wait_seconds` (60): coding agents have command timeouts, and a
  job that times out gets retried, which adds load. Jobs already running when the helper starts are
  left as they are.
- **Low-memory brake.** Only at *critical* pressure: on a busy 16 GB Mac, *warn* can be the normal state
  for hours, and acting on it would make jobs stutter. One job is paused every 5 seconds, newest first,
  builds and tests only — a paused install would have its downloads time out. Paused jobs are resumed
  one by one after 20 calm seconds, and never stay paused longer than 10 minutes.
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
listening socket's accept queue. Every second, loadline reads the TCP sockets of sleeping servers:

- **macOS:** `proc_pidinfo(PROC_PIDLISTFDS)` + `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)`; for a listening
  socket, `soi_qlen + soi_incqlen` is the queue length.
- **Linux:** `/proc/net/tcp` and `/proc/net/tcp6`; for a listening socket `rx_queue` is the accept-queue
  length ([tcp_ipv4.c](https://github.com/torvalds/linux/blob/master/net/ipv4/tcp_ipv4.c)), matched to the
  process through the socket inodes in `/proc/<pid>/fd`.

A finding from testing on macOS: servers that wait in a blocking `accept()` (Tomcat- and Jetty-style
acceptor threads) take the connection off the queue even while paused, so the queue reads 0. The request
bytes are then waiting on a new connected socket instead. So a server is woken when a listening socket has
a queue, **or** a connection has unread data that wasn't there when it went to sleep. Measured wake: 0.57–0.99 s.

### Acting

- **Pause and resume:** `SIGSTOP`/`SIGCONT` to every process in the job. Children forked while a job
  is paused are paused on the next tick. On macOS there is no way for an ordinary user to cap a
  process's memory (`RLIMIT_RSS` is not enforced), so pausing is the only tool.
- **Never the process a shell waits on.** An interactive shell (one with a terminal) reports a stopped
  child as "suspended" and takes the prompt back, so loadline never stops a direct child of an
  interactive shell. It pauses that child's own children instead (for `npm run build`, the build process
  itself). If nothing else is left, the job is not paused at all. Measured: non-interactive zsh, bash and
  sh running a command in the foreground — what coding agents do — don't notice a pause at all (exit
  status 0, correct timing). The one exception found is zsh's `cmd & wait $!`, which returns 145.
- **Terminal notices.** When a job started from a terminal has to wait or is paused for memory, one line
  is written to that terminal, and another when it continues.
- **Lower priority:** macOS background priority (`PRIO_DARWIN_BG`: lowest CPU priority, efficiency
  cores, throttled disk I/O), which an ordinary user can switch off again; `nice` can't be undone without
  root. On Linux there is no reversible equivalent for ordinary users, so loadline uses nice 10 and the
  job keeps it until it ends.
- **Crash safety.** Before any signal, the processes about to be paused or lowered are written to
  `~/.loadline/paused`. On start, stop, `loadline off` and `loadline uninstall`, everything in that file
  is resumed. Processes are identified by pid *and* start time, so a reused pid is never touched.

### Memory pressure

- **macOS:** `sysctl kern.memorystatus_vm_pressure_level` (1 normal, 2 warn, 4 critical), the same level
  the system uses ([xnu](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_memorystatus_notify.c)).
- **Linux:** critical when less than 8% of memory is available or `full avg10` in
  `/proc/pressure/memory` reaches 10%; warn under 20% available or `some avg10` at 10%. This acts before
  [earlyoom](https://github.com/rfjakob/earlyoom) (kills at 10% available and 10% free swap) and
  [systemd-oomd](https://www.freedesktop.org/software/systemd/man/latest/oomd.conf.html) (kills at 60%
  pressure for 30 s) — and pauses rather than kills.

## Part 3: editors

VS Code's default `files.watcherExclude` no longer excludes `node_modules`
([files.contribution.ts](https://github.com/microsoft/vscode/blob/main/src/vs/workbench/contrib/files/browser/files.contribution.ts)),
so an `npm ci` inside an open folder floods the file watcher. Agent worktrees inside the project make it
worse: one project reported a 2.7 GB dev-server heap from watching `.claude/worktrees`, down to 819 MB
after excluding it ([datagouv/cdata#1305](https://github.com/datagouv/cdata/pull/1305)).
`loadline install` adds a marked `files.watcherExclude` entry to VS Code, Cursor, Windsurf and VSCodium
settings when they don't have one; `loadline uninstall` removes exactly that entry.

## Prior art

- [Kiri](https://runkiri.com/) and [OrbStack](https://orbstack.dev/): lightweight Docker runtimes; the
  model for "tiny, invisible, reversible".
- [puma-dev](https://github.com/puma/puma-dev) and [portless](https://github.com/vercel-labs/portless):
  start dev servers on demand behind a proxy. loadline needs no proxy: the kernel queues connections
  for a paused server.
- [earlyoom](https://github.com/rfjakob/earlyoom), [systemd-oomd](https://www.freedesktop.org/software/systemd/man/latest/systemd-oomd.service.html),
  [nohang](https://github.com/hakavlad/nohang): kill processes under memory pressure. loadline pauses
  instead, and only dev work.
- [cpulimit](https://github.com/opsengine/cpulimit): limits CPU with `SIGSTOP`/`SIGCONT` cycles.
- [Volta](https://github.com/volta-cli/volta) and [mise](https://mise.jdx.dev/): transparent tool shims;
  loadline uses a Node preload and environment variables instead, so it works whichever version
  manager installed Node.

## Platform notes

| | macOS | Linux |
|---|---|---|
| Processes | `proc_listallpids`, `proc_pidinfo(PROC_PIDTBSDINFO)`, `sysctl KERN_PROCARGS2` | `/proc/<pid>/stat`, `cmdline`, `environ` |
| Memory per process | physical footprint (`proc_pid_rusage`), Activity Monitor's "Memory" | resident minus shared pages (`/proc/<pid>/statm`) |
| CPU time | `proc_pid_rusage`, scaled by `mach_timebase_info` on Apple silicon | `/proc/<pid>/stat`, clock ticks |
| Start at login | LaunchAgent `loadline` | systemd user service `loadline` |

Windows is planned: Job objects and `NtSuspendProcess` for pausing, `SetPriorityClass` and EcoQoS for
priority, `GlobalMemoryStatusEx` for pressure. It has no API for a socket's accept-queue length, so idle
wake would need a small port proxy there.
