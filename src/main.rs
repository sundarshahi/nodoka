mod agents;
mod classify;
mod clean;
mod config;
mod daemon;
mod policy;
mod setup;
mod sys;

use std::fs;
use std::time::Duration;

const HELP: &str = "\
nodoka keeps your computer responsive while dev servers, builds, installs and coding
agents run. It works in the background: you keep using the same commands.

Usage:
  nodoka status            what is running, waiting or asleep, and why
  nodoka explain <cmd…>    how nodoka treats a command (or a running pid)
  nodoka off | on          pause nodoka (everything it paused resumes) / turn it back on
  nodoka install           set up for every terminal, editor and agent on this machine
  nodoka uninstall         remove everything nodoka added
  nodoka daemon            run the background helper in the foreground (for debugging)

Settings: ~/.nodoka/config    Log: ~/.nodoka/log
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = config::dir();
    #[cfg(unix)] // `nodoka status | head` ends quietly, like any Unix tool, instead of panicking
    if args.first().is_none_or(|a| a != "daemon") {
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    match args.first().map(String::as_str) {
        #[cfg(windows)]
        Some("daemon") if args.iter().any(|a| a == "--detach") => {
            if let Err(e) = detach(&std::env::current_exe().expect("own path")) {
                eprintln!("nodoka: {e}");
                std::process::exit(1);
            }
        }
        #[cfg(windows)]
        Some("daemon") if args.iter().any(|a| a == "--supervise") => supervise(),
        Some("daemon") => {
            let scope = args.iter().position(|a| a == "--test").and_then(|i| args.get(i + 1)).cloned();
            daemon::run(daemon::Options { dry_run: args.iter().any(|a| a == "--dry-run"), scope });
        }
        Some("status") => status(),
        Some("explain") => explain(&args[1..]),
        Some("off") => {
            fs::create_dir_all(&dir).ok();
            fs::write(dir.join("off"), "").expect("write the off switch");
            println!(
                "nodoka is off: everything it paused continues, and new shells skip its limits.\nRun `nodoka on` to turn it back on."
            );
        }
        Some("on") => {
            fs::remove_file(dir.join("off")).ok();
            println!("nodoka is on.");
        }
        Some("install") => {
            if let Err(e) = setup::install(&config::load(&dir.join("config"))) {
                eprintln!("nodoka install failed: {e}");
                std::process::exit(1);
            }
        }
        Some("uninstall") => setup::uninstall(),
        Some("--version" | "-V" | "version") => println!("nodoka {}", env!("CARGO_PKG_VERSION")),
        Some("help" | "--help" | "-h") | None => print!("{HELP}"),
        Some(other) => {
            eprintln!("nodoka: unknown command `{other}`\n\n{HELP}");
            std::process::exit(2);
        }
    }
}

/// Windows: start the helper's supervisor with no window, apart from this terminal (closing it
/// doesn't stop the helper).
#[cfg(windows)]
fn detach(bin: &std::path::Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new(bin)
        .args(["daemon", "--supervise"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .spawn()
        .map(drop)
        .map_err(|e| format!("start the background helper: {e}"))
}

/// Windows: what launchd's KeepAlive and systemd's Restart do elsewhere: run the helper and start
/// it again if it stops on its own.
#[cfg(windows)]
fn supervise() {
    let me = std::env::current_exe().expect("own path");
    loop {
        let ok = std::process::Command::new(&me).arg("daemon").status().is_ok_and(|s| s.success());
        if ok {
            return;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn status() {
    let dir = config::dir();
    if dir.join("off").exists() {
        println!("nodoka is off. Run `nodoka on` to turn it back on.");
        return;
    }
    let p = dir.join("status");
    let fresh = fs::metadata(&p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.elapsed().ok())
        .is_some_and(|e| e < Duration::from_secs(15));
    if !fresh {
        if setup::service_running() {
            println!("nodoka is starting…");
        } else {
            println!("nodoka's background helper is not running. Run `nodoka install` to set it up.");
        }
        return;
    }
    print!("{}", fs::read_to_string(p).unwrap_or_default());
}

fn explain(args: &[String]) {
    let custom = config::load(&config::dir().join("config")).custom;
    let argv: Vec<String> = match args {
        [pid] if pid.parse::<i32>().is_ok() => match sys::args_env(pid.parse().unwrap()) {
            Some((argv, _)) => argv,
            None => {
                eprintln!("no process {pid} (or it belongs to another user)");
                std::process::exit(1);
            }
        },
        [] => {
            eprintln!("usage: nodoka explain <command…> | <pid>");
            std::process::exit(2);
        }
        _ => args.to_vec(),
    };
    let what = match classify::classify(&argv, &custom) {
        Some(classify::Kind::Install) => {
            "an install. A few run at once; extras wait their turn (at most a minute). Never paused for low memory, because downloads would time out."
        }
        Some(classify::Kind::Build) => {
            "a build. A few run at once; extras wait their turn. Paused if memory gets critically low, resumed when it recovers."
        }
        Some(classify::Kind::Test) => {
            "a test run. It starts right away and is never paused (a paused test fails its timeouts); builds and installs wait for it when the slots are full."
        }
        Some(classify::Kind::Dev) => {
            "a dev server or watcher. Never queued. If it listens on a port and sits idle, it sleeps; the next request or a saved file in its project wakes it."
        }
        None => "not dev work nodoka knows. It is left alone. (Teach it in ~/.nodoka/config, or open an issue.)",
    };
    println!("`{}` is {what}", daemon::label(&argv));
}
