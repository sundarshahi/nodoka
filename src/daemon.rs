//! The background loop: once a second, find dev work, decide, act, and write a status file.

use crate::agents;
use crate::classify::{self, Kind};
use crate::config::{self, Config};
use crate::policy::{self, Action, Job, State, Sys};
use crate::sys::{self, Proc};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

static STOP: AtomicBool = AtomicBool::new(false);

/// (pid, start time): one process, even after its pid is reused.
type Key = (i32, u64);

struct Seen {
    kind: Kind,
    label: String,
    production: bool,
    /// A headless browser: waits on its agent, never stopped for being idle.
    browser: bool,
    /// No rule matched: found by how much it uses (see `HEAVY_WINDOW_S`).
    found: bool,
}

/// A program no rule knows (a Java map-tile build, a data script) started by a coding agent becomes
/// a build once it has kept more than 1.5 cores busy, or a whole core while holding 1 GB, for this
/// long: from then on it is slowed, paused and resumed like one.
const HEAVY_WINDOW_S: u64 = 30;

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
    /// CPU time used since the last tick, and whether it's held to the agent CPU budget right now.
    used: u64,
    sliced: bool,
    /// Folder and command, for remembering how much memory this job needed; its peak so far.
    place: Option<(PathBuf, String)>,
    peak: u64,
    /// Logged once: a process of this paused job was found running.
    resumed_seen: bool,
}

pub struct Options {
    pub dry_run: bool,
    /// Only govern processes started with `NODOKA_TEST=<scope>` (used by the test suite).
    pub scope: Option<String>,
}

#[cfg(unix)]
extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

