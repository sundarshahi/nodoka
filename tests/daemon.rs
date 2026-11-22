//! End-to-end: a real daemon governing real processes. Each test uses its own NODOKA_HOME and
//! `--test <scope>`, so it only touches the fake jobs it starts — safe to run on a dev machine.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_nodoka");

struct Daemon {
    home: PathBuf,
    scope: String,
    child: Child,
}

impl Daemon {
    fn start(scope: &str, config: &str) -> Daemon {
        Daemon::start_env(scope, config, &[])
    }

    /// `env` pins what the daemon reads from the machine, e.g. NODOKA_PRESSURE or NODOKA_AVAIL_MB.
    fn start_env(scope: &str, config: &str, env: &[(&str, &str)]) -> Daemon {
        let home = std::env::temp_dir().join(format!("nodoka-it-{scope}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        // the CPU budget slices busy jobs; only the test about it turns it on (a later line wins)
        fs::write(home.join("config"), format!("agent_cpu_percent = 100\n{config}")).unwrap();
        let mut child = spawn_cmd(&home, scope);
        let child = child.envs(env.iter().copied()).spawn().unwrap();
        Daemon { home, scope: scope.into(), child }
    }

    /// Start a fake tool: `argv` is what the daemon sees, `mode` what it does.
    fn job(&self, argv: &[&str], mode: &str) -> Child {
        self.job_in(&std::env::temp_dir(), argv, mode)
    }

    fn job_in(&self, dir: &Path, argv: &[&str], mode: &str) -> Child {
        let mut c = Command::new(fakejob());
        c.current_dir(dir).arg0(argv[0]).args(&argv[1..]).env("NODOKA_TEST", &self.scope).env("FAKEJOB", mode);
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        unsafe {
            c.pre_exec(|| {
                libc::setsid(); // no controlling terminal, like a coding agent's shell
                Ok(())
            });
        }
        c.spawn().unwrap()
    }

    fn cli(&self, args: &[&str]) {
        let ok = Command::new(BIN)
            .args(args)
            .env("NODOKA_HOME", &self.home)
            .stdout(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "nodoka {args:?} failed");
    }

    fn status(&self) -> String {
        fs::read_to_string(self.home.join("status")).unwrap_or_default()
    }

    fn log(&self) -> String {
        fs::read_to_string(self.home.join("log")).unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.home);
    }
}

/// `cargo test` builds examples, `cargo test --test daemon` doesn't: build the helper if needed.
fn fakejob() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let path = Path::new(BIN).parent().unwrap().join("examples/fakejob");
            if !path.exists() {
                let ok = Command::new(env!("CARGO"))
                    .args(["build", "--example", "fakejob"])
                    .current_dir(env!("CARGO_MANIFEST_DIR"))
                    .status()
                    .unwrap()
                    .success();
                assert!(ok, "could not build the fakejob example");
            }
            path
        })
        .clone()
}

fn spawn_daemon(home: &Path, scope: &str) -> Child {
    spawn_cmd(home, scope).spawn().unwrap()
}

