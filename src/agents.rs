//! Coding-agent sessions (Claude Code, Codex) left open for days: each holds 200–300 MB plus its
//! helpers (MCP servers), and editors keep dozens. Idle ones are listed in `nodoka status`; with
//! `close_idle_agents_after_hours` set they're asked to exit while memory is short. The chat is
//! saved as it goes, so a closed session can be resumed.

use std::collections::HashSet;

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
    pub fn new(cpu: u64, tree: HashSet<i32>, t: u64, terminal: bool) -> Self {
        Session { idle_since: t, cpu, at: t, tree, mem: 0, terminal, asked: false }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(p: &[i32]) -> HashSet<i32> {
        p.iter().copied().collect()
    }

    #[test]
    fn an_agent_idle_past_the_limit_is_closed_once_and_only_while_memory_is_short() {
        let day = 86_400;
        let mut s = Session::new(0, tree(&[10, 11]), 0, false);
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

        let mut term = Session::new(0, tree(&[20]), 0, true);
        term.sample(0, tree(&[20]), day);
        assert!(!term.should_close(day, day, 3), "a terminal session is the user's");
    }

    #[test]
    fn a_tool_call_or_cpu_use_restarts_the_idle_clock() {
        let mut s = Session::new(0, tree(&[10]), 0, false);
        s.sample(0, tree(&[10, 12]), 100);
        assert_eq!(s.idle_since, 100, "it started a program");
        s.sample(0, tree(&[10]), 200);
        assert_eq!(s.idle_since, 100, "a program ending is not work");
        s.sample(5_000_000_000, tree(&[10]), 205);
        assert_eq!(s.idle_since, 205, "5 s of CPU in 5 s");
        s.sample(5_000_000_000 + 1_000_000_000, tree(&[10]), 300);
        assert_eq!(s.idle_since, 205, "1 s of CPU over 95 s is idle");
    }
}
