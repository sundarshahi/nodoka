//! Windows: Toolhelp snapshots for processes, the process's own counters for CPU and memory,
//! `GlobalMemoryStatusEx` for pressure. Only processes in nodoka's own logon session are read.

use super::{Proc, Sock};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE, LocalFree, SYSTEMTIME};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS, TH32CS_SNAPTHREAD,
    THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcessId, GetProcessTimes, GetSystemTimes, IDLE_PRIORITY_CLASS,
    NORMAL_PRIORITY_CLASS, OpenProcess, OpenThread, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
    PROCESS_SUSPEND_RESUME, PROCESS_TERMINATE, ResumeThread, SetPriorityClass, THREAD_SUSPEND_RESUME, TerminateProcess,
};
use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtSuspendProcess(process: HANDLE) -> i32;
    fn NtQueryInformationProcess(process: HANDLE, class: u32, info: *mut u8, len: u32, ret: *mut u32) -> i32;
}

/// `ProcessCommandLineInformation` (Windows 8.1+): the command line without reading the process's memory.
const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;

/// An open process handle, closed when dropped.
struct H(HANDLE);

impl H {
    fn open(pid: i32, access: u32) -> Option<H> {
        let h = unsafe { OpenProcess(access, 0, pid as u32) };
        (!h.is_null()).then_some(H(h))
    }
}

impl Drop for H {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn ft(f: FILETIME) -> u64 {
    (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64
}

fn session(pid: u32) -> Option<u32> {
    let mut s = 0;
    (unsafe { ProcessIdToSessionId(pid, &mut s) } != 0).then_some(s)
}

/// (creation, kernel + user CPU) in 100 ns units.
fn times(pid: i32) -> Option<(u64, u64)> {
    let h = H::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    (unsafe { GetProcessTimes(h.0, &mut c, &mut e, &mut k, &mut u) } != 0).then(|| (ft(c), ft(k) + ft(u)))
}

/// The user's processes: those in nodoka's own logon session (services and other users are in others).
pub fn procs(_uid: u32) -> Vec<Proc> {
    let me = session(unsafe { GetCurrentProcessId() });
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return vec![];
    }
    let snap = H(snap);
    let mut e: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut out = vec![];
    let mut ok = unsafe { Process32FirstW(snap.0, &mut e) } != 0;
    while ok {
        let pid = e.th32ProcessID;
        if pid != 0 && session(pid) == me {
            if let Some((created, _)) = times(pid as i32) {
                let name =
                    String::from_utf16_lossy(&e.szExeFile[..e.szExeFile.iter().position(|&c| c == 0).unwrap_or(0)]);
                out.push(Proc {
                    pid: pid as i32,
                    ppid: e.th32ParentProcessID as i32,
                    id: created,
                    tty: true,
                    stopped: false,
                    comm: name.strip_suffix(".exe").or(name.strip_suffix(".EXE")).unwrap_or(&name).to_string(),
                });
            }
        }
        ok = unsafe { Process32NextW(snap.0, &mut e) } != 0;
    }
    super::tty_by_ancestry(&mut out, crate::agents::NAMES);
    out
}

/// Arguments with `\` turned into `/`, so the rules' paths (`node_modules/next/…`) match.
// ponytail: no environment yet (it needs the process's memory read); test scopes and
// CLAUDE_CONFIG_DIR aren't seen on Windows
pub fn args_env(pid: i32) -> Option<(Vec<String>, Vec<String>)> {
    let h = H::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut buf = vec![0u8; 8192];
    loop {
        let mut need = 0u32;
        let st = unsafe {
            NtQueryInformationProcess(
                h.0,
                PROCESS_COMMAND_LINE_INFORMATION,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut need,
            )
        };
        if st >= 0 {
            break;
        }
        if need as usize <= buf.len() || buf.len() > 1 << 20 {
            return None;
        }
        buf.resize(need as usize, 0);
    }
    // a UNICODE_STRING (u16 length in bytes, u16 capacity, pointer) followed by its text
    let len = u16::from_ne_bytes([buf[0], buf[1]]) as usize / 2;
    let ptr = usize::from_ne_bytes(buf[8..16].try_into().ok()?) as *const u16;
    let mut line: Vec<u16> = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
    line.push(0);
    let mut n = 0;
    let argv = unsafe { CommandLineToArgvW(line.as_ptr(), &mut n) };
    if argv.is_null() {
        return None;
    }
    let args = (0..n as usize)
        .map(|i| {
            let p = unsafe { *argv.add(i) };
            let l = (0..).take_while(|&j| unsafe { *p.add(j) } != 0).count();
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, l) }).replace('\\', "/")
        })
        .collect();
    unsafe { LocalFree(argv as _) };
    Some((args, vec![]))
}

