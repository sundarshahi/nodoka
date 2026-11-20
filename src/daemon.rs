//! The background loop: once a second, find dev work, decide, act, and write a status file.

use crate::classify::{self, Kind};
use crate::config::{self, Config};
use crate::policy::{self, Action, Job, State, Sys};
use crate::sys::{self, Proc};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

static STOP: AtomicBool = AtomicBool::new(false);

/// (pid, start time): one process, even after its pid is reused.
type Key = (i32, u64);

struct Seen {
    kind: Kind,
    label: String,
    production: bool,
}

struct Tracked {
    job: Job,
    tree: Vec<Key>,
    /// The tree minus processes a shell is waiting on; only these are ever stopped.
    stoppable: Vec<Key>,
    cpu_ns: u64,
    /// When nodoka first saw this job. Process age can't be used: a shell that spends a minute
    /// on `npm ci && next build` and then becomes `next build` is a minute old but a new build.
    first_seen: u64,
    last_active: u64,
    mem: u64,
    label: String,
    production: bool,
    listener_checked: u64,
    has_listener: bool,
    /// Open TCP connections last tick, and when that set last changed (a visitor came or went).
    conns: HashSet<u64>,
    last_net: u64,
    /// Connections that already held unread data when the job went to sleep.
    snapshot: HashSet<u64>,
    /// While asleep: watches the project, so saving a file wakes the server (hot reload keeps working).
    watch: Option<sys::Watch>,
    noticed: State,
}

pub struct Options {
    pub dry_run: bool,
    /// Only govern processes started with `NODOKA_TEST=<scope>` (used by the test suite).
    pub scope: Option<String>,
}

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

