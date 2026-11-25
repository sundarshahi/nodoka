//! Operating-system facts and controls. Only the current user's own processes are read or
//! touched, so nothing here needs root.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!(
    "nodoka runs on macOS and Linux (including WSL 2) for now. Windows support is planned: see the roadmap in README.md."
);

pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    /// Start time in the OS's own units; (pid, id) names one process even after pid reuse.
    pub id: u64,
    /// Has a controlling terminal.
    pub tty: bool,
    pub stopped: bool,
    pub comm: String,
}

/// A TCP socket of a watched process. `waiting` is the accept-queue length for a listening socket,
/// or unread bytes for a connection.
pub struct Sock {
    pub key: u64,
    pub listen: bool,
    pub waiting: u64,
}

/// Folders whose changes never wake a sleeping server: dependencies, build output, caches, logs.
/// On Linux they are also not watched, which keeps the number of inotify watches small.
const QUIET: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "bower_components",
    "vendor",
    "deps",
    "_build",
    "target",
    "dist",
    "build",
    "out",
    "coverage",
    "tmp",
    "log",
    "logs",
    ".next",
    ".nuxt",
    ".output",
    ".svelte-kit",
    ".astro",
    ".turbo",
    ".vercel",
    ".netlify",
    ".wrangler",
    ".cache",
    ".parcel-cache",
    ".angular",
    ".expo",
    ".docusaurus",
    ".gradle",
    ".idea",
    ".vscode",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".elixir_ls",
    ".dart_tool",
    "Pods",
    "DerivedData",
];

pub fn quiet(name: &str) -> bool {
    QUIET.contains(&name)
}

/// The folder whose changes wake a sleeping dev server: its git checkout (edits in a sibling
/// package count too), else its working folder. Never the home folder or anything above it.
pub fn project_root(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    let home = std::path::PathBuf::from(std::env::var_os("HOME")?);
    let ok = |p: &std::path::Path| p.parent().is_some() && !home.starts_with(p);
    let root = cwd.ancestors().take_while(|p| ok(p)).find(|p| p.join(".git").exists()).unwrap_or(cwd);
    ok(root).then(|| root.to_path_buf())
}

/// A desktop notification, in the background (it never holds up the daemon). Text goes in as
/// arguments or environment, never into a script. Linux without `notify-send`: nothing (it's in the log).
pub fn notify(title: &str, body: &str) {
    let wsl = std::fs::read_to_string("/proc/sys/kernel/osrelease").is_ok_and(|v| v.contains("microsoft"));
    let mut c = notify_cmd(title, body, wsl);
    c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    std::thread::spawn(move || c.status());
}

/// Windows toast from PowerShell; text comes from the environment (WSLENV carries it out of WSL).
pub const TOAST_PS1: &str = include_str!("toast.ps1");

fn notify_cmd(title: &str, body: &str, wsl: bool) -> std::process::Command {
    use std::process::Command;
    if cfg!(target_os = "macos") {
        let mut c = Command::new("osascript");
        c.args(["-e", "on run a", "-e", "display notification (item 2 of a) with title (item 1 of a)"]);
        c.args(["-e", "end run", title, body]);
        c
    } else if wsl {
        // the helper runs as a systemd service: Windows isn't on its PATH
        let mut c = Command::new("/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe");
        c.args(["-NoProfile", "-NonInteractive", "-Command", TOAST_PS1]);
        let carry = std::env::var("WSLENV").map_or(String::new(), |w| format!("{w}:"));
        c.env("NODOKA_TITLE", title).env("NODOKA_BODY", body).env("WSLENV", carry + "NODOKA_TITLE:NODOKA_BODY");
        c
    } else {
        let mut c = Command::new("notify-send");
        c.args(["--app-name=nodoka", title, body]);
        c
    }
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn in_wsl_the_notice_is_a_windows_toast_with_the_text_outside_the_script() {
    let c = notify_cmd("Memory is full", "firefox 7 GB; $(rm -rf ~)", true);
    assert!(c.get_program().to_string_lossy().ends_with("powershell.exe"));
    let env: Vec<_> = c.get_envs().map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned()))).collect();
    let get = |k: &str| env.iter().find(|e| e.0 == k).and_then(|e| e.1.clone()).unwrap();
    assert_eq!(get("NODOKA_BODY"), "firefox 7 GB; $(rm -rf ~)");
    assert!(get("WSLENV").ends_with("NODOKA_TITLE:NODOKA_BODY"));
    assert!(c.get_args().all(|a| !a.to_string_lossy().contains("firefox")), "text never in the script");
    assert_eq!(notify_cmd("t", "b", false).get_program(), "notify-send");
}

pub fn load1() -> f64 {
    let mut l = [0f64; 3];
    let n = unsafe { libc::getloadavg(l.as_mut_ptr(), 3) };
    if n > 0 { l[0] } else { 0.0 }
}

/// Lower CPU priority: nice 10. It yields to your apps only while cores are contested and costs
/// nothing otherwise. (macOS's reversible background priority confines work to the efficiency
/// cores: three parallel `next build`s took 40s instead of 8s.) An ordinary user can't raise
/// priority back, so the restore usually fails and the job stays nice until it ends.
pub fn set_bg(pid: i32, on: bool) {
    unsafe { libc::setpriority(libc::PRIO_PROCESS as _, pid as libc::id_t, if on { 10 } else { 0 }) };
}

/// The slow lane, for a build while memory is critically low: it keeps running, but only on spare
/// CPU and disk time, so your apps' swap-ins go first. macOS: background priority (efficiency cores,
/// throttled disk). Linux: idle disk priority. Reversible.
#[cfg(target_os = "macos")]
pub fn set_slow(pid: i32, on: bool) {
    unsafe {
        libc::setpriority(libc::PRIO_DARWIN_PROCESS, pid as libc::id_t, if on { libc::PRIO_DARWIN_BG } else { 0 })
    };
}

#[cfg(target_os = "linux")]
pub fn set_slow(pid: i32, on: bool) {
    // per thread on Linux; class idle = 3, none = 0 (the default: follows nice)
    let prio: libc::c_long = if on { 3 << 13 } else { 0 };
    let tids = std::fs::read_dir(format!("/proc/{pid}/task")).into_iter().flatten().flatten();
    for t in tids.filter_map(|e| e.file_name().to_str()?.parse::<libc::c_long>().ok()) {
        unsafe { libc::syscall(libc::SYS_ioprio_set, 1 as libc::c_long, t, prio) };
    }
}

/// Returns the error if the signal wasn't delivered.
pub fn signal(pid: i32, stop: bool) -> Option<std::io::Error> {
    let r = unsafe { libc::kill(pid, if stop { libc::SIGSTOP } else { libc::SIGCONT }) };
    (r != 0).then(std::io::Error::last_os_error)
}

pub fn uid() -> u32 {
    unsafe { libc::getuid() }
}

pub fn cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Shells that do job control when interactive: stopping their direct child would print
/// "suspended" and hand the prompt back.
pub fn is_shell(comm: &str) -> bool {
    matches!(
        comm.trim_start_matches('-'),
        "zsh"
            | "bash"
            | "fish"
            | "sh"
            | "dash"
            | "ksh"
            | "mksh"
            | "tcsh"
            | "csh"
            | "nu"
            | "pwsh"
            | "elvish"
            | "xonsh"
            | "yash"
    )
}
