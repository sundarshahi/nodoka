//! Test helper for tests/daemon.rs: pretends to be a dev tool. The tests start it with a fake
//! command line (for example `node …/next/dist/bin/next build`) and choose what it does with
//! FAKEJOB=burn (use CPU, with one busy child process), serve:<port> (answer TCP requests), or
//! idle (do nothing).

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

#[allow(clippy::zombie_processes)] // the busy child lives as long as the job
fn main() {
    let mode = std::env::var("FAKEJOB").unwrap_or_default();
    if mode == "burn" {
        if std::env::var_os("FAKEJOB_CHILD").is_none() {
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