pub fn run(opts: Options) {
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    let home = config::dir();
    fs::create_dir_all(&home).expect("create the nodoka folder");
    rotate_log(&home);
    let ledger = home.join("paused");
    recover(&ledger);

    let cfg_path = home.join("config");
    let mut cfg = config::load(&cfg_path);
    let mut cfg_mtime = mtime(&cfg_path);
    cfg.warnings.iter().for_each(|w| log(w));
    let uid = sys::uid();
    let me = std::process::id() as i32;
    let cores = sys::cores();
    // first look time + verdict. Unrecognised processes are looked at again during their first 10s:
    // tools rename themselves at startup (npm → `npm ci`, Next → `next-build (v16)`).
    let mut seen: HashMap<(i32, u64, String), (u64, Option<Seen>)> = HashMap::new();
    let mut tracked: HashMap<Key, Tracked> = HashMap::new();
    let mut st = Sys {
        now: sys::now(),
        level: sys::pressure(),
        level_since: sys::now(),
        cpu_busy: false,
        base_cap: cfg.heavy_jobs,
        idle_s: cfg.idle_sleep_s,
        max_wait_s: cfg.max_wait_s,
        last_pressure_act: 0,
    };
    log(&format!(
        "started: {} heavy jobs at once, idle sleep {}, {} worker cap{}{}",
        cfg.heavy_jobs,
        if cfg.idle_sleep_s == 0 { "off".into() } else { format!("after {}s", cfg.idle_sleep_s) },
        cfg.cpus,
        if opts.dry_run { ", dry run" } else { "" },
        opts.scope.as_ref().map(|s| format!(", test scope {s}")).unwrap_or_default()
    ));
    let mut last_status = String::new();
    let mut tick = 0u64;

    while !STOP.load(Ordering::SeqCst) {
        tick += 1;
        if tick % 5 == 0 && mtime(&cfg_path) != cfg_mtime {
            cfg = config::load(&cfg_path);
            cfg_mtime = mtime(&cfg_path);
            cfg.warnings.iter().for_each(|w| log(w));
            (st.base_cap, st.idle_s, st.max_wait_s) = (cfg.heavy_jobs, cfg.idle_sleep_s, cfg.max_wait_s);
            seen.clear(); // custom rules may have changed
            log("settings reloaded");
        }
        if home.join("off").exists() {
            if !tracked.is_empty() {
                log("turned off: resuming everything");
                recover(&ledger);
                tracked.clear();
            }
            write_status(&home, &mut last_status, "nodoka is off. Run `nodoka on` to turn it back on.\n");
            sleep_tick();
            continue;
        }

        let t = sys::now();
        let procs = sys::procs(uid);
        let by_pid: HashMap<i32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
        let mut kids: HashMap<i32, Vec<i32>> = HashMap::new();
        for p in &procs {
            kids.entry(p.ppid).or_default().push(p.pid);
        }
        let alive: HashSet<Key> = procs.iter().map(|p| (p.pid, p.id)).collect();
        // comm is part of the key: it changes on exec, so a shell that becomes `next build` is looked at again
        seen.retain(|(pid, id, _), _| alive.contains(&(*pid, *id)));

        let mut classified: HashMap<i32, Kind> = HashMap::new();
        for p in procs.iter().filter(|p| p.pid != me) {
            let entry = seen
                .entry((p.pid, p.id, p.comm.clone()))
                .or_insert_with(|| (t, look(p.pid, &cfg, opts.scope.as_deref())));
            if entry.1.is_none() && t.saturating_sub(entry.0) < 10 && entry.0 != t {
                entry.1 = look(p.pid, &cfg, opts.scope.as_deref());
            }
            if let Some(s) = &entry.1 {
                classified.insert(p.pid, s.kind);
            }
        }

        // A job is a classified process with no classified ancestor, plus all its descendants.
        let mut roots_now = HashSet::new();
        for (&pid, &kind) in &classified {
            let mut a = by_pid[&pid].ppid;
            let mut nested = false;
            for _ in 0..64 {
                if a <= 1 {
                    break;
                }
                if classified.contains_key(&a) {
                    nested = true;
                    break;
                }
                a = by_pid.get(&a).map_or(0, |p| p.ppid);
            }
            if nested {
                continue;
            }
            let root = by_pid[&pid];
            let key = (pid, root.id);
            roots_now.insert(key);
            let mut tree = vec![pid];
            let mut i = 0;
            while i < tree.len() {
                tree.extend(kids.get(&tree[i]).into_iter().flatten().copied());
                i += 1;
            }
            let protected = |q: i32| {
                by_pid
                    .get(&q)
                    .and_then(|p| by_pid.get(&p.ppid))
                    .is_some_and(|parent| parent.tty && sys::is_shell(&parent.comm))
            };
            let stoppable: Vec<Key> = tree.iter().filter(|&&q| !protected(q)).map(|&q| (q, by_pid[&q].id)).collect();
            let tree: Vec<Key> = tree.iter().map(|&q| (q, by_pid[&q].id)).collect();
            let (cpu, mem) =
                tree.iter().filter_map(|&(q, _)| sys::cpu_mem(q)).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
            let s = seen.get(&(pid, root.id, root.comm.clone())).and_then(|s| s.1.as_ref());
            let tr = tracked.entry(key).or_insert_with(|| Tracked {
                // jobs already running when nodoka starts are left as they are
                job: Job { admitted: tick == 1, ..Job::new(kind, t) },
                tree: vec![],
                stoppable: vec![],
                cpu_ns: cpu,
                first_seen: t,
                last_active: t,
                mem: 0,
                label: s.map(|s| s.label.clone()).unwrap_or_default(),
                production: s.is_some_and(|s| s.production),
                listener_checked: 0,
                has_listener: false,
                conns: HashSet::new(),
                last_net: t,
                snapshot: HashSet::new(),
                watch: None,
                noticed: State::Running,
            });
            (tr.mem, tr.tree, tr.stoppable) = (mem, tree, stoppable);
            let running = tr.job.state == State::Running;
            // Is it a server? Look now and then (it may bind its port a few seconds after starting).
            if running && st.idle_s > 0 && !tr.has_listener && t.saturating_sub(tr.listener_checked) >= 20 {
                tr.listener_checked = t;
                tr.has_listener = sys::sockets(&pids(&tr.tree)).iter().any(|s| s.listen);
            }
            // Busy = used more than ~2% of a core since the last tick, or a visitor came or went.
            let mut busy = cpu.saturating_sub(tr.cpu_ns) > 20_000_000;
            if running && tr.has_listener {
                let socks = sys::sockets(&pids(&tr.tree));
                let conns: HashSet<u64> = socks.iter().filter(|s| !s.listen).map(|s| s.key).collect();
                if conns != tr.conns || socks.iter().any(|s| s.waiting > 0) {
                    tr.last_net = t;
                    busy = true;
                }
                tr.conns = conns;
            }
            if busy {
                tr.last_active = t;
            }
            tr.cpu_ns = cpu;
            let edited = tr.watch.as_mut().is_some_and(|w| w.changed());
            let j = &mut tr.job;
            j.age_s = t.saturating_sub(tr.first_seen);
            j.terminal = root.tty;
            j.pausable = !tr.stoppable.is_empty();
            // A server nobody has visited for 3× the idle time counts as idle even if it keeps the
            // CPU busy (polling file watchers, runaway loops).
            let unvisited = t.saturating_sub(tr.last_net);
            j.idle_for_s = if tr.has_listener && st.idle_s > 0 && unvisited >= 3 * st.idle_s {
                unvisited
            } else {
                t.saturating_sub(tr.last_active)
            };
            j.active = t.saturating_sub(tr.last_active) < 5;
            j.sleepable = tr.has_listener && !tr.production;
            j.wake = j.state == State::IdlePaused
                && (edited
                    || sys::sockets(&pids(&tr.tree))
                        .iter()
                        .any(|s| s.waiting > 0 && (s.listen || !tr.snapshot.contains(&s.key))));
        }

        // Jobs that ended or changed: never leave anything we stopped behind.
        let mut ledger_dirty = false;
        let gone: Vec<Key> = tracked.keys().filter(|k| !roots_now.contains(k)).copied().collect();
        for k in gone {
            let tr = tracked.remove(&k).unwrap();
            if tr.job.state != State::Running || tr.job.bg {
                ledger_dirty = true;
                for &(q, _) in tr.stoppable.iter().filter(|q| !opts.dry_run && alive.contains(q)) {
                    sys::set_bg(q, false);
                    sys::signal(q, false);
                }
            }
        }

        let level = sys::pressure();
        if level != st.level {
            log(&format!("memory pressure {} → {}", level_name(st.level), level_name(level)));
            (st.level, st.level_since) = (level, t);
        }
        // Node tools read this at start: short of memory → fewest workers. Rewritten every few
        // seconds; the preload ignores it once it's 30s old (helper stopped).
        if !opts.dry_run && (tick % 5 == 1 || st.level_since == t) {
            fs::write(home.join("pressure"), level.to_string()).ok();
        }
        st.now = t;
        st.cpu_busy = sys::load1() > cores as f64;
        let keys: Vec<Key> = tracked.keys().copied().collect();
        let mut jobs: Vec<Job> = keys.iter().map(|k| tracked[k].job.clone()).collect();
        let actions = policy::decide(&mut jobs, &mut st);
        for (k, j) in keys.iter().zip(jobs) {
            tracked.get_mut(k).unwrap().job = j;
        }
        if !actions.is_empty() || ledger_dirty {
            write_ledger(&ledger, &tracked); // before any signal, so a crash can always be undone
        }
        for a in &actions {
            let (Action::Pause(i) | Action::Resume(i) | Action::Bg(i, _)) = *a;
            let tr = tracked.get_mut(&keys[i]).unwrap();
            if let Action::Pause(_) = a {
                if tr.job.state == State::IdlePaused {
                    let socks = sys::sockets(&pids(&tr.tree));
                    tr.snapshot = socks.iter().filter(|s| !s.listen && s.waiting > 0).map(|s| s.key).collect();
                    tr.watch =
                        sys::cwd(keys[i].0).and_then(|c| sys::project_root(&c)).and_then(|r| sys::Watch::new(&r));
                }
            }
            if let Action::Resume(_) = a {
                (tr.last_active, tr.last_net) = (t, t); // a woken server gets a full idle period again
                tr.conns.clear();
                tr.watch = None;
            }
            let what = match a {
                Action::Pause(_) => "pause",
                Action::Resume(_) => "resume",
                Action::Bg(_, true) => "lower priority",
                Action::Bg(_, false) => "normal priority",
            };
            log(&format!("{what:<15} {} pid {} — {} ({})", tr.job.kind.name(), keys[i].0, tr.label, why(&tr.job, &st)));
            if opts.dry_run {
                continue;
            }
            for &(q, _) in &tr.stoppable {
                match *a {
                    Action::Pause(_) => sys::signal(q, true),
                    Action::Resume(_) => sys::signal(q, false),
                    Action::Bg(_, on) => sys::set_bg(q, on),
                }
            }
            if tr.job.terminal && tr.noticed != tr.job.state {
                notice(keys[i].0, tr, &st);
            }
        }
        // children forked into a paused tree since the last tick are paused too
        if !opts.dry_run {
            for tr in tracked.values().filter(|tr| tr.job.state != State::Running) {
                for &(q, _) in &tr.stoppable {
                    if by_pid.get(&q).is_some_and(|p| !p.stopped) {
                        sys::signal(q, true);
                    }
                }
            }
        }

        let status = render(&tracked, &st, cores);
        write_status(&home, &mut last_status, &status);
        sleep_tick();
    }
    log("stopping: resuming everything");
    recover(&ledger);
    fs::remove_file(home.join("pressure")).ok();
}

