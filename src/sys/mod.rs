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

pub fn signal(pid: i32, stop: bool) {
    unsafe { libc::kill(pid, if stop { libc::SIGSTOP } else { libc::SIGCONT }) };
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
