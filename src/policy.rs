//! What to do about each job this tick. Pure: no OS calls, so every rule is unit-tested.

use crate::classify::Kind;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Running,
    Queued,
    PressurePaused,
    IdlePaused,
}

#[derive(Clone, Debug)]
pub struct Job {
    pub kind: Kind,
    pub age_s: u64,
    /// Started from a terminal (has a controlling tty). Such jobs are never lowered in priority,
    /// and nodoka never stops the process a shell is waiting on (see `daemon::protected`).
    pub terminal: bool,
    /// Something in the tree can be stopped without a shell noticing.
    pub pausable: bool,
    /// Has a listening TCP socket and isn't a production server: may sleep when idle.
    pub sleepable: bool,
    /// A connection or request is waiting for this (paused) job.
    pub wake: bool,
    pub active: bool,
    pub idle_for_s: u64,
    pub admitted: bool,
    pub state: State,
    pub state_since: u64,
    pub bg: bool,
}

impl Job {
    pub fn new(kind: Kind, now: u64) -> Job {
        Job {
            kind,
            age_s: 0,
            terminal: false,
            pausable: true,
            sleepable: false,
            wake: false,
            active: true,
            idle_for_s: 0,
            admitted: false,
            state: State::Running,
            state_since: now,
            bg: false,
        }
    }
}

pub struct Sys {
    pub now: u64,
    /// Memory pressure: 1 normal, 2 warn, 4 critical.
    pub level: u32,
    pub level_since: u64,
    pub cpu_busy: bool,
    /// Heavy jobs allowed at normal pressure.
    pub base_cap: usize,
    pub idle_s: u64, // 0 = idle sleep off
    pub max_wait_s: u64,
    pub last_pressure_act: u64,
}