fn sleep_tick() {
    std::thread::sleep(Duration::from_secs(1));
}

fn pids(keys: &[Key]) -> Vec<i32> {
    keys.iter().map(|k| k.0).collect()
}

fn look(pid: i32, cfg: &Config, scope: Option<&str>) -> Option<Seen> {
    let (argv, env) = sys::args_env(pid)?;
    let var = |name: &str| env.iter().find_map(|e| e.strip_prefix(name)?.strip_prefix('='));
    let argv = classify::unwipe(argv, var("npm_lifecycle_script"));
    let kind = classify::classify(&argv, &cfg.custom)?;
    if var("NODOKA_SKIP") == Some("1") {
        return None;
    }
    if scope.is_some() && var("NODOKA_TEST") != scope {
        return None;
    }
    let production = [
        ("NODE_ENV", "production"),
        ("RAILS_ENV", "production"),
        ("RACK_ENV", "production"),
        ("APP_ENV", "production"),
        ("MIX_ENV", "prod"),
        ("ASPNETCORE_ENVIRONMENT", "Production"),
    ]
    .iter()
    .any(|(k, v)| var(k) == Some(v));
    Some(Seen { kind, label: label(&argv), production })
}

/// A short, readable command line for status output.
pub fn label(argv: &[String]) -> String {
    let argv = classify::untitle(argv);
    let words: Vec<&str> = argv.iter().map(String::as_str).collect();
    let skip = usize::from(words.first().is_some_and(|w| {
        matches!(classify::stem(w).as_str(), "node" | "ruby" | "python" | "php" | "java" | "beam" | "dart")
    }));
    let s = words
        .iter()
        .skip(skip)
        .enumerate()
        .map(|(i, w)| if i == 0 { w.rsplit('/').next().unwrap_or(w) } else { w })
        .collect::<Vec<_>>()
        .join(" ");
    if s.chars().count() > 60 { s.chars().take(59).collect::<String>() + "…" } else { s }
}