/// (CPU time in ns, private bytes: what the process alone commits, like Task Manager's "Memory").
pub fn cpu_mem(pid: i32) -> Option<(u64, u64)> {
    let (_, cpu) = times(pid)?;
    let h = H::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut m: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    m.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    let ok = unsafe { K32GetProcessMemoryInfo(h.0, &mut m as *mut _ as *mut PROCESS_MEMORY_COUNTERS, m.cb) } != 0;
    ok.then_some((cpu * 100, m.PrivateUsage as u64))
}

// ponytail: no sockets yet (GetExtendedTcpTable); idle dev servers don't sleep on Windows
pub fn sockets(_pids: &[i32]) -> Vec<Sock> {
    vec![]
}

fn memory() -> MEMORYSTATUSEX {
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    unsafe { GlobalMemoryStatusEx(&mut m) };
    m
}

/// 1 normal, 2 warn, 4 critical — from free physical memory and how full the commit limit is
/// (Windows has no pressure level; a full commit limit is when allocations start failing).
pub fn pressure() -> u32 {
    let m = memory();
    let avail_pct = (m.ullAvailPhys * 100).checked_div(m.ullTotalPhys).unwrap_or(100);
    let commit_pct = 100 - (m.ullAvailPageFile * 100).checked_div(m.ullTotalPageFile).unwrap_or(100);
    level(avail_pct, commit_pct)
}

fn level(avail_pct: u64, commit_pct: u64) -> u32 {
    if avail_pct < 8 || commit_pct >= 95 {
        4
    } else if avail_pct < 20 || commit_pct >= 85 {
        2
    } else {
        1
    }
}

// ponytail: no freeze signal yet (hard page faults per second need performance counters)
pub fn freezing() -> bool {
    false
}

pub fn avail_bytes() -> u64 {
    memory().ullAvailPhys
}

pub fn mem_bytes() -> u64 {
    match memory().ullTotalPhys {
        0 => 8 << 30,
        n => n,
    }
}

// ponytail: no working folder yet (it needs the process's memory read); worktree cleanup and
// wake-on-save are off on Windows
pub fn cwd(_pid: i32) -> Option<PathBuf> {
    None
}

pub struct Watch;

impl Watch {
    pub fn new(_root: &Path) -> Option<Watch> {
        None
    }

    pub fn changed(&mut self) -> bool {
        false
    }
}

pub fn tty_path(_pid: i32) -> Option<PathBuf> {
    None
}

/// Seconds since the epoch, from the creation time (100 ns since 1601).
pub fn started(p: &Proc) -> u64 {
    (p.id / 10_000_000).saturating_sub(11_644_473_600)
}

/// Like a load average: busy cores right now, from how much of the last interval the CPUs idled.
pub fn load1() -> f64 {
    static LAST: Mutex<(u64, u64)> = Mutex::new((0, 0));
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut i, mut k, mut u) = (z, z, z);
    if unsafe { GetSystemTimes(&mut i, &mut k, &mut u) } == 0 {
        return 0.0;
    }
    // kernel time includes idle time
    let (idle, total) = (ft(i), ft(k) + ft(u));
    let mut last = LAST.lock().unwrap();
    let (di, dt) = (idle.saturating_sub(last.0), total.saturating_sub(last.1));
    *last = (idle, total);
    if dt == 0 { 0.0 } else { (dt - di.min(dt)) as f64 / dt as f64 * super::cores() as f64 }
}

fn priority(pid: i32, class: u32) {
    if let Some(h) = H::open(pid, PROCESS_SET_INFORMATION) {
        unsafe { SetPriorityClass(h.0, class) };
    }
}

/// Lower CPU priority (below normal); restored when turned off.
pub fn set_bg(pid: i32, on: bool) {
    priority(pid, if on { BELOW_NORMAL_PRIORITY_CLASS } else { NORMAL_PRIORITY_CLASS });
}