fn spawn_cmd(home: &Path, scope: &str) -> Command {
    let mut c = Command::new(BIN);
    c.args(["daemon", "--test", scope])
        .env("NODOKA_HOME", home)
        .env("NODOKA_PRESSURE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c
}

struct Job(Child);
impl Drop for Job {
    fn drop(&mut self) {
        unsafe { libc::kill(self.0.id() as i32, libc::SIGCONT) };
        for c in children(self.0.id() as i32) {
            unsafe { libc::kill(c, libc::SIGKILL) };
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(target_os = "linux")]
fn state(pid: i32) -> Option<char> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    s[s.rfind(')')? + 1..].split_whitespace().next()?.chars().next()
}

#[cfg(target_os = "linux")]
fn children(pid: i32) -> Vec<i32> {
    fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|&p| {
            fs::read_to_string(format!("/proc/{p}/stat"))
                .ok()
                .and_then(|s| s[s.rfind(')')? + 1..].split_whitespace().nth(1)?.parse::<i32>().ok())
                == Some(pid)
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn state(pid: i32) -> Option<char> {
    let out = Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().chars().next()
}

#[cfg(target_os = "macos")]
fn children(pid: i32) -> Vec<i32> {
    let out = Command::new("pgrep").args(["-P", &pid.to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().parse().ok()).collect()
}

fn stopped(pid: i32) -> bool {
    state(pid) == Some('T')
}

fn eventually(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    f()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

const NEXT_BUILD: &[&str] = &["node", "/app/node_modules/next/dist/bin/next", "build"];

#[test]
fn a_fake_jobs_busy_child_stops_when_the_job_is_killed() {
    // A leftover busy child once spun two cores for 36 hours after its job was killed.
    let mut job = Command::new(fakejob()).env("FAKEJOB", "burn").stdout(Stdio::null()).spawn().unwrap();
    let pid = job.id() as i32;
    let mut kids = Vec::new();
    assert!(
        eventually(5, || {
            kids = children(pid);
            !kids.is_empty()
        }),
        "no busy child started"
    );
    job.kill().unwrap();
    job.wait().unwrap();
    let gone = |c: i32| unsafe { libc::kill(c, 0) } != 0;
    let ok = eventually(5, || kids.iter().all(|&c| gone(c)));
    kids.iter().for_each(|&c| unsafe {
        libc::kill(c, libc::SIGKILL);
    });
    assert!(ok, "the busy child outlived its job");
}

#[test]
fn agent_work_over_its_cpu_share_runs_in_slices_but_tests_never_do() {
    let d = Daemon::start("share", "agent_cpu_percent = 1\nheavy_jobs = 4\n");
    let build = Job(d.job(&["go", "build", "./..."], "burn"));
    let test = Job(d.job(&["node", "/x/node_modules/vitest/vitest.mjs", "run"], "burn"));
    let (b, t) = (build.0.id() as i32, test.0.id() as i32);
    assert!(eventually(10, || d.log().contains("share cpu")), "never held back:\n{}", d.log());
    let (mut b_stopped, mut b_ran, mut t_stopped) = (false, false, false);
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        (b_stopped, b_ran, t_stopped) = (b_stopped || stopped(b), b_ran || !stopped(b), t_stopped || stopped(t));
        std::thread::sleep(Duration::from_millis(20));
    }
    let ledger = fs::read_to_string(d.home.join("paused")).unwrap_or_default();
    assert!(b_stopped && b_ran, "the build should run in slices (stopped: {b_stopped}, ran: {b_ran})");
    assert!(!t_stopped, "a test run was paused");
    assert!(ledger.lines().any(|l| l.starts_with(&format!("{b} "))), "slices must be in the crash ledger: {ledger}");
    assert!(d.status().contains("agent work held to"), "{}", d.status());
}

#[test]
fn a_busy_process_left_without_a_parent_gets_lower_priority() {
    let d = Daemon::start("leftover", "leftover_busy_seconds = 3\n");
    // `sh` exits right away, leaving the busy job with no parent (it gives up on its own after 60s)
    let out = Command::new("sh")
        .args(["-c", r#""$1" >/dev/null 2>&1 & echo $!"#, "sh", fakejob().to_str().unwrap()])
        .env("NODOKA_TEST", "leftover")
        .env("FAKEJOB", "leftover")
        .output()
        .unwrap();
    let orphan: i32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    let parented = Job(d.job(&["node", "worker.js"], "burn")); // just as busy, but its parent is alive
    let nice = |p: i32| unsafe { libc::getpriority(libc::PRIO_PROCESS as _, p as libc::id_t) };
    // judged by the log: a real nodoka on this machine may have lowered this test run's priority already
    let flagged = eventually(25, || d.log().contains(&format!("leftover pid {orphan} ")));
    // the status file is rewritten at the end of the tick that logged it
    let listed = eventually(3, || d.status().contains(&format!("{orphan}  leftover busy")));
    let (log, status, orphan_nice) = (d.log(), d.status(), nice(orphan));
    unsafe { libc::kill(orphan, libc::SIGKILL) };
    assert!(flagged, "the leftover wasn't noticed; log:\n{log}");
    assert!(orphan_nice >= 10, "the leftover kept its priority ({orphan_nice})");
    assert!(listed, "{status}");
    assert!(!log.contains(&format!("pid {} ", parented.0.id())), "a busy process whose parent is alive was touched");
}

#[test]
fn test_helpers_stop_when_the_test_run_that_started_them_dies() {
    // `sh` stands in for a killed `cargo test`: it starts a test daemon and a busy job, then exits.
    let home = std::env::temp_dir().join(format!("nodoka-orphan-{}", std::process::id()));
    let out = Command::new("sh")
        .args([
            "-c",
            r#""$1" daemon --test orphan >/dev/null 2>&1 & echo $!; FAKEJOB=burn "$2" >/dev/null 2>&1 & echo $!; sleep 1"#,
            "sh",
        ])
        .args([BIN, fakejob().to_str().unwrap()])
        .env("NODOKA_HOME", &home)
        .output()
        .unwrap();
    let mut pids: Vec<i32> =
        String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().parse().ok()).collect();
    assert_eq!(pids.len(), 2, "could not start the helpers");
    pids.extend(children(pids[1])); // the busy child too, so even a failing run leaves nothing behind
    let gone = |p: i32| unsafe { libc::kill(p, 0) } != 0;
    let ok = eventually(5, || pids.iter().all(|&p| gone(p)));
    let alive: Vec<i32> = pids.iter().copied().filter(|&p| !gone(p)).collect();
    pids.iter().for_each(|&p| unsafe {
        libc::kill(p, libc::SIGKILL);
    });
    let _ = fs::remove_dir_all(&home);
    assert!(ok, "test helpers outlived the test run that started them: {alive:?} of [daemon, job, children] {pids:?}");
}

#[test]
fn extra_builds_wait_their_turn_then_run() {
    let d = Daemon::start("queue", "heavy_jobs = 1\nmax_wait_seconds = 300\n");
    let a = Job(d.job(NEXT_BUILD, "burn"));
    let a_pid = a.0.id() as i32;
    assert!(
        eventually(10, || d.status().contains(&a_pid.to_string())),
        "daemon never saw the first build:\n{}",
        d.status()
    );
    let b = Job(d.job(&["ruby", "bin/rails", "assets:precompile"], "burn"));
    let b_pid = b.0.id() as i32;
    assert!(eventually(10, || stopped(b_pid)), "second build was not queued:\n{}\n{}", d.status(), d.log());
    assert!(!stopped(a_pid), "the first build must keep running");
    assert!(eventually(5, || children(b_pid).iter().all(|&c| stopped(c))), "the queued build's worker kept running");
    assert!(d.status().contains("waiting for a slot"), "{}", d.status());
    drop(a); // first build finishes → the queued one starts
    assert!(eventually(10, || !stopped(b_pid)), "queued build never started:\n{}", d.log());
    // the build and its worker are resumed one after the other: give the worker its moment
    assert!(eventually(5, || children(b_pid).iter().all(|&c| !stopped(c))), "the worker stayed paused");
}

#[test]
fn idle_server_sleeps_and_the_next_request_wakes_it() {
    let d = Daemon::start("idle", "idle_sleep_seconds = 2\n");
    let port = free_port();
    let s = Job(
        // opens its port 2s after starting: nodoka's first look finds no port (missed on a slow CI runner)
        d.job(
            &["node", "/app/node_modules/vite/bin/vite.js", "--port", &port.to_string()],
            &format!("serve:{port}:2000"),
        ),
    );
    let pid = s.0.id() as i32;
    assert!(eventually(15, || stopped(pid)), "idle server never went to sleep:\n{}\n{}", d.status(), d.log());
    let started = Instant::now();
    let mut conn =
        TcpStream::connect(("127.0.0.1", port)).expect("the kernel accepts the connection while the server sleeps");
    conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    conn.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    let mut reply = String::new();
    conn.read_to_string(&mut reply).expect("the sleeping server woke up and answered");
    assert_eq!(reply, "ok\n");
    assert!(started.elapsed() < Duration::from_secs(5), "wake took {:?}", started.elapsed());
    assert!(!stopped(pid));
}

#[test]
fn saving_a_project_file_wakes_a_sleeping_server_but_dependency_changes_dont() {
    let d = Daemon::start("edit", "idle_sleep_seconds = 2\n");
    let project = d.home.join("app");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(project.join("node_modules/react")).unwrap();
    let port = free_port();
    let argv = ["node", "/app/node_modules/vite/bin/vite.js", "--port", &port.to_string()];
    let s = Job(d.job_in(&project, &argv, &format!("serve:{port}")));
    let pid = s.0.id() as i32;
    assert!(eventually(15, || stopped(pid)), "idle server never went to sleep:\n{}", d.log());
    std::thread::sleep(Duration::from_secs(1));
    fs::write(project.join("node_modules/react/index.js"), "x").unwrap();
    std::thread::sleep(Duration::from_secs(3));
    assert!(!d.log().contains("resume"), "a change inside node_modules woke the server:\n{}", d.log());
    let saved = Instant::now();
    fs::write(project.join("src/App.tsx"), "export default 1").unwrap();
    assert!(eventually(5, || !stopped(pid)), "saving a source file did not wake the server:\n{}", d.log());
    assert!(saved.elapsed() < Duration::from_secs(3), "wake took {:?}", saved.elapsed());
}

#[test]
fn a_watcher_without_a_port_never_sleeps() {
    let d = Daemon::start("watcher", "idle_sleep_seconds = 1\n");
    let w = Job(d.job(&["node", "/app/node_modules/typescript/bin/tsc", "--watch"], "idle"));
    let pid = w.0.id() as i32;
    assert!(eventually(10, || d.status().contains(&pid.to_string())));
    std::thread::sleep(Duration::from_secs(4));
    assert!(!stopped(pid), "nothing could wake a watcher with no port, so it must stay awake");
}

#[test]
fn a_crashed_daemon_resumes_what_it_paused_on_restart() {
    let mut d = Daemon::start("crash", "heavy_jobs = 1\nmax_wait_seconds = 300\n");
    let a = Job(d.job(NEXT_BUILD, "burn"));
    assert!(eventually(10, || d.status().contains(&a.0.id().to_string())));
    let b = Job(d.job(&["go", "build", "./..."], "burn"));
    let b_pid = b.0.id() as i32;
    assert!(eventually(10, || stopped(b_pid)));
    unsafe { libc::kill(d.child.id() as i32, libc::SIGKILL) };
    let _ = d.child.wait();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(stopped(b_pid), "a killed daemon can't resume anything by itself");
    d.child = spawn_daemon(&d.home, &d.scope);
    // on restart it first resumes everything in its ledger; the job is then queued again only
    // because the first build still runs — so check it ran at least once in between
    assert!(eventually(10, || d.log().matches("started:").count() == 2), "{}", d.log());
    drop(a);
    assert!(eventually(10, || !stopped(b_pid)), "{}", d.log());
}

#[test]
fn turning_nodoka_off_resumes_everything() {
    let d = Daemon::start("off", "heavy_jobs = 1\nmax_wait_seconds = 300\n");
    let a = Job(d.job(NEXT_BUILD, "burn"));
    assert!(eventually(10, || d.status().contains(&a.0.id().to_string())));
    let b = Job(d.job(&["cargo", "build", "--release"], "burn"));
    let b_pid = b.0.id() as i32;
    assert!(eventually(10, || stopped(b_pid)));
    d.cli(&["off"]);
    assert!(eventually(10, || !stopped(b_pid)), "{}", d.log());
    assert!(eventually(10, || d.status().contains("off")));
    d.cli(&["on"]);
}

#[test]
fn unknown_programs_are_never_touched() {
    let d = Daemon::start("unknown", "heavy_jobs = 1\nidle_sleep_seconds = 1\n");
    let a = Job(d.job(NEXT_BUILD, "burn"));
    assert!(eventually(10, || d.status().contains(&a.0.id().to_string())));
    let port = free_port();
    let u = Job(d.job(&["node", "my-app.js"], &format!("serve:{port}")));
    std::thread::sleep(Duration::from_secs(5));
    assert!(!stopped(u.0.id() as i32));
    assert!(!d.status().contains(&u.0.id().to_string()));
}

#[test]
fn a_type_check_that_needed_more_memory_than_is_free_waits_before_it_starts_next_time() {
    // memory is short (warn) and 50 MB is free; the fake type check holds 200 MB
    let d = Daemon::start_env("memwait", "", &[("NODOKA_PRESSURE", "2"), ("NODOKA_AVAIL_MB", "50")]);
    let base = std::env::temp_dir().join(format!("nodoka-memwait-{}", std::process::id()));
    let (here, there) = (base.join("here"), base.join("there"));
    fs::create_dir_all(&here).unwrap();
    fs::create_dir_all(&there).unwrap();
    let tsc = ["node", "/app/node_modules/typescript/bin/tsc", "--noEmit"];
    let mem_mb = |pid: u32| -> u64 {
        let status = d.status();
        let Some(line) = status.lines().find(|l| l.trim_start().starts_with(&format!("{pid} "))) else { return 0 };
        let w: Vec<&str> = line.split_whitespace().collect();
        w.windows(2).find(|p| p[1] == "MB" || p[1] == "GB").map_or(0, |p| {
            let v: f64 = p[0].parse().unwrap_or(0.0);
            (if p[1] == "GB" { v * 1024.0 } else { v }) as u64
        })
    };

    // first run here: nothing to go on, so it runs; nodoka notes it needed ~200 MB
    let first = Job(d.job_in(&here, &tsc, "hold:200"));
    let pid = first.0.id();
    assert!(eventually(15, || mem_mb(pid) >= 150), "the first run never showed its memory:\n{}", d.status());
    assert!(!stopped(pid as i32), "a run with no record must not wait");
    drop(first);
    assert!(eventually(5, || mem_mb(pid) == 0 && !d.status().contains(&format!("{pid} "))));

    // next run here: it waits before it starts, while it holds next to nothing
    let second = Job(d.job_in(&here, &tsc, "hold:200"));
    let pid2 = second.0.id() as i32;
    assert!(eventually(8, || stopped(pid2) && d.status().contains("waiting for memory")), "{}", d.status());
    assert!(d.log().contains("last time"), "{}", d.log());

    // the same command in another folder has no record: it runs
    let other = Job(d.job_in(&there, &tsc, "idle"));
    std::thread::sleep(Duration::from_secs(3));
    assert!(!stopped(other.0.id() as i32), "the record is per folder");
    drop((second, other));
    let _ = fs::remove_dir_all(&base);
}

/// Scheduling state that shows the slow lane: macOS's scheduler priority (31 normal, 4 background),
/// Linux's disk-priority class (0 default, 3 idle).
#[cfg(target_os = "macos")]
fn lane(pid: i32) -> i32 {
    let out = Command::new("ps").args(["-o", "pri=", "-p", &pid.to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(-1)
}
#[cfg(target_os = "macos")]
const SLOW: i32 = 4;

#[cfg(target_os = "linux")]
fn lane(pid: i32) -> i32 {
    (unsafe { libc::syscall(libc::SYS_ioprio_get, 1 as libc::c_long, pid as libc::c_long) } >> 13) as i32
}
#[cfg(target_os = "linux")]
const SLOW: i32 = 3;

#[test]
fn critical_memory_puts_a_busy_build_in_the_slow_lane_without_pausing_it() {
    let d = Daemon::start_env("slowlane", "", &[("NODOKA_PRESSURE", "4")]);
    let build = Job(d.job(&["node", "/app/node_modules/next/dist/bin/next", "build"], "burn"));
    let pid = build.0.id() as i32;
    let worker = || children(pid).first().copied();
    let slowed = eventually(15, || lane(pid) == SLOW && worker().is_some_and(|w| lane(w) == SLOW));
    assert!(slowed, "not slowed: {} {:?}\n{}", lane(pid), worker().map(lane), d.log());
    for _ in 0..6 {
        assert!(!stopped(pid), "a build is never paused for memory by default");
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(d.status().contains("running, slow lane"), "{}", d.status());
    let ledger = fs::read_to_string(d.home.join("paused")).unwrap_or_default();
    assert!(ledger.contains(&format!("{pid} ")), "not in the crash ledger: {ledger}");
    // stopping the helper gives the build its full speed back
    unsafe { libc::kill(d.child.id() as i32, libc::SIGTERM) };
    assert!(eventually(5, || lane(pid) != SLOW), "still slow after the helper stopped");
}