fn level_name(l: u32) -> &'static str {
    match l {
        1 => "normal",
        2 => "warn",
        _ => "critical",
    }
}

fn why(j: &Job, st: &Sys) -> String {
    match j.state {
        State::Queued => "enough heavy jobs are running; it starts when one finishes".into(),
        State::PressurePaused => "memory is critically low".into(),
        State::IdlePaused => "idle; the next request or saved file wakes it".into(),
        State::Running if j.bg && st.cpu_busy => "all cores are busy".into(),
        State::Running if j.bg => "waited the maximum time, started anyway".into(),
        State::Running => "ok".into(),
    }
}

/// One line in the terminal a job runs in, so a person watching knows why it stopped printing.
fn notice(pid: i32, tr: &mut Tracked, st: &Sys) {
    let msg = match tr.job.state {
        State::Queued => format!(
            "[nodoka] `{}` is waiting for a free slot. It starts on its own within {}s. See `nodoka status`.",
            tr.label, st.max_wait_s
        ),
        State::PressurePaused => format!(
            "[nodoka] paused `{}`: memory is critically low. It continues on its own as memory frees up.",
            tr.label
        ),
        State::Running if tr.noticed != State::Running => format!("[nodoka] `{}` continues.", tr.label),
        _ => return,
    };
    tr.noticed = tr.job.state;
    if let Some(path) = sys::tty_path(pid) {
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(mut f) = fs::OpenOptions::new().write(true).custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK).open(path)
        {
            let _ = write!(f, "\r\n{msg}\r\n");
        }
    }
}