pub fn run(opts: Options) {
    // first thing: a test helper whose test run was killed stops too (and resumes what it paused)
    // instead of running forever. Parent 1 means it was orphaned before this line ran.
    #[cfg(unix)]
    let orphaned = {
        let parent = std::os::unix::process::parent_id();
        unsafe {
            libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        }
        let test = opts.scope.is_some();
        move || test && (std::os::unix::process::parent_id() != parent || parent == 1)
    };
    // ponytail: on Windows a stopped helper leaves paused jobs to the next start, which resumes them
    #[cfg(windows)]
    let orphaned = || false;
    let home = config::dir();
    fs::create_dir_all(&home).expect("create the nodoka folder");
    rotate_log(&home);
    let ledger = home.join("paused");
    // builds paused for memory before a restart stay paused (an update resumed three type checks
    // holding 12.6 GB at once); everything else in the ledger is resumed
    let mut adopted = adopt(&ledger);

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
    // unknown programs under agents: (window start, CPU ns then)
    let mut heavy: HashMap<Key, (u64, u64)> = HashMap::new();
    // test runs share a loaded machine at low priority: they pin a shorter window and fewer cores
    let pin = |var: &str| opts.scope.as_ref().and_then(|_| std::env::var(var).ok()?.parse::<f64>().ok());
    let heavy_window = pin("NODOKA_HEAVY_WINDOW").map_or(HEAVY_WINDOW_S, |w| w as u64);
    let heavy_cores = pin("NODOKA_HEAVY_CORES").unwrap_or(1.5);
    let mut tracked: HashMap<Key, Tracked> = HashMap::new();
    // tests pin it: the real machine's pressure (a busy dev Mac swaps) must not decide their outcome
    let pressure = || {
        opts.scope
            .as_ref()
            .and_then(|_| std::env::var("NODOKA_PRESSURE").ok()?.parse().ok())
            .unwrap_or_else(sys::pressure)
    };
    let avail = || {
        opts.scope
            .as_ref()
            .and_then(|_| std::env::var("NODOKA_AVAIL_MB").ok()?.parse::<u64>().ok().map(|mb| mb << 20))
            .unwrap_or_else(sys::avail_bytes)
    };
    let mut leftovers: HashMap<Key, Leftover> = HashMap::new();
    // agent sessions by process; None = not ours to judge (outside the test scope)
    // ponytail: a non-Claude session's idle time counts from when nodoka first saw it (a restart waits again)
    let mut sessions: HashMap<Key, Option<agents::Session>> = HashMap::new();
    // kept in a file: a restart (every update, every login) forgot them, and a 3.9 GB type check
    // started at critical memory as if it were new
    let peaks_path = home.join("peaks");
    let mut peaks = load_peaks(&peaks_path);
    let mut apps: Vec<(String, u64)> = vec![];
    let mut last_notice = 0;
    let mut short_since = 0;
    let mut st = Sys {
        now: sys::now(),
        level: pressure(),
        level_since: sys::now(),
        cpu_busy: false,
        base_cap: cfg.heavy_jobs,
        idle_s: cfg.idle_sleep_s,
        max_wait_s: cfg.max_wait_s,
        last_pressure_act: 0,
        avail: 0,
        pause_for_memory: cfg.pause_for_memory,
        freezing: false,
        stop_idle_tests_s: cfg.stop_idle_tests_s,
        stop_long_tests_s: cfg.stop_long_tests_s,
        last_critical: 0,
    };
    log(&format!(
        "started: {} heavy jobs at once, idle sleep {}, {} worker cap{}{}",
        cfg.heavy_jobs,
        if cfg.idle_sleep_s == 0 { "off".into() } else { format!("after {}s", cfg.idle_sleep_s) },
        cfg.cpus,
        if opts.dry_run { ", dry run" } else { "" },
        opts.scope.as_ref().map(|s| format!(", test scope {s}")).unwrap_or_default()
    ));
    // disk cleaning: its own thread, hourly (deleting a 1 GB node_modules takes seconds), every 5
    // minutes while the disk is nearly full; it reads the settings each pass so edits apply without a restart
    if opts.scope.is_none() {
        let (path, dry_run) = (cfg_path.clone(), opts.dry_run);
        std::thread::spawn(move || {
            let (mut last, mut was_low) = (0u64, false);
            loop {
                let c = config::load(&path);
                let free = c.clean_dirs.first().and_then(|d| sys::disk_free(d));
                let low = free.is_some_and(|f| f < crate::clean::LOW_DISK);
                if low && !was_low && !c.clean_dirs.is_empty() {
                    log(&format!(
                        "disk low        {:.1} GB free: caches go from worktrees idle an hour",
                        free.unwrap_or(0) as f64 / (1u64 << 30) as f64
                    ));
                }
                was_low = low;
                if !c.clean_dirs.is_empty() && (low || sys::now().saturating_sub(last) >= 3600) {
                    last = sys::now();
                    let rules = crate::clean::Rules {
                        dirs: c.clean_dirs,
                        caches_after_s: crate::clean::caches_after(c.clean_caches_after_s, free),
                        worktrees_after_s: c.remove_worktrees_after_s,
                        dry_run,
                    };
                    crate::clean::pass(&rules, &crate::clean::in_use(), sys::now()).iter().for_each(|l| log(l));
                }
                std::thread::sleep(Duration::from_secs(300));
            }
        });
    }
    let mut last_status = String::new();
    let mut tick = 0u64;
    let (mut duty, mut last_tick) = (1.0f64, Instant::now());

    while !STOP.load(Ordering::SeqCst) && !orphaned() {
        tick += 1;
        if tick % 5 == 0 && mtime(&cfg_path) != cfg_mtime {
            cfg = config::load(&cfg_path);
            cfg_mtime = mtime(&cfg_path);
            cfg.warnings.iter().for_each(|w| log(w));
            (st.base_cap, st.idle_s, st.max_wait_s) = (cfg.heavy_jobs, cfg.idle_sleep_s, cfg.max_wait_s);
            st.pause_for_memory = cfg.pause_for_memory;
            st.stop_idle_tests_s = cfg.stop_idle_tests_s;
            st.stop_long_tests_s = cfg.stop_long_tests_s;
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

        // Heavy work no rule knows, started by an agent (a terminal's is the user's own).
        if tick % 5 == 0 {
            heavy.retain(|k, _| alive.contains(k));
            let under_agent = |p: &Proc| {
                let mut a = p.ppid;
                for _ in 0..64 {
                    let Some(q) = by_pid.get(&a) else { return false };
                    if classified.contains_key(&a) {
                        return false; // already part of a job
                    }
                    if agents::NAMES.contains(&q.comm.as_str()) {
                        return true;
                    }
                    a = q.ppid;
                }
                false
            };
            let mut found = vec![];
            for p in procs.iter().filter(|p| {
                !p.tty
                    && p.pid != me
                    && !classified.contains_key(&p.pid)
                    && !agents::NAMES.contains(&p.comm.as_str())
                    && !sys::is_shell(&p.comm)
            }) {
                if !under_agent(p) {
                    continue;
                }
                let Some((cpu, mem)) = sys::cpu_mem(p.pid) else { continue };
                let (since, cpu0) = *heavy.entry((p.pid, p.id)).or_insert((t, cpu));
                let span = t.saturating_sub(since);
                if span < heavy_window {
                    continue;
                }
                let cores = cpu.saturating_sub(cpu0) as f64 / (span as f64 * 1e9);
                if cores >= heavy_cores || cores >= 1.0 && mem >= 1 << 30 {
                    found.push((p, cores, mem));
                } else {
                    heavy.insert((p.pid, p.id), (t, cpu)); // a new window
                }
            }
            for (p, cores, mem) in found {
                let Some((argv, env)) = sys::args_env(p.pid) else { continue };
                let ours = opts
                    .scope
                    .as_deref()
                    .is_none_or(|sc| env.iter().any(|e| e.strip_prefix("NODOKA_TEST=") == Some(sc)));
                if !ours {
                    continue;
                }
                let label = label(&argv);
                log(&format!(
                    "found           build pid {} — {label} (no rule for it; an agent started it and it kept {cores:.1} cores busy, holding {})",
                    p.pid,
                    size(mem)
                ));
                let s = Seen { kind: Kind::Build, label, production: false, browser: false, found: true };
                seen.insert((p.pid, p.id, p.comm.clone()), (t, Some(s)));
                classified.insert(p.pid, Kind::Build);
                heavy.remove(&(p.pid, p.id));
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
            let tree = descendants(pid, &kids);
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
            let paused_since = tree.iter().find_map(|k| adopted.get(k)).copied();
            let tr = tracked.entry(key).or_insert_with(|| {
                let label = s.map(|s| s.label.clone()).unwrap_or_default();
                let place = kind.heavy().then(|| sys::cwd(pid)).flatten().map(|c| (c, label.clone()));
                let expect = place.as_ref().map_or(0, |p| expected(&peaks, p));
                Tracked {
                    // jobs already running when nodoka starts are left as they are
                    job: Job {
                        // found by its load: it's been running a while, so it isn't queued like a new start
                        admitted: tick == 1 || s.is_some_and(|s| s.found),
                        state: if paused_since.is_some() { State::PressurePaused } else { State::Running },
                        state_since: paused_since.unwrap_or(t),
                        expect,
                        runner: kind == Kind::Test && !s.is_some_and(|s| s.browser),
                        keep: s.is_some_and(|s| s.found),
                        ..Job::new(kind, t)
                    },
                    tree: vec![],
                    stoppable: vec![],
                    cpu_ns: cpu,
                    // already running when nodoka (re)started: its real age, or every update made a
                    // 50-minute test run look new
                    first_seen: if tick == 1 || s.is_some_and(|s| s.found) { sys::started(root).min(t) } else { t },
                    last_active: t,
                    mem: 0,
                    label,
                    production: s.is_some_and(|s| s.production),
                    listener_checked: 0,
                    has_listener: false,
                    conns: HashSet::new(),
                    last_net: t,
                    snapshot: HashSet::new(),
                    watch: None,
                    noticed: State::Running,
                    used: 0,
                    sliced: false,
                    place,
                    peak: 0,
                    resumed_seen: false,
                }
            });
            (tr.mem, tr.tree, tr.stoppable) = (mem, tree, stoppable);
            tr.peak = tr.peak.max(mem);
            let running = tr.job.state == State::Running;
            // Is it a server? Every second while it's new (it may bind its port a few seconds after
            // starting; a slow machine missed it at the first look), then now and then.
            let recheck = t.saturating_sub(tr.first_seen) < 10 || t.saturating_sub(tr.listener_checked) >= 20;
            if running && st.idle_s > 0 && !tr.has_listener && recheck {
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
            tr.used = cpu.saturating_sub(tr.cpu_ns);
            tr.cpu_ns = cpu;
            let edited = tr.watch.as_mut().is_some_and(|w| w.changed());
            let j = &mut tr.job;
            j.age_s = t.saturating_sub(tr.first_seen);
            j.terminal = root.tty;
            j.mem = tr.mem;
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
            // remember what it needed, for the next run of the same command here (one that never
            // got to run would teach a wrong, tiny number)
            if let Some(place) = tr.place.filter(|_| tr.job.admitted && tr.peak > 0) {
                peaks.insert(place, tr.peak);
                save_peaks(&peaks_path, &peaks);
            }
            if tr.job.state != State::Running || tr.job.bg || tr.job.slow {
                ledger_dirty = true;
                for &(q, _) in tr.stoppable.iter().filter(|q| !opts.dry_run && alive.contains(q)) {
                    sys::set_bg(q, false);
                    sys::set_slow(q, false);
                    sys::signal(q, false);
                }
            }
        }

        let level = pressure();
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
        st.avail = avail();
        st.freezing = match opts.scope.as_ref().and_then(|_| std::env::var("NODOKA_FREEZING").ok()) {
            Some(v) => v == "1",
            None => sys::freezing(),
        };
        // what else fills memory, for `nodoka status` (it leaves the user's apps alone)
        if tick % 10 == 1 {
            let in_jobs: HashSet<i32> = tracked.values().flat_map(|tr| tr.tree.iter().map(|k| k.0)).collect();
            apps = if st.level >= 2 { biggest_apps(&procs, &by_pid, &in_jobs) } else { vec![] };
        }
        st.cpu_busy = sys::load1() > cores as f64;
        let keys: Vec<Key> = tracked.keys().copied().collect();
        let mut jobs: Vec<Job> = keys.iter().map(|k| tracked[k].job.clone()).collect();
        let actions = policy::decide(&mut jobs, &mut st);
        for (k, j) in keys.iter().zip(jobs) {
            tracked.get_mut(k).unwrap().job = j;
        }
        // adopted processes that turned out not to be a job (settings changed): never left stopped
        if tick == 10 {
            let in_jobs: HashSet<Key> = tracked.values().flat_map(|tr| tr.tree.iter().copied()).collect();
            for (&(q, _), _) in adopted.iter().filter(|(k, _)| alive.contains(k) && !in_jobs.contains(k)) {
                sys::signal(q, false);
            }
            adopted.clear();
        }
        if !actions.is_empty() || ledger_dirty || tick == 1 {
            write_ledger(&ledger, &tracked); // before any signal, so a crash can always be undone
        }
        let mut paused = vec![];
        for a in &actions {
            let (Action::Pause(i) | Action::Resume(i) | Action::Bg(i, _) | Action::Slow(i, _) | Action::Stop(i)) = *a;
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
                Action::Slow(_, true) => "slow lane",
                Action::Slow(_, false) => "full speed",
                Action::Stop(_) => "stop",
            };
            log(&format!("{what:<15} {} pid {} — {} ({})", tr.job.kind.name(), keys[i].0, tr.label, why(&tr.job, &st)));
            if opts.dry_run {
                continue;
            }
            for &(q, _) in &tr.stoppable {
                match *a {
                    Action::Pause(_) => {
                        if let Some(e) = sys::signal(q, true) {
                            log(&format!("pause failed    pid {q} — {} ({e})", tr.label));
                        }
                        paused.push(q);
                    }
                    Action::Resume(_) => _ = sys::signal(q, false),
                    Action::Bg(_, on) => sys::set_bg(q, on),
                    Action::Slow(_, on) => sys::set_slow(q, on),
                    Action::Stop(_) => {
                        sys::signal(q, false);
                        sys::terminate(q);
                    }
                }
            }
            if tr.job.terminal && tr.noticed != tr.job.state {
                notice(keys[i].0, tr, &st);
            }
        }
        if !paused.is_empty() {
            sys::settle_stops(&paused);
        }
        // children forked into a paused tree since the last tick are paused too (and into a slow one, slowed)
        if !opts.dry_run {
            for tr in tracked.values().filter(|tr| tr.job.slow) {
                tr.stoppable.iter().for_each(|&(q, _)| sys::set_slow(q, true));
            }
            for tr in tracked.values_mut().filter(|tr| tr.job.state != State::Running) {
                for &(q, _) in &tr.stoppable {
                    if by_pid.get(&q).is_some_and(|p| !p.stopped) {
                        let err = sys::signal(q, true);
                        // a running process in a tree paused over a second ago: something else resumed it
                        if t.saturating_sub(tr.job.state_since) >= 2 && !tr.resumed_seen {
                            tr.resumed_seen = true;
                            let why = err.map_or("stopped it again".into(), |e| format!("can't stop it: {e}"));
                            // CPU used tells a real resume from a stop still pending (a process waiting on a page-in
                            // can't stop until the read ends, and doesn't run meanwhile)
                            log(&format!(
                                "found running   pid {q} in paused job {} — {why} (the job used {} ms CPU in the last second)",
                                tr.label,
                                tr.used / 1_000_000
                            ));
                        }
                    }
                }
            }
        }

        // Leftovers: a process whose parent is gone (killed agent, interrupted test run) that keeps a
        // core busy for long gets lower priority once and is listed, so it can't quietly heat the machine.
        if cfg.leftover_busy_s > 0 && tick % 5 == 0 {
            let in_jobs: HashSet<i32> = tracked.values().flat_map(|tr| pids(&tr.tree)).collect();
            leftovers.retain(|k, _| alive.contains(k));
            let orphan = |p: &Proc| p.ppid == 1 || by_pid.get(&p.ppid).is_some_and(|q| q.comm == "systemd");
            for p in procs.iter().filter(|p| p.pid != me && !p.stopped && !in_jobs.contains(&p.pid) && orphan(p)) {
                let Some((cpu, _)) = sys::cpu_mem(p.pid) else { continue };
                let e = leftovers.entry((p.pid, p.id)).or_insert(Leftover { busy_since: t, cpu, at: t, label: None });
                // under ~25% of one core since the last look (a runaway on a crowded machine still gets more): start counting again
                if cpu.saturating_sub(e.cpu) < t.saturating_sub(e.at) * 250_000_000 || e.at == t {
                    e.busy_since = t;
                }
                (e.cpu, e.at) = (cpu, t);
                if e.label.is_none() && t.saturating_sub(e.busy_since) >= cfg.leftover_busy_s {
                    let label = leftover_label(p.pid, opts.scope.as_deref()).unwrap_or_default();
                    if !label.is_empty() {
                        let mins = t.saturating_sub(e.busy_since).div_ceil(60);
                        log(&format!(
                            "lower priority  leftover pid {} — {label} (busy {mins} min with no parent; `kill {}` stops it)",
                            p.pid, p.pid
                        ));
                        if !opts.dry_run {
                            sys::set_bg(p.pid, true);
                        }
                    }
                    e.label = Some(label);
                }
            }
        }

        // Agent sessions left open: idle ones are listed, and closed while memory is short if that's on.
        if tick % 5 == 0 {
            sessions.retain(|k, _| alive.contains(k));
            let is_agent = |p: &Proc| agents::NAMES.contains(&p.comm.as_str());
            for p in procs.iter().filter(|p| is_agent(p)) {
                let (cpu, mem) = descendants(p.pid, &kids)
                    .into_iter()
                    .filter_map(sys::cpu_mem)
                    .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
                // a tool call starts a direct child; a wait loop's `sleep`s are deeper
                let tree: HashSet<i32> = kids.get(&p.pid).into_iter().flatten().copied().collect();
                let entry = sessions.entry((p.pid, p.id)).or_insert_with(|| {
                    let env = sys::args_env(p.pid).map(|(_, env)| env).unwrap_or_default();
                    let ours = opts
                        .scope
                        .as_deref()
                        .is_none_or(|scope| env.iter().any(|e| e.strip_prefix("NODOKA_TEST=") == Some(scope)));
                    let last = agents::last_chat_write(p.pid, &env);
                    ours.then(|| agents::Session::new(cpu, tree.clone(), t, p.tty, last))
                });
                let Some(s) = entry else { continue };
                s.sample(cpu, tree, t);
                s.mem = mem;
                if s.should_close(t, cfg.close_idle_agents_s, st.level) {
                    s.asked = true;
                    log(&format!(
                        "close           agent session pid {} — {} (idle {}, holding {}; memory is short; its chat is saved)",
                        p.pid,
                        p.comm,
                        span(s.idle_for(t)),
                        size(mem)
                    ));
                    if !opts.dry_run {
                        sys::terminate(p.pid);
                    }
                }
            }
        }

        // CPU budget for agent work: measure what it used, then let it run only part of the next second.
        let dt = last_tick.elapsed().as_secs_f64().max(0.2);
        last_tick = Instant::now();
        let share: HashSet<Key> =
            tracked.iter().filter(|(_, tr)| policy::shares_budget(&tr.job)).map(|(k, _)| *k).collect();
        let used = share.iter().map(|k| tracked[k].used).sum::<u64>() as f64 / 1e9 / dt;
        let budget = cores as f64 * cfg.agent_cpu_percent as f64 / 100.0;
        let new_duty =
            if cfg.agent_cpu_percent >= 100 || share.is_empty() { 1.0 } else { policy::duty(used, duty, budget) };
        if (new_duty < 1.0) != (duty < 1.0) {
            log(&if new_duty < 1.0 {
                format!("share cpu       agent work used {used:.1} cores: held to {budget:.1} of {cores}")
            } else {
                "share cpu       agent work back to full speed".into()
            });
        }
        duty = new_duty;
        let mut slices_changed = false;
        for (k, tr) in tracked.iter_mut() {
            let sliced = duty < 1.0 && share.contains(k);
            slices_changed |= tr.sliced != sliced;
            tr.sliced = sliced;
        }
        if slices_changed {
            write_ledger(&ledger, &tracked); // before any slice, so a crash mid-slice is undone on restart
        }

        let mut status = render(&tracked, &st, cores);
        if duty < 1.0 {
            status += &format!(
                "agent work held to {budget:.1} of {cores} cores: it runs {:.0}% of each second\n",
                duty * 100.0
            );
        }
        for ((pid, _), l) in &leftovers {
            if let Some(label) = l.label.as_ref().filter(|l| !l.is_empty()) {
                status +=
                    &format!("{pid:>7}  leftover busy, no parent, low priority  {label}  (`kill {pid}` stops it)\n");
            }
        }
        let list_after = if cfg.close_idle_agents_s > 0 { cfg.close_idle_agents_s } else { 86_400 };
        let mut idle: Vec<(i32, &agents::Session)> = sessions
            .iter()
            .filter_map(|((pid, _), s)| Some((*pid, s.as_ref()?)))
            .filter(|(_, s)| !s.asked && s.idle_for(t) >= list_after)
            .collect();
        if !idle.is_empty() {
            idle.sort_by_key(|(_, s)| s.idle_since);
            let held: u64 = idle.iter().map(|(_, s)| s.mem).sum();
            let list: Vec<String> = idle.iter().map(|(pid, _)| pid.to_string()).collect();
            status += &format!(
                "agent sessions idle over {}: {} holding {} (pids {}); {}\n",
                span(list_after),
                idle.len(),
                size(held),
                list.join(" "),
                if cfg.close_idle_agents_s > 0 {
                    "closed while memory is short"
                } else {
                    "closing them frees it (`close_idle_agents_after_hours` lets nodoka do it)"
                }
            );
        }
        if !apps.is_empty() {
            let list: Vec<String> = apps.iter().map(|(n, m)| format!("{n} {}", size(*m))).collect();
            status +=
                &format!("biggest apps (nodoka leaves these alone; closing one frees memory): {}\n", list.join(", "));
        }
        write_status(&home, &mut last_status, &status);
        // Memory full and no build, test or install to hold back: the apps hold it. nodoka never
        // touches them, so it says which ones, at most once an hour.
        short_since = policy::short_since(short_since, st.level, st.last_critical, t);
        let builds = tracked.values().any(|tr| tr.job.kind.heavy() && !tr.job.ended);
        if cfg.notify_after_s > 0
            && short_since > 0
            && t.saturating_sub(short_since) >= cfg.notify_after_s
            && t.saturating_sub(last_notice) >= 3600
            && !builds
            && !apps.is_empty()
        {
            last_notice = t;
            let list: Vec<String> = apps.iter().take(3).map(|(n, m)| format!("{n} {}", size(*m))).collect();
            let body = format!("{}. Quit one you aren't using to stop the lag.", list.join(", "));
            log(&format!("notice          memory is full: {body}"));
            if opts.scope.is_none() && !opts.dry_run {
                sys::notify("Memory is full", &body);
            }
        }
        let slice: Vec<i32> = if opts.dry_run {
            vec![]
        } else {
            tracked.values().filter(|tr| tr.sliced).flat_map(|tr| pids(&tr.stoppable)).collect()
        };
        if slice.is_empty() {
            sleep_tick();
        } else {
            slice.iter().for_each(|&p| _ = sys::signal(p, true));
            std::thread::sleep(Duration::from_secs_f64(1.0 - duty));
            slice.iter().for_each(|&p| _ = sys::signal(p, false));
            std::thread::sleep(Duration::from_secs_f64(duty));
        }
    }
    if orphaned() {
        log("stopping: resuming everything");
        recover(&ledger);
    } else {
        log("stopping: resuming everything but builds paused for memory (the next start takes them over)");
        hold(&ledger);
    }
    fs::remove_file(home.join("pressure")).ok();
}

fn sleep_tick() {
    std::thread::sleep(Duration::from_secs(1));
}

struct Leftover {
    busy_since: u64,
    cpu: u64,
    at: u64,
    /// Set once judged: empty = not ours to touch (an app, a system service, outside the test scope).
    label: Option<String>,
}

/// The label of a busy process with no parent, unless it's an app or system service (they run
/// parentless by design) or outside the test scope.
fn leftover_label(pid: i32, scope: Option<&str>) -> Option<String> {
    let (argv, env) = sys::args_env(pid)?;
    let var = |name: &str| env.iter().find_map(|e| e.strip_prefix(name)?.strip_prefix('='));
    let exe = argv.first()?;
    let system = ["/System/", "/usr/libexec/", "/usr/sbin/", "/Library/"].iter().any(|d| exe.starts_with(d))
        || [".app/", ".appex/", ".framework/", ".xpc/"].iter().any(|b| exe.contains(b));
    if system || var("NODOKA_SKIP") == Some("1") || (scope.is_some() && var("NODOKA_TEST") != scope) {
        return None;
    }
    Some(label(&argv))
}

/// `pid` and everything it started, at any depth.
fn descendants(pid: i32, kids: &HashMap<i32, Vec<i32>>) -> Vec<i32> {
    let mut tree = vec![pid];
    let mut i = 0;
    while i < tree.len() {
        tree.extend(kids.get(&tree[i]).into_iter().flatten().copied());
        i += 1;
    }
    tree
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
    let browser = argv.iter().any(|a| a.contains("headless"));
    Some(Seen { kind, label: label(&argv), production, browser, found: false })
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

fn size(b: u64) -> String {
    if b >= 1 << 30 { format!("{:.1} GB", b as f64 / (1u64 << 30) as f64) } else { format!("{} MB", b >> 20) }
}

/// Memory per app, biggest first: each process counts toward its top ancestor (the one launchd or
/// systemd started), so Firefox's tabs count as Firefox. Dev jobs nodoka tracks are left out.
fn biggest_apps(procs: &[Proc], by_pid: &HashMap<i32, &Proc>, skip: &HashSet<i32>) -> Vec<(String, u64)> {
    let mut sum: HashMap<&str, u64> = HashMap::new();
    for p in procs.iter().filter(|p| !skip.contains(&p.pid)) {
        let mut top = p;
        for _ in 0..64 {
            match by_pid.get(&top.ppid) {
                Some(q) if q.pid > 1 && q.comm != "systemd" => top = q,
                _ => break,
            }
        }
        *sum.entry(&top.comm).or_default() += sys::cpu_mem(p.pid).map_or(0, |m| m.1);
    }
    let mut v: Vec<(String, u64)> = sum.into_iter().filter(|a| a.1 >= 256 << 20).map(|(n, m)| (n.into(), m)).collect();
    v.sort_by_key(|a| std::cmp::Reverse(a.1));
    v.truncate(4);
    v
}

fn span(s: u64) -> String {
    if s >= 3600 { format!("{}h", s / 3600) } else { format!("{s}s") }
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
        State::Queued if policy::short_of_memory(j, st.level, st.avail) => format!(
            "it needed {} last time and {} is free; it starts when there's room",
            size(j.expect),
            size(st.avail)
        ),
        State::Queued => "enough heavy jobs are running; it starts when one finishes".into(),
        State::PressurePaused => "memory is critically low".into(),
        State::IdlePaused => "idle; the next request or saved file wakes it".into(),
        State::Running if j.wake => "a request or saved file woke it".into(),
        State::Running if j.ended && j.sleepable && j.kind == Kind::Build => {
            "an idle build server holds its memory even paused; asked it to exit (the next build starts a fresh one)"
                .into()
        }
        State::Running if j.ended && j.kind != Kind::Test => {
            "held 10 minutes for memory, long after its agent stopped waiting; asked to exit (it can be run again)"
                .into()
        }
        State::Running if j.ended && st.stop_idle_tests_s > 0 && j.idle_for_s >= st.stop_idle_tests_s => format!(
            "a test run that used no CPU for {} min finished but never exited; asked it to exit",
            st.stop_idle_tests_s / 60
        ),
        State::Running if j.ended => format!(
            "an agent's test run still going after {} min while memory is critical; asked it to exit",
            j.age_s / 60
        ),
        State::Running if j.slow => "memory is critically low; it runs on spare CPU and disk time".into(),
        State::Running if j.bg && st.cpu_busy => "all cores are busy".into(),
        State::Running if j.bg => "waited the maximum time, started anyway".into(),
        State::Running => "ok".into(),
    }
}

/// One line in the terminal a job runs in, so a person watching knows why it stopped printing.
fn notice(pid: i32, tr: &mut Tracked, st: &Sys) {
    let msg = match tr.job.state {
        State::Queued if policy::short_of_memory(&tr.job, st.level, st.avail) => format!(
            "[nodoka] `{}` is waiting for memory: it needed {} last time. It starts on its own within {} minutes. See `nodoka status`.",
            tr.label,
            size(tr.job.expect),
            policy::MAX_PRESSURE_PAUSE_S / 60
        ),
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let o = fs::OpenOptions::new().write(true).custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK).open(path);
            if let Ok(mut f) = o {
                let _ = write!(f, "\r\n{msg}\r\n");
            }
        }
        // ponytail: no terminal notices on Windows (tty_path finds none)
        #[cfg(windows)]
        let _ = (path, msg);
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
            State::Running if tr.job.slow => "running, slow lane",
            State::Running if tr.job.bg => "running, low priority",
            State::Running => "running",
            State::Queued if policy::short_of_memory(&tr.job, st.level, st.avail) => "waiting for memory",
            State::Queued => "waiting for a slot",
            State::PressurePaused => "paused: low memory",
            State::IdlePaused => "asleep: idle",
        };
        let mem = size(tr.mem);
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

/// One line per process nodoka changed: `<pid> <start>`, plus ` mem <since>` for a build paused
/// for memory (kept paused across a restart).
fn write_ledger(path: &Path, tracked: &HashMap<Key, Tracked>) {
    let lines: String = tracked
        .values()
        .filter(|tr| tr.job.state != State::Running || tr.job.bg || tr.job.slow || tr.sliced)
        .flat_map(|tr| {
            let mem = tr.job.state == State::PressurePaused;
            let tag = if mem { format!(" mem {}", tr.job.state_since) } else { String::new() };
            tr.stoppable.iter().map(move |(p, id)| format!("{p} {id}{tag}\n"))
        })
        .collect();
    fs::write(path, lines).ok();
}

fn ledger_lines(path: &Path) -> Vec<(Key, Option<u64>)> {
    let text = fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let w: Vec<&str> = l.split(' ').collect();
            let key = (w.first()?.parse().ok()?, w.get(1)?.parse().ok()?);
            let since = (w.get(2) == Some(&"mem")).then(|| w.get(3)?.parse().ok()).flatten();
            Some((key, since))
        })
        .collect()
}

