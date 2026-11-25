# nodoka on Windows

nodoka runs on Windows inside **WSL 2**. It looks after everything you run inside WSL: terminals,
VS Code opened in WSL, and coding agents (Claude Code, Codex) started there. It doesn't see programs
running on Windows itself (Windows Node, PowerShell builds, Windows apps). A native Windows version
is in progress.

## Set it up (about 10 minutes)

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

## Check it's working

```sh
nodoka status
```

It shows memory pressure, the load, and any builds, tests or dev servers it's looking after. Start
a build or a dev server and run it again: it should be listed.

When memory stays critically low with no build running, a Windows notification (from Windows
PowerShell) names the programs holding it.

## Optional: give Windows its memory back

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
