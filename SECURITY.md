# Security policy

## Supported versions

Security fixes go into the latest release.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub: open the repository's **Security** tab and
choose **Report a vulnerability**. Don't open a public issue. You'll get a reply within a week, and a
fix or a plan within 30 days.

## What nodoka can do on a machine

So you can judge the impact of a report:

- It runs as your own user. It never asks for root and never touches other users' processes.
- It reads process information (command lines, environment variables, CPU and memory use, open TCP
  sockets) for your processes, and sends them `SIGSTOP`, `SIGCONT` and priority changes.
- It writes to `~/.nodoka`, your shell startup files (`~/.zshenv`, `~/.bashrc`, `~/.bash_profile`,
  fish's `conf.d`), your editors' `settings.json`, and its login service (a LaunchAgent or systemd user
  service). Each change is marked so `nodoka uninstall` can remove it.
- It adds a preload to `NODE_OPTIONS`, so its JavaScript file runs in every Node process. That file is
  `~/.nodoka/preload.cjs`, writable only by you.
- It makes no network connections and collects no telemetry. `install.sh` downloads a release from
  GitHub and checks its SHA-256 checksum before running it.