fn undo(q: i32) {
    sys::set_bg(q, false);
    sys::set_slow(q, false);
    sys::signal(q, false);
}

/// At start: resume everything in the ledger except live builds paused for memory, returned with
/// when they were paused. The file stays until the first tick rewrites it (a crash before then
/// loses nothing).
fn adopt(path: &Path) -> HashMap<Key, u64> {
    let alive: HashSet<Key> = sys::procs(sys::uid()).iter().map(|p| (p.pid, p.id)).collect();
    let mut kept = HashMap::new();
    for (k, since) in ledger_lines(path).into_iter().filter(|(k, _)| alive.contains(k)) {
        match since {
            Some(s) => _ = kept.insert(k, s),
            None => undo(k.0),
        }
    }
    kept
}

/// On a normal stop: resume everything except builds paused for memory, which stay in the ledger
/// for the next start. `nodoka off`, uninstall and a crash-restart still resume or adopt them.
fn hold(path: &Path) {
    let alive: HashSet<Key> = sys::procs(sys::uid()).iter().map(|p| (p.pid, p.id)).collect();
    let mut keep = String::new();
    for (k, since) in ledger_lines(path).into_iter().filter(|(k, _)| alive.contains(k)) {
        match since {
            Some(s) => keep += &format!("{} {} mem {s}\n", k.0, k.1),
            None => undo(k.0),
        }
    }
    if keep.is_empty() {
        fs::remove_file(path).ok();
    } else {
        fs::write(path, keep).ok();
    }
}

