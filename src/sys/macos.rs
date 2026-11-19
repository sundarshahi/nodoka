use super::{Proc, Sock};
use libc::{c_int, c_void};
use std::mem::{size_of, zeroed};
use std::path::PathBuf;

pub fn procs(uid: u32) -> Vec<Proc> {
    let mut pids = vec![0 as c_int; 16384];
    let n =
        unsafe { libc::proc_listallpids(pids.as_mut_ptr() as *mut c_void, (pids.len() * size_of::<c_int>()) as c_int) };
    pids.truncate(n.max(0) as usize);
    pids.into_iter().filter(|&p| p > 0).filter_map(bsd).filter(|b| b.pbi_uid == uid).map(|b| to_proc(&b)).collect()
}

fn bsd(pid: i32) -> Option<libc::proc_bsdinfo> {
    let mut b: libc::proc_bsdinfo = unsafe { zeroed() };
    let sz = size_of::<libc::proc_bsdinfo>() as c_int;
    let r = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, &mut b as *mut _ as *mut c_void, sz) };
    (r == sz).then_some(b)
}

fn has_tty(b: &libc::proc_bsdinfo) -> bool {
    b.e_tdev != u32::MAX && b.e_tdev != 0
}

fn to_proc(b: &libc::proc_bsdinfo) -> Proc {
    let comm: Vec<u8> = b.pbi_comm.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    Proc {
        pid: b.pbi_pid as i32,
        ppid: b.pbi_ppid as i32,
        id: b.pbi_start_tvsec * 1_000_000 + b.pbi_start_tvusec,
        tty: has_tty(b),
        stopped: b.pbi_status == 4, // SSTOP
        comm: String::from_utf8_lossy(&comm).into_owned(),
    }
}

/// argv and environment, read with KERN_PROCARGS2: argc, exec path, padding, argv…, env….
pub fn args_env(pid: i32) -> Option<(Vec<String>, Vec<String>)> {
    thread_local!(static BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(vec![0u8; 1 << 20]));
    BUF.with_borrow_mut(|buf| parse_procargs(pid, buf))
}

fn parse_procargs(pid: i32, buf: &mut [u8]) -> Option<(Vec<String>, Vec<String>)> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut len = buf.len();
    let r = unsafe {
        libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr() as *mut c_void, &mut len, std::ptr::null_mut(), 0)
    };
    if r != 0 || len < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?) as usize;
    let mut parts = buf[4..len].split(|&c| c == 0).skip(1).skip_while(|s| s.is_empty());
    let argv: Vec<String> = parts.by_ref().take(argc).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
    let env = parts.take_while(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
    Some((argv, env))
}

/// (CPU time in ns, memory in bytes). Memory is the physical footprint: Activity Monitor's "Memory".
pub fn cpu_mem(pid: i32) -> Option<(u64, u64)> {
    let mut ri: libc::rusage_info_v4 = unsafe { zeroed() };
    let r = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, &mut ri as *mut _ as *mut libc::rusage_info_t) };
    if r != 0 {
        return None;
    }
    let (num, den) = timebase();
    Some(((ri.ri_user_time + ri.ri_system_time) * num / den, ri.ri_phys_footprint))
}

// rusage times are mach ticks on Apple silicon (ns on Intel, where the ratio is 1/1)
fn timebase() -> (u64, u64) {
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut [u32; 2]) -> c_int;
    }
    let mut t = [0u32; 2];
    unsafe { mach_timebase_info(&mut t) };
    if t[1] == 0 { (1, 1) } else { (t[0] as u64, t[1] as u64) }
}

/// TCP sockets of the given processes. The kernel completes TCP handshakes into a listening
/// socket's queue even while its process is stopped, so a waiting visitor is visible here.
pub fn sockets(pids: &[i32]) -> Vec<Sock> {
    let mut out = Vec::new();
    let mut fds = vec![unsafe { zeroed::<libc::proc_fdinfo>() }; 4096];
    let mut buf = [0u8; 2048];
    for &pid in pids {
        let bytes = (fds.len() * size_of::<libc::proc_fdinfo>()) as c_int;
        let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr() as *mut c_void, bytes) };
        let n = n.max(0) as usize / size_of::<libc::proc_fdinfo>();
        for f in fds[..n].iter().filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32) {
            let r = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    f.proc_fd,
                    3, /* PROC_PIDFDSOCKETINFO */
                    buf.as_mut_ptr() as *mut c_void,
                    buf.len() as c_int,
                )
            };
            if r < 212 {
                continue;
            }
            // ponytail: fixed offsets into struct socket_fdinfo (sys/proc_info.h), checked on macOS 27 arm64
            let i16_at = |o: usize| i16::from_ne_bytes([buf[o], buf[o + 1]]);
            let i32_at = |o: usize| i32::from_ne_bytes(buf[o..o + 4].try_into().unwrap());
            if i32_at(180) != libc::IPPROTO_TCP {
                continue;
            }
            let listen = i16_at(188) & 0x2 != 0; // SO_ACCEPTCONN
            let waiting = if listen {
                (i16_at(194).max(0) + i16_at(196).max(0)) as u64
            } else {
                u32::from_ne_bytes(buf[208..212].try_into().unwrap()) as u64
            };
            out.push(Sock { key: (pid as u64) << 32 | f.proc_fd as u32 as u64, listen, waiting });
        }
    }
    out
}

/// kern.memorystatus_vm_pressure_level: 1 normal, 2 warn, 4 critical.
pub fn pressure() -> u32 {
    sysctl_u64("kern.memorystatus_vm_pressure_level").map(|v| v as u32).unwrap_or(1)
}

pub fn mem_bytes() -> u64 {
    sysctl_u64("hw.memsize").unwrap_or(8 << 30)
}

fn sysctl_u64(name: &str) -> Option<u64> {
    let c = std::ffi::CString::new(name).ok()?;
    let mut v = [0u8; 8];
    let mut len = v.len();
    let r = unsafe { libc::sysctlbyname(c.as_ptr(), v.as_mut_ptr() as *mut c_void, &mut len, std::ptr::null_mut(), 0) };
    (r == 0)
        .then(|| if len == 4 { u32::from_ne_bytes(v[..4].try_into().unwrap()) as u64 } else { u64::from_ne_bytes(v) })
}

/// macOS background priority: lowest CPU priority, efficiency cores, throttled disk I/O.
/// Unlike `nice`, it can be undone without root.
pub fn set_bg(pid: i32, on: bool) {
    unsafe {
        libc::setpriority(libc::PRIO_DARWIN_PROCESS, pid as libc::id_t, if on { libc::PRIO_DARWIN_BG } else { 0 })
    };
}

/// The terminal device a process is attached to, for a one-line notice.
pub fn tty_path(pid: i32) -> Option<PathBuf> {
    let b = bsd(pid)?;
    if !has_tty(&b) {
        return None;
    }
    let name = unsafe { libc::devname(b.e_tdev as libc::dev_t, libc::S_IFCHR) };
    if name.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy().into_owned();
    Some(PathBuf::from("/dev").join(name))
}
