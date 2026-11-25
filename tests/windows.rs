//! End-to-end on Windows: a real daemon governing fake jobs. Each test uses its own NODOKA_HOME
//! and `--test <scope>`, so it only touches the jobs it starts (found by their environment).
#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_nodoka");

struct Kill(Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Daemon {
    home: PathBuf,
    _child: Kill,
}

impl Daemon {
    fn start(scope: &str, config: &str, env: &[(&str, &str)]) -> Daemon {
        let home = std::env::temp_dir().join(format!("nodoka-win-{scope}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("config"), format!("agent_cpu_percent = 100\n{config}")).unwrap();
        let child = Command::new(BIN)
            .args(["daemon", "--test", scope])
            .env("NODOKA_HOME", &home)
            .env("NODOKA_PRESSURE", "1")
            .envs(env.iter().copied())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Daemon { home, _child: Kill(child) }
    }

    fn status(&self) -> String {
        fs::read_to_string(self.home.join("status")).unwrap_or_default()
    }

    fn log(&self) -> String {
        fs::read_to_string(self.home.join("log")).unwrap_or_default()
    }
}

/// A fake `next build`: the helper copied as node.exe (Windows names a process by its program file).
fn build(scope: &str, mode: &str) -> Kill {
    let dir = std::env::temp_dir().join(format!("nodoka-win-bin-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let node = dir.join("node.exe");
    if !node.exists() {
        let fake = Path::new(BIN).parent().unwrap().join("examples").join("fakejob.exe");
        fs::copy(&fake, &node).unwrap_or_else(|e| panic!("copy {}: {e}", fake.display()));
    }
    let child = Command::new(&node)
        .args(["/app/node_modules/next/dist/bin/next", "build"])
        .env("NODOKA_TEST", scope)
        .env("FAKEJOB", mode)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    Kill(child)
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

#[test]
fn a_build_is_paused_while_the_machine_freezes() {
    let _job = build("winfreeze", "burn");
    std::thread::sleep(Duration::from_millis(300));
    let d = Daemon::start("winfreeze", "", &[("NODOKA_PRESSURE", "4"), ("NODOKA_FREEZING", "1")]);
    let paused = eventually(20, || d.status().contains("paused: low memory"));
    assert!(paused, "not paused:\n{}\n{}", d.status(), d.log());
}

#[test]
fn a_second_build_waits_for_a_slot_and_starts_when_the_first_ends() {
    let d = Daemon::start("winslot", "heavy_jobs = 1\nmax_wait_seconds = 600\n", &[]);
    std::thread::sleep(Duration::from_secs(2));
    let first = build("winslot", "burn");
    assert!(eventually(10, || d.status().contains("running")), "first not seen:\n{}", d.log());
    let _second = build("winslot", "burn");
    let queued = eventually(15, || d.status().contains("waiting for a slot"));
    assert!(queued, "second not queued:\n{}\n{}", d.status(), d.log());
    drop(first);
    let resumed = eventually(15, || !d.status().contains("waiting for a slot") && d.status().contains("running"));
    assert!(resumed, "second never resumed:\n{}\n{}", d.status(), d.log());
    assert!(d.log().contains("resume"), "{}", d.log());
}