impl Sys {
    pub fn cap(&self) -> usize {
        match self.level {
            1 => self.base_cap,
            2 => self.base_cap.div_ceil(2),
            _ => 0,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Pause(usize),
    Resume(usize),
    Bg(usize, bool),
}

pub const ADMIT_WINDOW_S: u64 = 3;
pub const MAX_PRESSURE_PAUSE_S: u64 = 600;
pub const PRESSURE_STEP_S: u64 = 5;
pub const CALM_BEFORE_RESUME_S: u64 = 20;

pub fn decide(jobs: &mut [Job], sys: &mut Sys) -> Vec<Action> {
    let now = sys.now;
    let cap = sys.cap();
    let mut out = Vec::new();
    let counts = |j: &Job| j.kind.heavy() && j.state == State::Running && j.admitted && (j.active || j.age_s < 30);
    let mut running = jobs.iter().filter(|j| counts(j)).count();
    let set = |j: &mut Job, s: State| {
        j.state = s;
        j.state_since = now;
    };

    // oldest first: stable FIFO order, and "who started first" wins
    let mut order: Vec<usize> = (0..jobs.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(jobs[i].age_s));

    // 1. admission: a heavy job that just started waits if enough heavy jobs already run
    for &i in &order {
        let j = &mut jobs[i];
        if j.admitted || !j.kind.heavy() || j.state != State::Running {
            continue;
        }
        // Test runs never wait: clocks keep running while a process is paused, and a real Vitest run
        // held at start failed 9 of 60 tests on their 5s timeouts. They still take a slot.
        if !j.pausable || j.kind == Kind::Test || j.age_s > ADMIT_WINDOW_S || running < cap {
            j.admitted = true;
            running += 1;
        } else {
            set(j, State::Queued);
            out.push(Action::Pause(i));
        }
    }

    // 2. queued jobs start first-in first-out; one that waited too long starts anyway
    let mut queued: Vec<usize> = order.iter().copied().filter(|&i| jobs[i].state == State::Queued).collect();
    queued.sort_by_key(|&i| jobs[i].state_since);
    for i in queued {
        let j = &mut jobs[i];
        let overdue = now.saturating_sub(j.state_since) >= sys.max_wait_s;
        if running < cap || overdue {
            set(j, State::Running);
            j.admitted = true;
            running += 1;
            out.push(Action::Resume(i));
            if overdue && !j.bg && !j.terminal {
                j.bg = true;
                out.push(Action::Bg(i, true));
            }
        }
    }

    // 3. memory brake: only at critical pressure (warn can be the normal state on a busy laptop).
    //    Busy builds only. Clocks keep running while a process is paused, so a paused test run would
    //    fail its per-test timeouts (5s in Jest and Vitest) and a paused install its downloads; and
    //    pausing an idle job (a watcher between rebuilds) stops nothing from growing.
    let since_act = now.saturating_sub(sys.last_pressure_act);
    if sys.level >= 4 && since_act >= PRESSURE_STEP_S {
        let victim = order.iter().rev().copied().find(|&i| {
            let j = &jobs[i];
            j.state == State::Running && j.pausable && j.active && j.kind == Kind::Build
        });
        if let Some(i) = victim {
            set(&mut jobs[i], State::PressurePaused);
            out.push(Action::Pause(i));
            sys.last_pressure_act = now;
        }
    } else if since_act >= PRESSURE_STEP_S {
        let calm = sys.level < 4 && now.saturating_sub(sys.level_since) >= CALM_BEFORE_RESUME_S;
        let mut paused: Vec<usize> =
            order.iter().copied().filter(|&i| jobs[i].state == State::PressurePaused).collect();
        paused.sort_by_key(|&i| jobs[i].state_since);
        if let Some(&i) = paused.first() {
            if calm || now.saturating_sub(jobs[i].state_since) >= MAX_PRESSURE_PAUSE_S {
                set(&mut jobs[i], State::Running);
                out.push(Action::Resume(i));
                sys.last_pressure_act = now;
            }
        }
    }

    // 4. idle servers sleep; a waiting connection or request wakes them
    for &i in &order {
        let j = &mut jobs[i];
        if j.state == State::Running && sys.idle_s > 0 && j.sleepable && j.pausable && j.idle_for_s >= sys.idle_s {
            set(j, State::IdlePaused);
            out.push(Action::Pause(i));
        } else if j.state == State::IdlePaused && j.wake {
            set(j, State::Running);
            out.push(Action::Resume(i));
        }
    }

    // 5. all cores busy → background heavy jobs yield: nice 10, kept until the job ends (an ordinary
    //    user can't raise priority back). Never terminal jobs: someone is watching those.
    for &i in &order {
        let j = &mut jobs[i];
        if sys.cpu_busy && !j.bg && j.kind.heavy() && !j.terminal && j.state == State::Running {
            j.bg = true;
            out.push(Action::Bg(i, true));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(kind: Kind, age_s: u64) -> Job {
        Job { age_s, ..Job::new(kind, 0) }
    }

    fn sys(now: u64, level: u32, cap: usize) -> Sys {
        Sys {
            now,
            level,
            level_since: 0,
            cpu_busy: false,
            base_cap: cap,
            idle_s: 600,
            max_wait_s: 60,
            last_pressure_act: 0,
        }
    }

    #[test]
    fn admission_queues_beyond_cap_then_fifo() {
        let mut jobs = vec![job(Kind::Build, 1), job(Kind::Install, 1), job(Kind::Build, 0)];
        let mut s = sys(100, 1, 2);
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(2)]);
        assert_eq!(jobs[2].state, State::Queued);
        jobs.remove(0); // first job finishes → a slot frees → the queued one resumes
        s.now = 105;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Resume(1)]);
        assert!(jobs[1].admitted);
    }

