use super::{Proc, Sock};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::OnceLock;

fn clk_tck() -> u64 {
    static T: OnceLock<u64> = OnceLock::new();
    *T.get_or_init(|| (unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).max(1) as u64)
}

fn page_size() -> u64 {
    static P: OnceLock<u64> = OnceLock::new();
    *P.get_or_init(|| (unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).max(1) as u64)
}

struct Stat {
    comm: String,
    state: char,
    ppid: i32,
    tty_nr: i64,
    utime: u64,
    stime: u64,
    starttime: u64,
}

fn stat(pid: i32) -> Option<Stat> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (open, close) = (s.find('(')?, s.rfind(')')?);
    let f: Vec<&str> = s[close + 1..].split_whitespace().collect();
    let n = |i: usize| f.get(i).and_then(|v| v.parse::<i64>().ok());
    Some(Stat {
        comm: s[open + 1..close].to_string(),
        state: f.first()?.chars().next()?,
        ppid: n(1)? as i32,
        tty_nr: n(4)?,
        utime: n(11)? as u64,
        stime: n(12)? as u64,
        starttime: n(19)? as u64,
    })
}

pub fn procs(uid: u32) -> Vec<Proc> {
    let Ok(dir) = fs::read_dir("/proc") else { return vec![] };
    dir.flatten()
        .filter_map(|e| {
            let pid: i32 = e.file_name().to_str()?.parse().ok()?;
            if e.metadata().ok()?.uid() != uid {
                return None;
            }
            let s = stat(pid)?;
            Some(Proc {
                pid,
                ppid: s.ppid,
                id: s.starttime,
                tty: s.tty_nr != 0,
                stopped: matches!(s.state, 'T' | 't'),
                comm: s.comm,
            })
        })
        .collect()
}

pub fn args_env(pid: i32) -> Option<(Vec<String>, Vec<String>)> {
    let argv = split_nul(&fs::read(format!("/proc/{pid}/cmdline")).ok()?);
    let env = split_nul(&fs::read(format!("/proc/{pid}/environ")).unwrap_or_default());
    Some((argv, env))
}

/// (CPU time in ns, memory in bytes). Memory is resident minus shared file pages, close to the
/// process's private memory, and cheap to read.
pub fn cpu_mem(pid: i32) -> Option<(u64, u64)> {
    let s = stat(pid)?;
    let statm = fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let mut f = statm.split_whitespace().skip(1).filter_map(|v| v.parse::<u64>().ok());
    let (resident, shared) = (f.next()?, f.next()?);
    Some(((s.utime + s.stime) * 1_000_000_000 / clk_tck(), resident.saturating_sub(shared) * page_size()))
}

/// TCP sockets of the given processes. For a listening socket /proc/net/tcp's rx_queue is the
/// accept-queue length; for a connection it is unread bytes. The kernel completes handshakes into
/// the queue even while the process is stopped.
pub fn sockets(pids: &[i32]) -> Vec<Sock> {
    let mut inodes: HashSet<u64> = HashSet::new();
    for &pid in pids {
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for fd in fds.flatten() {
            if let Some(ino) = fs::read_link(fd.path())
                .ok()
                .and_then(|l| l.to_str()?.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok())
            {
                inodes.insert(ino);
            }
        }
    }
    if inodes.is_empty() {
        return vec![];
    }
    let mut out = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = fs::read_to_string(table) else { continue };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            let (Some(st), Some(queues), Some(ino)) =
                (f.get(3), f.get(4), f.get(9).and_then(|v| v.parse::<u64>().ok()))
            else {
                continue;
            };
            if !inodes.contains(&ino) {
                continue;
            }
            let rx = queues.split(':').nth(1).and_then(|v| u64::from_str_radix(v, 16).ok()).unwrap_or(0);
            out.push(Sock { key: ino, listen: *st == "0A", waiting: rx });
        }
    }
    out
}

/// 1 normal, 2 warn, 4 critical — from pressure-stall information (PSI) when the kernel has it,
/// and from available memory.
pub fn pressure() -> u32 {
    let mem = meminfo();
    let (total, avail) = (mem.get("MemTotal").copied().unwrap_or(0), mem.get("MemAvailable").copied().unwrap_or(0));
    let avail_pct = (avail * 100).checked_div(total).unwrap_or(100);
    let psi = fs::read_to_string("/proc/pressure/memory").unwrap_or_default();
    let avg10 = |kind: &str| {
        psi.lines()
            .find(|l| l.starts_with(kind))
            .and_then(|l| l.split_whitespace().find_map(|w| w.strip_prefix("avg10=")?.parse::<f64>().ok()))
            .unwrap_or(0.0)
    };
    if avail_pct < 8 || avg10("full") >= 10.0 {
        4
    } else if avail_pct < 20 || avg10("some") >= 10.0 {
        2
    } else {
        1
    }
}

fn meminfo() -> HashMap<String, u64> {
    let text = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.to_string(), v.split_whitespace().next()?.parse::<u64>().ok()? * 1024))
        })
        .collect()
}

pub fn mem_bytes() -> u64 {
    meminfo().get("MemTotal").copied().unwrap_or(8 << 30)
}

/// Lower CPU priority (nice 10). Linux only lets an ordinary user raise priority back within
/// RLIMIT_NICE, so the restore usually fails: the job then stays nice until it ends, which only
/// matters while the CPU is contended.
pub fn set_bg(pid: i32, on: bool) {
    unsafe { libc::setpriority(libc::PRIO_PROCESS as _, pid as libc::id_t, if on { 10 } else { 0 }) };
}

/// The terminal device a process is attached to, for a one-line notice.
pub fn tty_path(pid: i32) -> Option<PathBuf> {
    (0..3).find_map(|fd| {
        let p = fs::read_link(format!("/proc/{pid}/fd/{fd}")).ok()?;
        let s = p.to_str()?;
        (s.starts_with("/dev/pts/") || s.starts_with("/dev/tty")).then_some(p)
    })
}

fn split_nul(bytes: &[u8]) -> Vec<String> {
    let mut v: Vec<String> = bytes.split(|&c| c == 0).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
    while v.last().is_some_and(|s| s.is_empty()) {
        v.pop();
    }
    v
}
