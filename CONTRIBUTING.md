# Contributing to nodoka

Thanks for helping. The most useful contributions, in order:

1. **A tool nodoka doesn't recognise.** Usually a one-line rule and a one-line test.
2. **Bug reports** with `nodoka status`, `nodoka explain <pid>` and `~/.nodoka/log`.
3. **Windows support** (see [docs/how-it-works.md](docs/how-it-works.md#platform-notes)).

Please read [docs/how-it-works.md](docs/how-it-works.md) before changing behaviour: most rules exist
because of something measured.

## Set up

You need Rust 1.85 or newer on macOS or Linux.

```sh
git clone https://github.com/sundarshahi/nodoka
cd nodoka
cargo test
```

`cargo test` runs the unit tests and the end-to-end tests in [`tests/daemon.rs`](tests/daemon.rs). The
end-to-end tests start a real helper that only governs the fake jobs they start (it is scoped with
`--test <name>` and its own `NODOKA_HOME`), so they are safe to run on your own machine. They take
about 10 seconds.

To run the Linux tests from a Mac:

```sh
docker run --rm -v "$PWD":/src -w /src -e CARGO_TARGET_DIR=/tmp/target rust:1-slim cargo test
```

Try the helper without letting it act on anything:

```sh
cargo run -- daemon --dry-run      # logs what it would do, pauses nothing
cargo run -- explain bin/rails server
```

## Where things are

| File | What it does |
|---|---|
| `src/classify.rs` | Command line → install / build / test / dev. All the tool rules. |
| `src/policy.rs` | What to do each tick. Pure functions, unit-tested. |
| `src/daemon.rs` | The loop: find jobs, measure, decide, act, write status. |
| `src/sys/macos.rs`, `src/sys/linux.rs` | Process, socket and memory facts; pause, resume, priority. |
| `src/setup.rs` | `install` / `uninstall`: shell files, editor settings, login service. |
| `src/config.rs` | `~/.nodoka/config`. |
| `preload/preload.cjs` | The Node/Bun preload that lowers worker counts. |
| `tests/daemon.rs`, `examples/fakejob.rs` | End-to-end tests and the fake tool they drive. |

## Adding a tool

1. Find out what nodoka sees: start the tool, then run `nodoka explain <pid>`. The command line may
   differ from what you typed: interpreters (`node …/vite.js`), wrappers (`bundle exec`) or a renamed
   process (`puma 8.0.3 (tcp://…)`).
2. Add a rule in `rule()` in `src/classify.rs`, next to its ecosystem. Use `pick(sub, …)` for
   subcommands and `dev_or(Build)` for tools whose `--watch` flag makes them long-running.
3. Add a line to the matching test in `classify.rs`, using the real command line you saw.
4. If it's a new ecosystem, add it to the table in `README.md`.

Choosing a kind:

- **install** — downloads dependencies. Admitted a few at a time; never paused for memory.
- **build** and **test** — run and finish. Admitted a few at a time; paused at critical memory.
- **dev** — runs until stopped (servers, watchers). Never queued; sleeps when idle if it has a port.

When unsure, leave the tool unrecognised. A tool nodoka ignores costs nothing; one it gets wrong can
pause something it shouldn't.

## Before you open a pull request

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

- Keep changes small and focused; one pull request per change.
- A bug fix comes with a test that fails without the fix.
- A behaviour change updates `docs/how-it-works.md` and, if users see it, `README.md` and `CHANGELOG.md`.
- Comments explain *why* in a line or two; longer explanations belong in the pull request.
- Write commit subjects in the imperative ("Recognise `astro preview`").

## Releasing (maintainers)

1. Update the version in `Cargo.toml` and move the `CHANGELOG.md` entries under the new version.
2. Commit, then tag: `git tag v0.2.0 && git push origin v0.2.0`.
3. The [release workflow](.github/workflows/release.yml) builds macOS and Linux binaries (x86_64 and
   arm64), adds SHA-256 checksums, and publishes a GitHub release that `install.sh` downloads.

## Reporting security issues

Please don't open a public issue; see [SECURITY.md](SECURITY.md).
