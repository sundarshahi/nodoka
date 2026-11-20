//! `~/.nodoka/config`: optional `key = value` lines. Every setting has a default.

use crate::classify::{Custom, Kind};
use crate::sys;
use std::path::{Path, PathBuf};

pub struct Config {
    /// Builds, tests and installs allowed to run at once (at normal memory pressure).
    pub heavy_jobs: usize,
    /// Worker/thread cap each tool is told about.
    pub cpus: usize,
    /// Idle servers sleep after this long; 0 turns idle sleep off.
    pub idle_sleep_s: u64,
    /// A queued job never waits longer than this.
    pub max_wait_s: u64,
    pub custom: Custom,
    pub warnings: Vec<String>,
}

pub const TEMPLATE: &str = "\
# nodoka settings. Every line is optional; delete a line to get the default back.
# Changes apply within a few seconds. After changing `cpus`, open a new terminal.

# Builds, tests and installs allowed to run at once (default: 1 per 5 GB of memory, at most half your cores).
# heavy_jobs = 3

# Worker/thread count each tool is told about (default: half your cores, at least 2).
# cpus = 4

# Idle dev servers sleep after this many minutes and wake on the next request or saved file. 0 turns it off.
# idle_sleep_minutes = 10

# A job waiting for a free slot starts anyway after this many seconds.
# max_wait_seconds = 60

# Teach nodoka about your own tools (program names, comma-separated):
# dev = my-server, my-watcher
# build = my-compiler
# test = my-test-runner
# install = my-installer
# ignore = some-tool
";

pub fn dir() -> PathBuf {
    if let Ok(d) = std::env::var("NODOKA_HOME") {
        return PathBuf::from(d);
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".nodoka")
}

pub fn default_cpus() -> usize {
    (sys::cores() / 2).max(2)
}

pub fn load(path: &Path) -> Config {
    let cores = sys::cores();
    let mem_gb = (sys::mem_bytes() >> 30) as usize;
    let mut c = Config {
        heavy_jobs: (mem_gb / 5).min(cores / 2).max(1),
        cpus: default_cpus(),
        idle_sleep_s: 600,
        max_wait_s: 60,
        custom: Custom::new(),
        warnings: vec![],
    };
    let text = std::fs::read_to_string(path).unwrap_or_default();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            c.warnings.push(format!("config line {}: expected `key = value`", n + 1));
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let num = value.parse::<u64>();
        match (key, &num) {
            ("heavy_jobs", Ok(v)) => c.heavy_jobs = (*v as usize).max(1),
            ("cpus", Ok(v)) => c.cpus = (*v as usize).max(1),
            ("idle_sleep_minutes", Ok(v)) => c.idle_sleep_s = v * 60,
            ("idle_sleep_seconds", Ok(v)) => c.idle_sleep_s = *v,
            ("max_wait_seconds", Ok(v)) => c.max_wait_s = *v,
            ("dev" | "build" | "test" | "install" | "ignore", _) => {
                for tool in value.split(',').map(|t| t.trim().to_ascii_lowercase()).filter(|t| !t.is_empty()) {
                    c.custom.insert(tool, Kind::parse(key));
                }
            }
            (_, Err(_))
                if matches!(
                    key,
                    "heavy_jobs" | "cpus" | "idle_sleep_minutes" | "idle_sleep_seconds" | "max_wait_seconds"
                ) =>
            {
                c.warnings.push(format!("config line {}: `{key}` needs a whole number", n + 1));
            }
            _ => c.warnings.push(format!("config line {}: unknown setting `{key}`", n + 1)),
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_settings_rules_and_reports_mistakes() {
        let p = std::env::temp_dir().join(format!("nodoka-config-test-{}", std::process::id()));
        std::fs::write(&p, "heavy_jobs = 2\nidle_sleep_minutes = 0 # off\ndev = My-Server, other\nignore = next\ncpus = lots\nbogus = 1\n").unwrap();
        let c = load(&p);
        std::fs::remove_file(&p).ok();
        assert_eq!(c.heavy_jobs, 2);
        assert_eq!(c.idle_sleep_s, 0);
        assert_eq!(c.custom.get("my-server"), Some(&Some(Kind::Dev)));
        assert_eq!(c.custom.get("next"), Some(&None));
        assert_eq!(c.cpus, default_cpus());
        assert_eq!(c.warnings.len(), 2, "{:?}", c.warnings);
    }

    #[test]
    fn template_is_all_comments() {
        let p = std::env::temp_dir().join(format!("nodoka-template-test-{}", std::process::id()));
        std::fs::write(&p, TEMPLATE).unwrap();
        let c = load(&p);
        std::fs::remove_file(&p).ok();
        assert!(c.warnings.is_empty() && c.custom.is_empty());
    }
}
