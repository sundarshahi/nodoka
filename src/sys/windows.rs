//! Windows: Toolhelp snapshots for processes, the process's own counters for CPU and memory,
//! `GlobalMemoryStatusEx` for pressure. Only processes in nodoka's own logon session are read.

use super::{Proc, Sock};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE, LocalFree, SYSTEMTIME};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS, TH32CS_SNAPTHREAD,
    THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE, PdhAddEnglishCounterW, PdhCollectQueryData, PdhGetFormattedCounterValue,
    PdhOpenQueryW,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcessId, GetProcessTimes, GetSystemTimes, IDLE_PRIORITY_CLASS,
    NORMAL_PRIORITY_CLASS, OpenProcess, OpenThread, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
    PROCESS_SUSPEND_RESUME, PROCESS_TERMINATE, PROCESS_VM_READ, ResumeThread, SetPriorityClass, THREAD_SUSPEND_RESUME,
    TerminateProcess,
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
    let env = params(pid).map(|p| p.0).unwrap_or_default();
    Some((args, env))
}

fn read<T: Copy>(h: HANDLE, at: usize) -> Option<T> {
    let mut v = std::mem::MaybeUninit::<T>::uninit();
    let mut n = 0;
    let ok = unsafe { ReadProcessMemory(h, at as _, v.as_mut_ptr() as _, size_of::<T>(), &mut n) } != 0;
    (ok && n == size_of::<T>()).then(|| unsafe { v.assume_init() })
}

fn read_utf16(h: HANDLE, at: usize, bytes: usize) -> Option<Vec<u16>> {
    let mut v = vec![0u16; bytes / 2];
    let mut n = 0;
    let ok = unsafe { ReadProcessMemory(h, at as _, v.as_mut_ptr() as _, v.len() * 2, &mut n) } != 0;
    ok.then(|| {
        v.truncate(n / 2);
        v
    })
}

/// (environment, working folder) from the process's parameters block, read out of its memory.
/// Offsets are the 64-bit layout (x64 and ARM64): PEB.ProcessParameters at 0x20; in the
/// parameters, CurrentDirectory.DosPath at 0x38, Environment at 0x80, EnvironmentSize at 0x3F0.
fn params(pid: i32) -> Option<(Vec<String>, PathBuf)> {
    let h = H::open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ)?;
    // PROCESS_BASIC_INFORMATION: exit status, PEB address, affinity, priority, pid, parent pid
    let mut pbi = [0usize; 6];
    let mut ret = 0;
    if unsafe { NtQueryInformationProcess(h.0, 0, pbi.as_mut_ptr() as *mut u8, size_of_val(&pbi) as u32, &mut ret) } < 0
    {
        return None;
    }
    let pp: usize = read(h.0, pbi[1] + 0x20)?;
    let (cwd_len, cwd_at): (u16, usize) = (read(h.0, pp + 0x38)?, read(h.0, pp + 0x40)?);
    let (env_at, env_size): (usize, usize) = (read(h.0, pp + 0x80)?, read(h.0, pp + 0x3F0)?);
    let cwd = String::from_utf16_lossy(&read_utf16(h.0, cwd_at, cwd_len as usize)?);
    // "C:\work\app\" → "C:\work\app"; a drive's root keeps its backslash
    let cwd = if cwd.len() > 3 { cwd.trim_end_matches('\\').to_string() } else { cwd };
    let block = read_utf16(h.0, env_at, env_size.min(1 << 20)).unwrap_or_default();
    let env = block
        .split(|&c| c == 0)
        .take_while(|e| !e.is_empty())
        .map(String::from_utf16_lossy)
        .filter(|e| !e.starts_with('=')) // "=C:=C:\work": the shell's per-drive folders
        .collect();
    Some((env, PathBuf::from(cwd)))
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

/// None reported: idle dev servers don't sleep on Windows. Waking one needs to see a request
/// waiting, and Windows doesn't show another process's unread socket data or accept queue (a
/// browser reusing a kept-alive connection to a paused server would hang).
// ponytail: a small port proxy in front of the server would make sleep safe here
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
    // swapping hard is critical whatever the counters say (as on macOS)
    if page_in_mbs() >= 30.0 { 4 } else { level(avail_pct, commit_pct) }
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

/// Memory read back from the page file, MB/s, smoothed like macOS's swap-in rate (one sample a
/// second at most; "\\Memory\\Pages Input/sec" counts 4 KB pages read on hard faults).
fn page_in_mbs() -> f64 {
    static Q: OnceLock<Option<(usize, usize)>> = OnceLock::new();
    static LAST: Mutex<(u64, f64)> = Mutex::new((0, 0.0));
    let Some(&(q, c)) = Q
        .get_or_init(|| {
            let (mut q, mut c) = (unsafe { std::mem::zeroed() }, unsafe { std::mem::zeroed() });
            let path: Vec<u16> = "\\Memory\\Pages Input/sec".encode_utf16().chain([0]).collect();
            let ok = unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut q) } == 0
                && unsafe { PdhAddEnglishCounterW(q, path.as_ptr(), 0, &mut c) } == 0
                && unsafe { PdhCollectQueryData(q) } == 0;
            ok.then_some((q as usize, c as usize))
        })
        .as_ref()
    else {
        return 0.0;
    };
    let mut last = LAST.lock().unwrap();
    let now = super::now();
    if now == last.0 {
        return last.1;
    }
    let mut v = PDH_FMT_COUNTERVALUE::default();
    let ok = unsafe { PdhCollectQueryData(q as _) } == 0
        && unsafe { PdhGetFormattedCounterValue(c as _, PDH_FMT_DOUBLE, std::ptr::null_mut(), &mut v) } == 0;
    let mbs = if ok { (unsafe { v.Anonymous.doubleValue }) * 4096.0 / (1 << 20) as f64 } else { 0.0 };
    *last = (now, 0.7 * last.1 + 0.3 * mbs);
    last.1
}

/// Reading memory back from disk this fast is what a freeze is (the same line as on macOS).
pub fn freezing() -> bool {
    page_in_mbs() >= 100.0
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

pub fn cwd(pid: i32) -> Option<PathBuf> {
    params(pid).map(|p| p.1)
}

/// Bytes this user can still write on the disk holding `p`.
pub fn disk_free(p: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let w: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    let mut free = 0u64;
    (unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) } != 0)
        .then_some(free)
}

/// Unused on Windows: it wakes sleeping servers, and they don't sleep here (see `sockets`).
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
    fn another_processs_environment_and_working_folder_are_read() {
        let dir = std::env::temp_dir();
        let mut child = std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .env("NODOKA_TEST", "envcheck")
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (_, env) = args_env(pid).unwrap();
        assert!(env.iter().any(|e| e == "NODOKA_TEST=envcheck"), "{env:?}");
        let got = cwd(pid).unwrap();
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap().to_string_lossy().to_lowercase();
        assert_eq!(canon(&got), canon(&dir));
        terminate(pid);
        child.wait().unwrap();
        assert!(page_in_mbs() >= 0.0 && !freezing() || page_in_mbs() >= 100.0);
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