/// Resume and restore priority for everything listed in the ledger (crash-safe undo).
pub fn recover(path: &Path) {
    let alive: HashSet<Key> = sys::procs(sys::uid()).iter().map(|p| (p.pid, p.id)).collect();
    for (k, _) in ledger_lines(path).into_iter().filter(|(k, _)| alive.contains(k)) {
        undo(k.0);
    }
    fs::remove_file(path).ok();
}

/// One line per command and folder: `<bytes>\t<command>\t<folder>`.
/// The program a label runs, past its launcher: `npx tsc --noEmit` and `npm exec tsc -p .` are tsc.
fn tool(label: &str) -> &str {
    const LAUNCHERS: &[&str] =
        &["npx", "npm", "pnpm", "yarn", "bunx", "bun", "bundle", "mise", "exec", "dlx", "x", "run", "env", "--"];
    label.split_whitespace().find(|w| !LAUNCHERS.contains(w) && !w.starts_with('-')).unwrap_or(label)
}

/// What this command will need: what it used last time in this folder, or else the middle of
/// what the same program used anywhere. An agent's new worktree, or `npx tsc` where it ran
/// `npm exec tsc` before, has no exact record, and counting that as "nothing" let two 5 GB type
/// checks start side by side.
fn expected(peaks: &HashMap<(PathBuf, String), u64>, place: &(PathBuf, String)) -> u64 {
    if let Some(&b) = peaks.get(place) {
        return b;
    }
    let t = tool(&place.1);
    let mut same: Vec<u64> = peaks.iter().filter(|((_, l), _)| tool(l) == t).map(|(_, &b)| b).collect();
    same.sort_unstable();
    same.get(same.len() / 2).copied().unwrap_or(0)
}

