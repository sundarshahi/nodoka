mod classify;
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
    match args.first().map(String::as_str) {
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
