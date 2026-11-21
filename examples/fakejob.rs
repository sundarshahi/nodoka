//! Test helper for tests/daemon.rs: pretends to be a dev tool. The tests start it with a fake
//! command line (for example `node …/next/dist/bin/next build`) and choose what it does with
//! FAKEJOB=burn (use CPU, with one busy child process), serve:<port> (answer TCP requests), or
//! idle (do nothing), or leftover (use CPU after its parent is gone, for at most 60s).

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

#[allow(clippy::zombie_processes)] // the busy child lives as long as the job
fn main() {
    let mode = std::env::var("FAKEJOB").unwrap_or_default();
    // exit once orphaned (job killed, test run killed, manual run), so no mode ever outlives its test;
    // `leftover` is orphaned on purpose and has a hard 60s limit instead
    let parent = std::os::unix::process::parent_id();
    let leftover = mode == "leftover";
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(300));
            if (!leftover && std::os::unix::process::parent_id() != parent) || start.elapsed().as_secs() >= 60 {
                std::process::exit(0);
            }
        }
    });
    if mode == "burn" || leftover {
        if !leftover && std::env::var_os("FAKEJOB_CHILD").is_none() {
            let me = std::env::current_exe().unwrap();
            Command::new(me)
                .arg0("node")
                .arg("worker.js")
                .env("FAKEJOB_CHILD", "1")
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
        }
        loop {
            std::hint::black_box((0..100_000u64).sum::<u64>());
        }
    }
    if let Some(port) = mode.strip_prefix("serve:") {
        // a blocking accept(), the way Tomcat- and Jetty-style servers wait for connections
        let listener = std::net::TcpListener::bind(("127.0.0.1", port.parse().unwrap())).unwrap();
        for mut conn in listener.incoming().flatten() {
            let mut buf = [0u8; 1024];
            let _ = conn.read(&mut buf);
            let _ = conn.write_all(b"ok\n");
        }
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
