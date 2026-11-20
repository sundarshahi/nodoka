use super::{Proc, Sock};
use libc::{c_char, c_int, c_void};
use std::collections::HashMap;
use std::ffi::{CStr, OsStr};
use std::mem::{size_of, zeroed};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::null;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

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
    // a process that renamed itself (process.title) leaves its old argv as zero bytes before the environment
    let env = parts
        .skip_while(|s| s.is_empty())
        .take_while(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
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

/// A process's working folder.
pub fn cwd(pid: i32) -> Option<PathBuf> {
    let mut v: libc::proc_vnodepathinfo = unsafe { zeroed() };
    let sz = size_of::<libc::proc_vnodepathinfo>() as c_int;
    let r = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, &mut v as *mut _ as *mut c_void, sz) };
    let path = unsafe { CStr::from_ptr(v.pvi_cdir.vip_path.as_ptr() as *const c_char) }.to_bytes();
    (r == sz && !path.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(path)))
}

/// Notices file changes under a folder (FSEvents: recursive, no polling). Changes only in
/// dependency, build-output and cache folders are ignored.
pub struct Watch {
    stream: *mut c_void,
    id: usize,
}

/// watch id → (folder, changed since last asked). FSEvents calls back on its own thread.
static WATCHES: Mutex<Option<HashMap<usize, (PathBuf, bool)>>> = Mutex::new(None);

#[repr(C)]
struct StreamContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    describe: *const c_void,
}

type Callback = extern "C" fn(*const c_void, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        a: *const c_void,
        cb: Callback,
        ctx: *const StreamContext,
        paths: *const c_void,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> *mut c_void;
    fn FSEventStreamSetDispatchQueue(s: *mut c_void, q: *mut c_void);
    fn FSEventStreamStart(s: *mut c_void) -> u8;
    fn FSEventStreamStop(s: *mut c_void);
    fn FSEventStreamInvalidate(s: *mut c_void);
    fn FSEventStreamRelease(s: *mut c_void);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithBytes(a: *const c_void, b: *const u8, n: isize, encoding: u32, external: u8) -> *const c_void;
    fn CFArrayCreate(a: *const c_void, v: *const *const c_void, n: isize, cb: *const c_void) -> *const c_void;
    fn CFRelease(p: *const c_void);
    static kCFTypeArrayCallBacks: [usize; 5];
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
}

extern "C" fn on_events(
    _: *const c_void,
    info: *mut c_void,
    n: usize,
    paths: *mut c_void,
    _: *const u32,
    _: *const u64,
) {
    let paths = paths as *const *const c_char;
    let mut w = WATCHES.lock().unwrap_or_else(|e| e.into_inner());
    let Some((root, changed)) = w.as_mut().and_then(|m| m.get_mut(&(info as usize))) else { return };
    for i in 0..n {
        let p = Path::new(OsStr::from_bytes(unsafe { CStr::from_ptr(*paths.add(i)) }.to_bytes()));
        // a path outside the folder (different spelling) counts: better a needless wake than a missed one
        let quiet = p.strip_prefix(&*root).is_ok_and(|rel| rel.iter().any(|c| super::quiet(&c.to_string_lossy())));
        *changed |= !quiet;
    }
}

impl Watch {
    pub fn new(root: &Path) -> Option<Watch> {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        static QUEUE: OnceLock<usize> = OnceLock::new();
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let queue = *QUEUE.get_or_init(|| unsafe { dispatch_queue_create(c"nodoka.watch".as_ptr(), null()) } as usize);
        WATCHES.lock().ok()?.get_or_insert_default().insert(id, (root.to_path_buf(), false));
        let bytes = root.as_os_str().as_bytes();
        let ctx =
            StreamContext { version: 0, info: id as *mut c_void, retain: null(), release: null(), describe: null() };
        let stream = unsafe {
            let path =
                CFStringCreateWithBytes(null(), bytes.as_ptr(), bytes.len() as isize, 0x0800_0100 /* UTF-8 */, 0);
            let list = CFArrayCreate(null(), &path, 1, (&raw const kCFTypeArrayCallBacks).cast());
            // since now, 0.2s latency, NoDefer (first event at once) | FileEvents (one path per file)
            let s = FSEventStreamCreate(null(), on_events, &ctx, list, u64::MAX, 0.2, 0x02 | 0x10);
            CFRelease(list);
            CFRelease(path);
            s
        };
        let w = Watch { stream, id }; // from here on, Drop cleans up
        if stream.is_null() {
            return None;
        }
        unsafe { FSEventStreamSetDispatchQueue(stream, queue as *mut c_void) };
        (unsafe { FSEventStreamStart(stream) } != 0).then_some(w)
    }

    /// Whether anything that matters changed since the last call.
    pub fn changed(&mut self) -> bool {
        let mut w = WATCHES.lock().unwrap_or_else(|e| e.into_inner());
        w.as_mut().and_then(|m| m.get_mut(&self.id)).is_some_and(|e| std::mem::take(&mut e.1))
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        if !self.stream.is_null() {
            unsafe {
                FSEventStreamStop(self.stream);
                FSEventStreamInvalidate(self.stream);
                FSEventStreamRelease(self.stream);
            }
        }
        if let Some(m) = WATCHES.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            m.remove(&self.id);
        }
    }
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