fn load_peaks(path: &Path) -> HashMap<(PathBuf, String), u64> {
    let text = fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, '\t');
            let bytes = it.next()?.parse().ok()?;
            let (label, dir) = (it.next()?, it.next()?);
            Some(((PathBuf::from(dir), label.to_string()), bytes))
        })
        .collect()
}

fn save_peaks(path: &Path, peaks: &HashMap<(PathBuf, String), u64>) {
    // ponytail: one line per command and folder ever seen; prune if it ever grows large
    let text: String = peaks
        .iter()
        .map(|((dir, label), b)| format!("{b}\t{}\t{}\n", label.replace(['\t', '\n'], " "), dir.display()))
        .collect();
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, text).is_ok() {
        fs::rename(&tmp, path).ok();
    }
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
    if std::io::stdout().is_terminal() {
        print!("{line}");
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = f.write_all(line.as_bytes());
    }
}

fn clock() -> String {
    sys::clock()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_woken_server_is_logged_as_woken_not_as_overdue() {
        let mut j = Job::new(Kind::Dev, 0);
        (j.wake, j.bg) = (true, true); // lowered earlier, while every core was busy
        assert_eq!(why(&j, &policy::Sys::default()), "a request or saved file woke it");
    }

    #[test]
    fn what_each_job_needed_survives_a_restart() {
        let path = std::env::temp_dir().join(format!("nodoka-peaks-{}", std::process::id()));
        let mut peaks = HashMap::new();
        peaks.insert((PathBuf::from("/w/fe bulk"), "npm exec tsc --noEmit -p .".to_string()), 4_100_000_000);
        peaks.insert((PathBuf::from("/w/app"), "next\tbuild".to_string()), 9 << 30);
        save_peaks(&path, &peaks);
        let back = load_peaks(&path);
        fs::remove_file(&path).ok();
        assert_eq!(back.get(&(PathBuf::from("/w/fe bulk"), "npm exec tsc --noEmit -p .".into())), Some(&4_100_000_000));
        assert_eq!(
            back.get(&(PathBuf::from("/w/app"), "next build".into())),
            Some(&(9 << 30)),
            "a tab can't split a line"
        );
        assert!(load_peaks(Path::new("/nonexistent/peaks")).is_empty());

        // no record for this folder or this spelling: what the same program usually needs
        let gb = |n: u64| n << 30;
        let mut seen = HashMap::new();
        for (dir, label, b) in [
            ("/w/a", "npm exec tsc --noEmit", gb(5)),
            ("/w/b", "tsc --noEmit -p .", gb(6)),
            ("/w/c", "npm exec tsc --noEmit -p tsconfig.json", gb(4)),
            ("/w/a", "npm exec eslint src", gb(1)),
        ] {
            seen.insert((PathBuf::from(dir), label.to_string()), b);
        }
        let want = |dir: &str, label: &str| expected(&seen, &(PathBuf::from(dir), label.to_string()));
        assert_eq!(want("/w/a", "npm exec tsc --noEmit"), gb(5), "its own record first");
        assert_eq!(want("/w/new", "npx tsc --noEmit"), gb(5), "a new worktree, another launcher: tsc's middle");
        assert_eq!(want("/w/new", "npx eslint ."), gb(1));
        assert_eq!(want("/w/new", "cargo build"), 0, "never seen: unknown");
        seen.insert((PathBuf::from("/w/a"), "rubocop app/a.rb".into()), 100 << 20);
        let want = |dir: &str, label: &str| expected(&seen, &(PathBuf::from(dir), label.to_string()));
        assert_eq!(want("/w/b", "mise x -- bundle exec rubocop -a b.rb"), 100 << 20, "past bundle exec and mise x");
    }
}