fn render(tracked: &HashMap<Key, Tracked>, st: &Sys, cores: usize) -> String {
    let mut rows: Vec<_> = tracked.iter().collect();
    rows.sort_by_key(|(k, tr)| (std::cmp::Reverse(tr.job.age_s), k.0));
    let mut s = format!(
        "memory pressure: {}   load: {:.1} on {} cores   builds/tests/installs at once: {}\n",
        level_name(st.level),
        sys::load1(),
        cores,
        st.cap()
    );
    if rows.is_empty() {
        s += "no dev jobs right now\n";
    }
    for ((pid, _), tr) in rows {
        let state = match tr.job.state {
            State::Running if tr.job.bg => "running, low priority",
            State::Running => "running",
            State::Queued => "waiting for a slot",
            State::PressurePaused => "paused: low memory",
            State::IdlePaused => "asleep: idle",
        };
        let mem = if tr.mem >= 1 << 30 {
            format!("{:.1} GB", tr.mem as f64 / (1u64 << 30) as f64)
        } else {
            format!("{} MB", tr.mem >> 20)
        };
        s += &format!("{pid:>7}  {:<8} {:<22} {mem:>7}  {}\n", tr.job.kind.name(), state, tr.label);
    }
    s
}

fn write_status(home: &Path, last: &mut String, s: &str) {
    let tmp = home.join("status.tmp");
    // rewrite at least every 5s so `nodoka status` can tell the helper is alive
    let stale = mtime(&home.join("status")).and_then(|m| m.elapsed().ok()).is_none_or(|e| e > Duration::from_secs(5));
    if s != last || stale {
        if fs::write(&tmp, s).is_ok() {
            fs::rename(&tmp, home.join("status")).ok();
        }
        *last = s.to_string();
    }
}

fn write_ledger(path: &Path, tracked: &HashMap<Key, Tracked>) {
    let lines: String = tracked
        .values()
        .filter(|tr| tr.job.state != State::Running || tr.job.bg)
        .flat_map(|tr| tr.stoppable.iter().map(|(p, id)| format!("{p} {id}\n")))
        .collect();
    fs::write(path, lines).ok();
}

/// Resume and restore priority for everything listed in the ledger (crash-safe undo).
pub fn recover(path: &Path) {
    let Ok(text) = fs::read_to_string(path) else { return };
    let alive: HashSet<Key> = sys::procs(sys::uid()).iter().map(|p| (p.pid, p.id)).collect();
    for line in text.lines() {
        let mut it = line.split(' ').filter_map(|x| x.parse::<u64>().ok());
        if let (Some(p), Some(id)) = (it.next(), it.next()) {
            if alive.contains(&(p as i32, id)) {
                sys::set_bg(p as i32, false);
                sys::signal(p as i32, false);
            }
        }
    }
    fs::remove_file(path).ok();
}

fn mtime(p: &Path) -> Option<SystemTime> {
    fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn log_path() -> PathBuf {
    config::dir().join("log")
}

fn rotate_log(home: &Path) {
    let p = home.join("log");
    if fs::metadata(&p).is_ok_and(|m| m.len() > 1 << 20) {
        fs::rename(&p, home.join("log.1")).ok();
    }
}

pub fn log(msg: &str) {
    let line = format!("{} {msg}\n", clock());
    if unsafe { libc::isatty(1) } == 1 {
        print!("{line}");
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = f.write_all(line.as_bytes());
    }
}

fn clock() -> String {
    let t = sys::now() as _;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}
