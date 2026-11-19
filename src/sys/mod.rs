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
    "loadline runs on macOS and Linux (including WSL 2) for now. Windows support is planned: see the roadmap in README.md."
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

pub fn load1() -> f64 {
    let mut l = [0f64; 3];
    let n = unsafe { libc::getloadavg(l.as_mut_ptr(), 3) };
    if n > 0 { l[0] } else { 0.0 }
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
