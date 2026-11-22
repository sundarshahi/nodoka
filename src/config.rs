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
    /// A process left with no parent that keeps a core busy this long gets lower priority; 0 turns it off.
    pub leftover_busy_s: u64,
    /// Share of all cores that builds, installs and dev servers started without a terminal may use
    /// together; 100 turns the budget off.
    pub agent_cpu_percent: u64,
    /// At critical memory: pause busy builds (true) or move them to the slow lane (false, default).
    pub pause_for_memory: bool,
    /// Folders holding git worktrees to clean when idle (empty: cleaning off).
    pub clean_dirs: Vec<PathBuf>,
    pub clean_caches_after_s: u64,
    pub remove_worktrees_after_s: u64,
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

# A process left behind by a tool or agent (its parent is gone) that keeps a core busy this many
# minutes gets lower priority and is listed in `nodoka status`. 0 turns it off.
# leftover_busy_minutes = 10

# Builds, installs and dev servers started without a terminal (coding agents, scripts) share at most
# this much of your CPU, so the rest stays free for you and the machine runs cooler. Test runs and
# anything you start in a terminal are never held back. 100 turns it off.
# agent_cpu_percent = 50

# When memory is critically low, busy builds keep running in a slow lane: spare CPU and disk time
# only, so your apps stay responsive. `pause` stops them until there's room instead (frees the
# machine faster, but a paused job keeps its memory and whoever started it waits).
# memory_brake = slow

# Folders holding git worktrees (comma-separated). In a worktree idle this long — no git activity,
# no program working in it — folders git ignores and a tool rebuilds (node_modules, .next, .turbo…)
# are deleted. After `remove_worktrees_after_days` the worktree itself goes, but only if nothing
# would be lost: no changes, its commit is on a remote, no other ignored files (.env). 0: never.
# clean_dirs = ~/worktrees
# clean_caches_after_hours = 24
# remove_worktrees_after_days = 7

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
        leftover_busy_s: 600,
        agent_cpu_percent: 50,
        pause_for_memory: false,
        clean_dirs: vec![],
        clean_caches_after_s: 24 * 3600,
        remove_worktrees_after_s: 7 * 86400,
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
            ("leftover_busy_minutes", Ok(v)) => c.leftover_busy_s = v * 60,
            ("leftover_busy_seconds", Ok(v)) => c.leftover_busy_s = *v,
            ("agent_cpu_percent", Ok(v)) => c.agent_cpu_percent = (*v).clamp(1, 100),
            ("clean_caches_after_hours", Ok(v)) => c.clean_caches_after_s = v * 3600,
            ("remove_worktrees_after_days", Ok(v)) => c.remove_worktrees_after_s = v * 86400,
            ("clean_dirs", _) => {
                let home = std::env::var("HOME").unwrap_or_default();
                c.clean_dirs = value
                    .split(',')
                    .map(|d| d.trim())
                    .filter(|d| !d.is_empty())
                    .map(|d| PathBuf::from(d.strip_prefix("~/").map_or(d.to_string(), |r| format!("{home}/{r}"))))
                    .collect();
            }
            ("memory_brake", _) => match value {
                "slow" => c.pause_for_memory = false,
                "pause" => c.pause_for_memory = true,
                _ => c.warnings.push(format!("config line {}: `memory_brake` is `slow` or `pause`", n + 1)),
            },
            ("dev" | "build" | "test" | "install" | "ignore", _) => {
                for tool in value.split(',').map(|t| t.trim().to_ascii_lowercase()).filter(|t| !t.is_empty()) {
                    c.custom.insert(tool, Kind::parse(key));
                }
            }
            (_, Err(_))
                if matches!(
                    key,
                    "heavy_jobs"
                        | "cpus"
                        | "idle_sleep_minutes"
                        | "idle_sleep_seconds"
                        | "max_wait_seconds"
                        | "leftover_busy_minutes"
                        | "leftover_busy_seconds"
                        | "agent_cpu_percent"
                        | "clean_caches_after_hours"
                        | "remove_worktrees_after_days"
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