    #[test]
    fn test_runs_never_wait_but_take_a_slot() {
        let mut jobs = vec![job(Kind::Build, 5), job(Kind::Test, 0)];
        let mut s = sys(100, 1, 1);
        assert!(decide(&mut jobs, &mut s).is_empty(), "a paused test run would fail its timeouts");
        assert!(jobs[1].admitted && jobs[1].state == State::Running);
        jobs.push(job(Kind::Install, 0)); // slots are full (build + test): the install waits
        s.now = 101;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(2)]);
    }

    #[test]
    fn warn_pressure_halves_the_cap_rounding_up() {
        let s = sys(0, 2, 3);
        assert_eq!(s.cap(), 2);
        assert_eq!(sys(0, 4, 3).cap(), 0);
    }

    #[test]
    fn unpausable_and_already_running_jobs_are_never_queued() {
        let mut fg = job(Kind::Build, 0);
        fg.pausable = false; // e.g. typed straight at a shell prompt
        // cap 1 and a build already running: the new unpausable one still runs
        let mut jobs = vec![job(Kind::Build, 60), fg];
        assert!(decide(&mut jobs, &mut sys(100, 1, 1)).is_empty());
        assert!(jobs.iter().all(|j| j.admitted && j.state == State::Running));
    }

    #[test]
    fn overdue_queue_starts_in_background_but_not_for_terminal_jobs() {
        let mut t = job(Kind::Build, 0);
        t.terminal = true;
        let mut jobs = vec![job(Kind::Build, 1), job(Kind::Build, 0), t];
        let mut s = sys(10, 1, 1);
        decide(&mut jobs, &mut s);
        assert_eq!((jobs[1].state, jobs[2].state), (State::Queued, State::Queued));
        s.now = 10 + s.max_wait_s;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Resume(1), Action::Bg(1, true), Action::Resume(2)]);
    }

    #[test]
    fn critical_pauses_newest_busy_build_only_and_resumes_when_calm() {
        let mut idle_watcher = job(Kind::Build, 20); // e.g. `webpack --watch` between rebuilds
        idle_watcher.active = false;
        let mut jobs =
            vec![job(Kind::Build, 60), job(Kind::Build, 50), job(Kind::Test, 40), job(Kind::Install, 30), idle_watcher];
        jobs.iter_mut().for_each(|j| j.admitted = true);
        let mut s = sys(100, 4, 3);
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(1)], "newest busy build first");
        s.now = 102; // too soon for another step
        assert!(decide(&mut jobs, &mut s).is_empty());
        s.now = 106;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(0)]);
        s.now = 112;
        assert!(decide(&mut jobs, &mut s).is_empty(), "tests, installs and idle jobs are never paused for memory");
        s.level = 2; // pressure eases: resume the longest-paused first, after a calm period
        s.level_since = 112;
        s.now = 120;
        assert!(decide(&mut jobs, &mut s).is_empty());
        s.now = 132;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Resume(1)]);
    }

    #[test]
    fn idle_servers_sleep_and_wake_on_a_request() {
        let mut server = job(Kind::Dev, 3000);
        server.sleepable = true;
        server.idle_for_s = 601;
        let mut watcher = job(Kind::Dev, 3000); // e.g. `tsc --watch`: no socket, nothing could wake it
        watcher.idle_for_s = 601;
        let mut jobs = vec![server, watcher];
        let mut s = sys(5000, 1, 2);
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(0)]);
        assert!(decide(&mut jobs, &mut s).is_empty());
        jobs[0].wake = true;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Resume(0)]);
        s.idle_s = 0; // idle sleep switched off
        jobs[0].wake = false;
        assert!(decide(&mut jobs, &mut s).is_empty());
    }

    #[test]
    fn busy_cpu_backgrounds_agent_jobs_only() {
        let mut t = job(Kind::Build, 60);
        t.terminal = true;
        let mut jobs = vec![job(Kind::Build, 60), job(Kind::Dev, 60), t];
        let mut s = sys(100, 1, 3);
        s.cpu_busy = true;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Bg(0, true)]);
        assert!(decide(&mut jobs, &mut s).is_empty(), "lowered once");
        s.cpu_busy = false;
        s.now = 200;
        assert!(decide(&mut jobs, &mut s).is_empty(), "stays lowered: it can't be raised without root");
    }
}
