//! Coding-agent sessions (Claude Code, Codex) left open for days: each holds 200–300 MB plus its
//! helpers (MCP servers), and editors keep dozens. Idle ones are listed in `nodoka status`; with
//! `close_idle_agents_after_hours` set they're asked to exit while memory is short. The chat is
//! saved as it goes, so a closed session can be resumed.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

/// Program names of agent sessions.
// ponytail: matched on the process name; an agent that runs as `node cli.js` isn't seen
pub const NAMES: &[&str] = &["claude", "codex"];

/// Used more than 2% of a core: an idle session's own timers use under 1%.
const BUSY_NS_PER_S: u64 = 20_000_000;
/// CPU is judged over at least this long, so one timer firing doesn't count as work.
const WINDOW_S: u64 = 60;

pub struct Session {
    pub idle_since: u64,
    cpu: u64,
    at: u64,
    tree: HashSet<i32>,
    pub mem: u64,
    pub terminal: bool,
    /// Already asked to exit.
    pub asked: bool,
}

impl Session {
    /// `last_seen`: its last known activity, if older than now (a session open before nodoka started).
    pub fn new(cpu: u64, tree: HashSet<i32>, t: u64, terminal: bool, last_seen: Option<u64>) -> Self {
        let idle_since = last_seen.map_or(t, |s| s.min(t));
        Session { idle_since, cpu, at: t, tree, mem: 0, terminal, asked: false }
    }

    /// Working = it, or anything it started, used CPU, or it started a program (a tool call).
    pub fn sample(&mut self, cpu: u64, tree: HashSet<i32>, t: u64) {
        let used = cpu.saturating_sub(self.cpu);
        let busy = used > t.saturating_sub(self.at).max(WINDOW_S) * BUSY_NS_PER_S
            || tree.iter().any(|p| !self.tree.contains(p));
        if busy {
            self.idle_since = t;
        }
        if busy || t.saturating_sub(self.at) >= WINDOW_S {
            (self.cpu, self.at) = (cpu, t);
        }
        self.tree = tree;
    }

    pub fn idle_for(&self, t: u64) -> u64 {
        t.saturating_sub(self.idle_since)
    }

    /// Closed only when it's switched on, memory is short, and nobody is typing to it in a terminal.
    pub fn should_close(&self, t: u64, after_s: u64, level: u32) -> bool {
        after_s > 0 && level >= 2 && !self.terminal && !self.asked && self.idle_for(t) >= after_s
    }
}

/// When a Claude Code session last wrote to its chat (seconds since the epoch). It keeps
/// `sessions/<pid>.json` naming the chat, saved as `projects/<folder>/<id>.jsonl`.
pub fn last_chat_write(pid: i32, env: &[String]) -> Option<u64> {
    let dir = match env.iter().find_map(|e| e.strip_prefix("CLAUDE_CONFIG_DIR=")) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(env.iter().find_map(|e| e.strip_prefix("HOME="))?).join(".claude"),
    };
    let text = fs::read_to_string(dir.join("sessions").join(format!("{pid}.json"))).ok()?;
    // ponytail: one string field, read without a JSON parser
    let id = text.split("\"sessionId\":\"").nth(1)?.split('"').next()?;
    let chat = format!("{id}.jsonl");
    fs::read_dir(dir.join("projects")).ok()?.flatten().find_map(|d| {
        let m = fs::metadata(d.path().join(&chat)).ok()?.modified().ok()?;
        Some(m.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(p: &[i32]) -> HashSet<i32> {
        p.iter().copied().collect()
    }

    #[test]
    fn an_agent_idle_past_the_limit_is_closed_once_and_only_while_memory_is_short() {
        let day = 86_400;
        let mut s = Session::new(0, tree(&[10, 11]), 0, false, None);
        // its timers: 0.5% of a core
        for t in (60..=day).step_by(60) {
            s.sample(t * 5_000_000, tree(&[10, 11]), t);
        }
        assert_eq!(s.idle_for(day), day);
        assert!(!s.should_close(day, 0, 3), "off unless switched on");
        assert!(!s.should_close(day, day, 1), "memory is fine: left alone");
        assert!(!s.should_close(day - 1, day, 3), "not idle long enough yet");
        assert!(s.should_close(day, day, 2));
        s.asked = true;
        assert!(!s.should_close(day + 60, day, 3), "asked once");

        let mut term = Session::new(0, tree(&[20]), 0, true, None);
        term.sample(0, tree(&[20]), day);
        assert!(!term.should_close(day, day, 3), "a terminal session is the user's");
    }

    #[test]
    fn a_tool_call_or_cpu_use_restarts_the_idle_clock() {
        let mut s = Session::new(0, tree(&[10]), 0, false, None);
        s.sample(0, tree(&[10, 12]), 100);
        assert_eq!(s.idle_since, 100, "it started a program");
        s.sample(0, tree(&[10]), 200);
        assert_eq!(s.idle_since, 100, "a program ending is not work");
        s.sample(5_000_000_000, tree(&[10]), 205);
        assert_eq!(s.idle_since, 205, "5 s of CPU in 5 s");
        s.sample(5_000_000_000 + 1_000_000_000, tree(&[10]), 300);
        assert_eq!(s.idle_since, 205, "1 s of CPU over 95 s is idle");
    }

    #[test]
    fn a_claude_session_open_before_nodoka_started_is_idle_since_its_last_chat_write() {
        let dir = std::env::temp_dir().join(format!("nodoka-claude-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sessions")).unwrap();
        fs::create_dir_all(dir.join("projects/-work-app")).unwrap();
        fs::write(dir.join("sessions/4242.json"), r#"{"pid":4242,"sessionId":"abc-1","cwd":"/work/app"}"#).unwrap();
        let chat = fs::File::create(dir.join("projects/-work-app/abc-1.jsonl")).unwrap();
        let two_days_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86_400);
        chat.set_modified(two_days_ago).unwrap();
        let env = vec![format!("CLAUDE_CONFIG_DIR={}", dir.display())];

        let last = last_chat_write(4242, &env);
        let now = crate::sys::now();
        assert!(last.is_some_and(|l| now - l >= 2 * 86_400 - 5), "{last:?}");
        let s = Session::new(0, tree(&[4242]), now, false, last);
        assert!(s.should_close(now, 86_400, 2), "idle two days: no need to wait another day");
        assert_eq!(last_chat_write(4243, &env), None, "another process: unknown");
        assert_eq!(Session::new(0, tree(&[1]), 100, false, Some(500)).idle_since, 100, "never in the future");
        fs::remove_dir_all(&dir).ok();
    }
}
