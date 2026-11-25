# nodoka on Windows

There are two ways, depending on where your tools run:

- **Native Windows**: your projects, terminals and coding agents run on Windows itself (PowerShell,
  Windows Terminal, VS Code on Windows, Windows Node).
- **WSL 2**: they run inside Linux on Windows. Use this one if you develop in WSL.

## Native Windows

In PowerShell (no administrator needed):

```powershell
irm https://raw.githubusercontent.com/sundarshahi/nodoka/main/install.ps1 | iex
```

Then open a new terminal and restart VS Code and any coding agents. `nodoka status` shows what it is
doing. `nodoka install` sets your user environment variables (Node's preload, worker caps where you
haven't set your own, nodoka's folder on your Path) and starts the helper now and at login.
`nodoka uninstall` puts every variable back as it was.

What's different from macOS and Linux:

- **Idle dev servers don't sleep.** Waking one safely needs to see a request waiting, and Windows
  doesn't show that for another program's connections. Everything else (a few heavy jobs at a time,
  pausing builds while memory is short, stuck test runs, idle agent sessions) works the same.
- **Ending a job is immediate.** Windows has no "please exit" signal for programs without a window,
  so the few things nodoka asks to exit (a stuck test run, an agent's build held 10 minutes) are ended.
- **A console window flashes briefly at login** while the helper starts.
- **Your terminal vs an agent**: Windows has no terminal ownership, so nodoka goes by who started a
  program: anything a coding agent (Claude Code, Codex) started is the agent's; the rest is yours.

## WSL 2

### Set it up (about 10 minutes)

1. **Install WSL 2.** In PowerShell opened as administrator:

   ```powershell
   wsl --install
   ```

   Restart when it asks, then open **Ubuntu** from the Start menu and create your Linux user.

2. **Check systemd is on.** nodoka starts at login through systemd. In Ubuntu:

   ```sh
   ps -p 1 -o comm=
   ```

   If this prints `systemd`, go on. Otherwise run `sudo nano /etc/wsl.conf`, add these two lines,
   save, run `wsl --shutdown` in PowerShell, and open Ubuntu again:

   ```ini
   [boot]
   systemd=true
   ```

3. **Install nodoka** in Ubuntu:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/sundarshahi/nodoka/main/install.sh | sh
   ```

   Then close Ubuntu and open it again.

4. **Keep your projects inside WSL**, for example `~/code/app`, not under `/mnt/c/...`. Builds are
   much faster there, and nodoka only notices saved files there (that's what wakes a sleeping dev
   server).

5. **Open projects in WSL.** In VS Code, install the **WSL** extension, then run
   `code .` from a project folder in Ubuntu. Start coding agents from that window or from Ubuntu.

### Check it's working

```sh
nodoka status
```

It shows memory pressure, the load, and any builds, tests or dev servers it's looking after. Start
a build or a dev server and run it again: it should be listed.

When memory stays critically low with no build running, a Windows notification (from Windows
PowerShell) names the programs holding it.

### Optional: give Windows its memory back

WSL keeps memory it has used until it shuts down. To let it hand unused memory back to Windows, put
this in `C:\Users\<you>\.wslconfig`, then run `wsl --shutdown`:

```ini
[experimental]
autoMemoryReclaim=gradual
```

nodoka judges memory inside WSL. By default WSL gets half of the PC's memory, and nodoka works
within that.

## If something looks wrong

Run these in Ubuntu and send the output:

```sh
nodoka status
tail -50 ~/.nodoka/log
uname -r
ps -p 1 -o comm=
systemctl --user status nodoka --no-pager
```

On native Windows, run these in PowerShell instead:

```powershell
nodoka status
Get-Content -Tail 50 ~\.nodoka\log
```
