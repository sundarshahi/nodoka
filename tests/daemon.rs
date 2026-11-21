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
        let home = std::env::temp_dir().join(format!("nodoka-it-{scope}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("config"), config).unwrap();
        let child = spawn_daemon(&home, scope);
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
    Command::new(BIN)
        .args(["daemon", "--test", scope])
        .env("NODOKA_HOME", home)
        .env("NODOKA_PRESSURE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
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
    let (log, status, orphan_nice) = (d.log(), d.status(), nice(orphan));
    unsafe { libc::kill(orphan, libc::SIGKILL) };
    assert!(flagged, "the leftover wasn't noticed; log:\n{log}");
    assert_eq!(orphan_nice, 10, "the leftover kept its priority");
    assert!(status.contains(&format!("{orphan}  leftover busy")), "{status}");
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
    pids.iter().for_each(|&p| unsafe {
        libc::kill(p, libc::SIGKILL);
    });
    let _ = fs::remove_dir_all(&home);
    assert!(ok, "a test helper outlived the test run that started it");
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
    assert!(children(b_pid).iter().all(|&c| !stopped(c)));
}

#[test]
fn idle_server_sleeps_and_the_next_request_wakes_it() {
    let d = Daemon::start("idle", "idle_sleep_seconds = 2\n");
    let port = free_port();
    let s = Job(
        d.job(&["node", "/app/node_modules/vite/bin/vite.js", "--port", &port.to_string()], &format!("serve:{port}"))
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