/// The slow lane: idle priority, so it only gets CPU nobody else wants.
// ponytail: CPU only; EcoQoS (efficiency cores) and low disk priority would match macOS more closely
pub fn set_slow(pid: i32, on: bool) {
    priority(pid, if on { IDLE_PRIORITY_CLASS } else { NORMAL_PRIORITY_CLASS });
}

/// Pause or resume a process. Windows counts suspensions, so a resume undoes every one (a paused
/// job paused again must still continue on one resume, like SIGSTOP/SIGCONT).
pub fn signal(pid: i32, stop: bool) -> Option<std::io::Error> {
    if stop {
        let Some(h) = H::open(pid, PROCESS_SUSPEND_RESUME) else { return Some(std::io::Error::last_os_error()) };
        let st = unsafe { NtSuspendProcess(h.0) };
        return (st < 0).then(|| std::io::Error::other(format!("NtSuspendProcess: {st:#x}")));
    }
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return Some(std::io::Error::last_os_error());
    }
    let snap = H(snap);
    let mut t: THREADENTRY32 = unsafe { std::mem::zeroed() };
    t.dwSize = size_of::<THREADENTRY32>() as u32;
    let mut ok = unsafe { Thread32First(snap.0, &mut t) } != 0;
    while ok {
        if t.th32OwnerProcessID == pid as u32 {
            let th = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, t.th32ThreadID) };
            if !th.is_null() {
                let th = H(th);
                // returns the count before: 1 means it now runs; u32::MAX is an error
                while (2..u32::MAX).contains(&unsafe { ResumeThread(th.0) }) {}
            }
        }
        ok = unsafe { Thread32Next(snap.0, &mut t) } != 0;
    }
    None
}

/// Ask a process to exit. Windows has no SIGTERM for a program without a window: it is ended.
pub fn terminate(pid: i32) {
    if let Some(h) = H::open(pid, PROCESS_TERMINATE) {
        unsafe { TerminateProcess(h.0, 1) };
    }
}

pub fn uid() -> u32 {
    0
}

pub fn clock() -> String {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_listed_with_its_command_line_and_counters() {
        let me = std::process::id() as i32;
        let p = procs(0).into_iter().find(|p| p.pid == me).expect("nodoka's own process");
        assert!(started(&p) > 1_700_000_000, "{}", started(&p));
        let (argv, _) = args_env(me).unwrap();
        assert!(!argv[0].contains('\\') && argv[0].contains('/'), "{argv:?}");
        let (cpu, mem) = cpu_mem(me).unwrap();
        assert!(cpu > 0 && mem > 1 << 20, "{cpu} {mem}");
        assert!(mem_bytes() > avail_bytes() && avail_bytes() > 0);
    }

    #[test]
    fn a_paused_process_paused_twice_continues_on_one_resume() {
        let mut child = std::process::Command::new("ping").args(["-n", "30", "127.0.0.1"]).spawn().unwrap();
        let pid = child.id() as i32;
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(signal(pid, true).is_none());
        assert!(signal(pid, true).is_none());
        assert!(signal(pid, false).is_none());
        // a resumed thread has a suspend count of 0: suspending it once more returns 0 before
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        let snap = H(snap);
        let mut t: THREADENTRY32 = unsafe { std::mem::zeroed() };
        t.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut ok = unsafe { Thread32First(snap.0, &mut t) } != 0;
        let mut checked = 0;
        while ok {
            if t.th32OwnerProcessID == pid as u32 {
                let th = H(unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, t.th32ThreadID) });
                assert_eq!(unsafe { windows_sys::Win32::System::Threading::SuspendThread(th.0) }, 0, "still suspended");
                unsafe { ResumeThread(th.0) };
                checked += 1;
            }
            ok = unsafe { Thread32Next(snap.0, &mut t) } != 0;
        }
        assert!(checked > 0);
        terminate(pid);
        child.wait().unwrap();
    }

    #[test]
    fn memory_levels() {
        assert_eq!(level(50, 40), 1);
        assert_eq!(level(15, 40), 2);
        assert_eq!(level(50, 90), 2);
        assert_eq!(level(5, 40), 4);
        assert_eq!(level(50, 97), 4);
    }
}
