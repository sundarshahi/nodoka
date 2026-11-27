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
    /// Memory of the whole tree in bytes (Activity Monitor's "Memory", swapped-out part included).
    pub mem: u64,
    /// Memory this command needed the last time it ran in this folder (0: not known yet).
    pub expect: u64,
    /// Waited the longest a job is ever held for memory: it runs to the end and isn't paused again.
    pub turn: bool,
    /// In the slow lane: runs on spare CPU and disk time while memory is critically low.
    pub slow: bool,
    /// A one-shot test run (not a watcher, not a browser): stopped once it sits idle too long.
    pub runner: bool,
    /// Sent a stop signal; not sent again.
    pub ended: bool,
    /// Long work found by how it behaves, not by a rule (a map build in Java): never asked to exit;
    /// paused or slowed, it is always resumed.
    pub keep: bool,
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
            mem: 0,
            expect: 0,
            turn: false,
            slow: false,
            runner: false,
            ended: false,
            keep: false,
        }
    }
}

#[derive(Default)]
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
    /// Memory the system can hand out without swapping, in bytes.
    pub avail: u64,
    /// At critical memory, pause busy builds (opt-in) instead of moving them to the slow lane.
    pub pause_for_memory: bool,
    /// Swapping so hard everything waits on the disk (the Mac freezes): even the slow lane pauses.
    pub freezing: bool,
    /// A test run that used no CPU this long finished but never exited: stop it. 0: never.
    pub stop_idle_tests_s: u64,
    /// A test run an agent started that has run this long while memory is critical: stop it. 0: never.
    pub stop_long_tests_s: u64,
    /// Last time memory was critical (0: never). Set by `decide`.
    pub last_critical: u64,
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
    Slow(usize, bool),
    /// Ask it to exit (SIGTERM).
    Stop(usize),
}

pub const ADMIT_WINDOW_S: u64 = 3;
pub const MAX_PRESSURE_PAUSE_S: u64 = 600;
pub const PRESSURE_STEP_S: u64 = 5;
pub const CALM_BEFORE_RESUME_S: u64 = 20;

/// Needed less than this last time: starts at once and takes no slot. Half of 212 rubocop runs used
/// under 100 MB, yet seven queued behind tests and builds, and an editor hook that lints each edited
/// file gives up after 60 s, the same time a queued job waits for its turn.
pub const LIGHT_JOB: u64 = 512 << 20;

fn light(j: &Job) -> bool {
    j.expect > 0 && j.expect < LIGHT_JOB
}

/// Each job always runs at least this share of every second: slowed, never hung.
pub const MIN_DUTY: f64 = 0.25;

/// CPU budget for agent work: the share of each second it may run so that, together, it uses at
/// most `budget` cores. `used` is the cores it used last second while running `duty` of it.
pub fn duty(used: f64, duty: f64, budget: f64) -> f64 {
    let demand = used / duty.max(MIN_DUTY);
    if demand <= budget { 1.0 } else { (budget / demand).max(MIN_DUTY) }
}

/// Whether a job counts toward the agent CPU budget: running, started without a terminal, and not
/// a test run (pausing tests, even briefly, eats into their timeouts).
pub fn shares_budget(j: &Job) -> bool {
    j.state == State::Running && !j.terminal && j.pausable && j.kind != Kind::Test
}

/// Memory is short and this job needed more last time than the system can hand out now.
/// When memory became short and stayed so: critical now, with no calm stretch of
/// `CALM_BEFORE_RESUME_S` since. A Mac flips critical → warn → critical every few seconds
/// for hours (98 flips in one evening), so a clock reset by every flip never runs out. 0: not short.
pub fn short_since(prev: u64, level: u32, last_critical: u64, now: u64) -> u64 {
    if level >= 4 {
        if prev == 0 { now } else { prev }
    } else if now.saturating_sub(last_critical) >= CALM_BEFORE_RESUME_S {
        0
    } else {
        prev
    }
}

pub fn short_of_memory(j: &Job, level: u32, room: u64) -> bool {
    level >= 2 && j.expect > room
}

pub fn decide(jobs: &mut [Job], sys: &mut Sys) -> Vec<Action> {
    let now = sys.now;
    let level = sys.level;
    // memory not yet promised to a job started or resumed this tick
    let mut room = sys.avail;
    let cap = sys.cap();
    let calm = level < 4 && now.saturating_sub(sys.level_since) >= CALM_BEFORE_RESUME_S;
    if level >= 4 {
        sys.last_critical = now;
    }
    // critical now or under 20 s ago: it flips to warn for a second or two
    let tight = sys.last_critical > 0 && now.saturating_sub(sys.last_critical) < CALM_BEFORE_RESUME_S;
    let mut out = Vec::new();
    let counts =
        |j: &Job| j.kind.heavy() && !light(j) && j.state == State::Running && j.admitted && (j.active || j.age_s < 30);
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
        // A job that needed more memory last time than is free now waits before it starts, while it
        // holds next to nothing: paused later, a 3.4 GB type check keeps all 3.4 GB.
        let fits = !short_of_memory(j, level, room);
        if light(j) && fits {
            j.admitted = true; // no slot: the cap is for heavy work, memory still decides
        } else if !j.pausable || j.kind == Kind::Test || j.age_s > ADMIT_WINDOW_S || running < cap && fits {
            j.admitted = true;
            running += 1;
            room = room.saturating_sub(j.expect);
        } else {
            set(j, State::Queued);
            out.push(Action::Pause(i));
        }
    }

    // 2. queued jobs start first-in first-out; one that waited too long starts anyway (one waiting
    //    for memory gets as long as a job paused for memory)
    let mut queued: Vec<usize> = order.iter().copied().filter(|&i| jobs[i].state == State::Queued).collect();
    queued.sort_by_key(|&i| jobs[i].state_since);
    for i in queued {
        let j = &mut jobs[i];
        let short = short_of_memory(j, level, room);
        // While memory is critical (or was, under 20 s ago: it flips to warn for a second or two) no
        // new heavy job starts for up to 10 minutes, whatever the brake, known size or not: a third
        // 3.9 GB type check was about to start after a minute while a 16 GB Mac swapped 240 MB/s.
        // While the machine freezes nothing starts. Waiting before the start holds next to nothing.
        let long = tight || short && sys.pause_for_memory;
        let wait = if long { MAX_PRESSURE_PAUSE_S } else { sys.max_wait_s };
        let overdue = now.saturating_sub(j.state_since) >= wait && !sys.freezing;
        let free = running < cap && !short && !tight;
        if overdue && tight && !j.terminal {
            // its agent stopped waiting long ago (after about 10 minutes); started now it would only
            // add to a machine that's short of memory. Asked to exit; it can be run again.
            set(j, State::Running);
            j.ended = true;
            out.push(Action::Stop(i));
            continue;
        }
        if free || overdue {
            set(j, State::Running);
            j.admitted = true;
            running += 1;
            room = room.saturating_sub(j.expect);
            out.push(Action::Resume(i));
            if !free && !j.bg && !j.terminal {
                j.bg = true;
                out.push(Action::Bg(i, true));
            }
        }
    }

    // 3. memory brake: only at critical pressure (warn can be the normal state on a busy laptop).
    //    Busy builds only. Clocks keep running while a process is paused, so a paused test run would
    //    fail its per-test timeouts (5s in Jest and Vitest) and a paused install its downloads; and
    //    pausing an idle job (a watcher between rebuilds) stops nothing from growing.
    //    The last busy build too: a lone 9 GB `next build` froze a 16 GB Mac while nodoka watched.
    //    A job that has waited MAX_PRESSURE_PAUSE_S gets its turn, even while memory stays critical:
    //    it runs to the end at low priority and isn't paused again, one such job at a time. Paused,
    //    it holds its memory for good and its agent waits forever: two type checks held 7 GB for 3
    //    hours, resumed every 10 minutes only to be paused again 5 seconds later.
    //    By default nothing is paused for memory: a paused job keeps all its memory and its agent
    //    waits. Busy builds move to the slow lane instead, newest first, one every 5 seconds; they
    //    keep running and finish, and leave the slow lane one by one after 20 calm seconds.
    //    Except while the machine is freezing (swapping 100+ MB/s): a slowed build still touches all
    //    its memory, and a 4 GB type check kept a 16 GB Mac swapping 205 MB/s. Then it is paused, as
    //    with `memory_brake = pause`, and resumed or given its turn by the same rules.
    let since_act = now.saturating_sub(sys.last_pressure_act);
    let pausing = sys.pause_for_memory || sys.freezing || jobs.iter().any(|j| j.state == State::PressurePaused);
    if !pausing && since_act >= PRESSURE_STEP_S {
        let busy = |j: &Job| j.state == State::Running && j.pausable && j.active && j.kind == Kind::Build && !j.ended;
        let step = if sys.level >= 4 {
            order.iter().rev().copied().find(|&i| busy(&jobs[i]) && !jobs[i].slow).map(|i| (i, true))
        } else if calm {
            order.iter().copied().find(|&i| jobs[i].slow).map(|i| (i, false))
        } else {
            None
        };
        if let Some((i, on)) = step {
            jobs[i].slow = on;
            out.push(Action::Slow(i, on));
            sys.last_pressure_act = now;
        }
    } else if since_act >= PRESSURE_STEP_S {
        let mut paused: Vec<usize> =
            order.iter().copied().filter(|&i| jobs[i].state == State::PressurePaused && !jobs[i].ended).collect();
        paused.sort_by_key(|&i| jobs[i].state_since);
        let turn_running = jobs.iter().any(|j| j.turn && j.state == State::Running);
        let oldest = paused.first().copied();
        let busy = |j: &Job| {
            j.state == State::Running && j.pausable && j.active && j.kind == Kind::Build && !j.turn && !j.ended
        };
        let held_long = |i: &usize| now.saturating_sub(jobs[*i].state_since) >= MAX_PRESSURE_PAUSE_S;
        let agents = paused.iter().copied().find(|i| held_long(i) && !jobs[*i].terminal && !jobs[*i].keep);
        let overdue = oldest.filter(held_long);
        if let Some(i) = agents {
            // An agent's build held 10 minutes: its agent stopped waiting long ago (after about 10
            // minutes), paused it keeps all its memory (three type checks held 12.6 GB), and its turn
            // would add a running build to a machine short of memory (a 3.8 GB one ran through a
            // 240 MB/s freeze). It's asked to exit; it can be run again. One every step, so each
            // one's memory shows before the next goes. Running again: the daemon re-pauses anything
            // in a paused tree each tick, and `npm exec` re-paused while passing the signal on would hang.
            set(&mut jobs[i], State::Running);
            jobs[i].ended = true;
            out.push(Action::Stop(i));
            sys.last_pressure_act = now;
        } else if let Some(i) = overdue.filter(|_| !turn_running && !sys.freezing) {
            let j = &mut jobs[i];
            set(j, State::Running);
            j.turn = true;
            out.push(Action::Resume(i));
            if !j.bg && !j.terminal {
                j.bg = true;
                out.push(Action::Bg(i, true));
            }
            sys.last_pressure_act = now;
        } else if if sys.pause_for_memory { sys.level >= 4 } else { sys.freezing } {
            if let Some(i) = order.iter().rev().copied().find(|&i| busy(&jobs[i])) {
                set(&mut jobs[i], State::PressurePaused);
                out.push(Action::Pause(i));
                sys.last_pressure_act = now;
            }
        } else if let Some(i) = oldest {
            // Resume only once there's room for all of it. Calm alone isn't enough: pausing the build
            // is what calmed memory, so it went critical again 3s after resuming (a lone 5 GB type
            // check flapped every 25s).
            // A build resumed or started in the last minute is still pulling its memory back in, and
            // the free memory doesn't show it yet: three 4 GB type checks resumed 5 s apart, each
            // seeing the room the one before was about to take.
            let arriving: u64 = jobs
                .iter()
                .filter(|j| j.state == State::Running && j.kind.heavy() && now.saturating_sub(j.state_since) < 60)
                .map(|j| j.mem.max(j.expect))
                .sum();
            if calm && room >= jobs[i].mem + arriving {
                set(&mut jobs[i], State::Running);
                out.push(Action::Resume(i));
                sys.last_pressure_act = now;
            }
        }
    }

    // 4. a test run an agent started that used no CPU for `stop_idle_tests_s` finished but never
    //    exited (Jest with an open handle): it only holds memory. Four held 2.9 GB for up to 78 minutes
    //    while a Mac swapped 205 MB/s. Terminal runs are the user's to see and stop.
    for &i in &order {
        let j = &mut jobs[i];
        let agents = j.runner && !j.terminal && j.state == State::Running;
        let hung = sys.stop_idle_tests_s > 0 && j.idle_for_s >= sys.stop_idle_tests_s;
        // A busy one that runs on while memory is critical: an agent gives up on a command after
        // about 10 minutes, and a Jest run kept 4 workers busy for 43 minutes on a Mac swapping 38 MB/s.
        let long = sys.stop_long_tests_s > 0 && tight && j.age_s >= sys.stop_long_tests_s;
        if agents && (hung || long) && !j.ended {
            j.ended = true; // ponytail: SIGTERM once, no SIGKILL follow-up; add one if runs ignore it
            out.push(Action::Stop(i));
        }
    }

    // 5. idle servers sleep; a waiting connection or request wakes them. An idle build server
    //    (Gradle, Kotlin, Bazel daemon) is asked to exit instead: paused, it kept 4.7 GB in swap for
    //    hours and could never reach its own idle exit. The next build starts a fresh one.
    for &i in &order {
        let j = &mut jobs[i];
        let idle =
            j.state == State::Running && sys.idle_s > 0 && j.sleepable && j.pausable && j.idle_for_s >= sys.idle_s;
        if idle && j.kind == Kind::Build && !j.terminal && !j.keep && !j.ended {
            j.ended = true;
            out.push(Action::Stop(i));
        } else if idle && j.kind != Kind::Build {
            set(j, State::IdlePaused);
            out.push(Action::Pause(i));
        } else if j.state == State::IdlePaused && j.wake {
            set(j, State::Running);
            out.push(Action::Resume(i));
        }
    }

    // 6. all cores busy → background jobs yield: nice 10, kept until the job ends (an ordinary user
    //    can't raise priority back). Dev servers too: one an agent started and drives (a demo recorder
    //    held a Rails server at 100%) shouldn't outrank your apps. Never terminal jobs: someone is
    //    watching those.
    for &i in &order {
        let j = &mut jobs[i];
        if sys.cpu_busy && !j.bg && !j.terminal && j.state == State::Running {
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
            avail: 8 << 30,
            pause_for_memory: false,
            freezing: false,
            stop_idle_tests_s: 1800,
            stop_long_tests_s: 1800,
            last_critical: 0,
        }
    }

    #[test]
    fn a_job_that_needed_little_memory_last_time_starts_at_once_and_takes_no_slot() {
        let mut lint = job(Kind::Build, 0);
        lint.expect = 100 << 20; // rubocop on a few files
        let mut tsc = job(Kind::Build, 0);
        tsc.expect = 5 << 30;
        let mut jobs = vec![job(Kind::Test, 5), job(Kind::Test, 5), job(Kind::Test, 5), lint.clone(), lint, tsc];
        let mut s = sys(100, 1, 3);
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(5)], "lint runs while tests fill the slots");
        let mut unknown = job(Kind::Build, 0);
        unknown.expect = 0;
        let mut jobs = vec![job(Kind::Test, 5), job(Kind::Test, 5), job(Kind::Test, 5), unknown];
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(3)], "never measured: it waits like before");
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
    fn critical_pauses_busy_builds_newest_first_and_resumes_when_there_is_room() {
        let mut idle_watcher = job(Kind::Build, 20); // e.g. `webpack --watch` between rebuilds
        idle_watcher.active = false;
        let mut jobs =
            vec![job(Kind::Build, 60), job(Kind::Build, 50), job(Kind::Test, 40), job(Kind::Install, 30), idle_watcher];
        jobs.iter_mut().for_each(|j| j.admitted = true);
        let mut s = sys(100, 4, 3);
        s.pause_for_memory = true;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(1)], "newest busy build first");
        s.now = 102; // too soon for another step
        assert!(decide(&mut jobs, &mut s).is_empty());
        s.now = 106;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(0)], "the last busy build too: one froze a Mac");
        s.now = 112;
        assert!(decide(&mut jobs, &mut s).is_empty(), "tests, installs and idle jobs are never paused for memory");
        s.level = 2; // pressure eases: resume the longest-paused first, after a calm period
        s.level_since = 112;
        s.now = 120;
        assert!(decide(&mut jobs, &mut s).is_empty());
        jobs[1].mem = 9 << 30; // but not while there's no room for it: it would push memory straight back
        s.now = 132;
        assert!(decide(&mut jobs, &mut s).is_empty(), "calm came from pausing it; resuming would flap");
        s.avail = 10 << 30;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Resume(1)]);
        s.avail = 0;
        s.now = 106 + MAX_PRESSURE_PAUSE_S;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Stop(0)], "never held for good: its agent stopped waiting");
    }

    #[test]
    fn a_build_started_in_a_terminal_held_the_longest_runs_to_the_end_even_while_memory_stays_critical() {
        let mut jobs = vec![job(Kind::Build, 60), job(Kind::Build, 50)];
        jobs.iter_mut().for_each(|j| (j.admitted, j.terminal) = (true, true)); // someone is watching
        let mut s = sys(100, 4, 3);
        s.pause_for_memory = true;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(1)]);
        s.now = 105;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(0)]);
        s.now = 100 + MAX_PRESSURE_PAUSE_S; // still critical
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Resume(1)], "its turn (never lowered: a terminal job)");
        for now in [705, 710, 800, 2000] {
            s.now = now;
            assert!(decide(&mut jobs, &mut s).is_empty(), "not paused again, and the next waits its turn");
        }
        jobs.remove(1); // it finished
        s.now = 2005;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Resume(0)], "then the next");
    }

    #[test]
    fn memory_flipping_between_critical_and_warn_counts_as_short_until_it_stays_calm() {
        let mut since = 0;
        let mut last_critical = 0;
        // critical 5 s, warn 3 s, over and over, for 3 minutes
        for t in 1000..1180 {
            let level = if t % 8 < 5 { 4 } else { 2 };
            if level >= 4 {
                last_critical = t;
            }
            since = short_since(since, level, last_critical, t);
        }
        assert_eq!(since, 1000, "the flips never reset it");
        for t in 1180..1180 + CALM_BEFORE_RESUME_S {
            since = short_since(since, 2, last_critical, t);
        }
        assert_eq!(since, 0, "20 calm seconds do");
    }

    #[test]
    fn long_work_found_by_its_load_is_paused_while_freezing_and_resumed_never_asked_to_exit() {
        let mut jobs = vec![job(Kind::Build, 60)];
        (jobs[0].admitted, jobs[0].keep) = (true, true);
        let mut s = sys(100, 4, 3);
        s.freezing = true;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(0)]);
        for now in [400, 100 + MAX_PRESSURE_PAUSE_S, 1500] {
            s.now = now;
            assert!(decide(&mut jobs, &mut s).is_empty(), "still freezing: it waits, it is never stopped");
        }
        s.freezing = false;
        s.now = 1505;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Resume(0), Action::Bg(0, true)], "its turn: it finishes");
        assert!(!jobs[0].ended);
    }

    #[test]
    fn an_agents_build_held_ten_minutes_is_asked_to_exit_not_given_a_turn() {
        let mut jobs = vec![job(Kind::Build, 60), job(Kind::Build, 50)];
        jobs.iter_mut().for_each(|j| j.admitted = true);
        jobs[0].terminal = true;
        let mut s = sys(100, 4, 3);
        s.freezing = true;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(1)]);
        s.now = 105;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(0)]);
        s.now = 100 + MAX_PRESSURE_PAUSE_S;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Stop(1)], "no turn while freezing");
        assert_eq!(jobs[1].state, State::Running, "never paused again while it exits");
        s.now += PRESSURE_STEP_S;
        assert!(decide(&mut jobs, &mut s).is_empty(), "asked once; a terminal build is the user's");
        s.freezing = false; // critical, no longer freezing: the terminal build gets its turn
        s.now += PRESSURE_STEP_S;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Resume(0)]);

        // not freezing at all, just critical: same for an agent's build
        let mut jobs = vec![job(Kind::Build, 60)];
        jobs[0].admitted = true;
        let mut s = sys(100, 4, 3);
        s.freezing = true;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(0)]);
        s.freezing = false;
        s.now = 100 + MAX_PRESSURE_PAUSE_S;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Stop(0)]);
        s.now += PRESSURE_STEP_S;
        assert!(decide(&mut jobs, &mut s).is_empty(), "nor slowed while it exits");
    }

    #[test]
    fn at_critical_memory_a_job_that_wont_fit_waits_ten_minutes_and_never_starts_while_freezing() {
        let mut tsc = job(Kind::Build, 0);
        tsc.expect = 4 << 30;
        let mut jobs = vec![tsc];
        let mut s = sys(100, 4, 5); // default brake (slow lane)
        s.avail = 1 << 30;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(0)]);
        s.now = 100 + s.max_wait_s;
        assert!(decide(&mut jobs, &mut s).is_empty(), "critical: not after a minute");
        s.now = 100 + MAX_PRESSURE_PAUSE_S;
        s.freezing = true;
        assert!(decide(&mut jobs, &mut s).is_empty(), "never into a freezing machine");
        s.freezing = false;
        s.now += 1;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Stop(0)], "an agent's, still critical: asked to exit");
    }

    #[test]
    fn at_critical_memory_no_new_build_starts_for_ten_minutes_even_one_of_unknown_size() {
        let mut jobs = vec![job(Kind::Build, 0)]; // never seen before: expect 0
        let mut s = sys(100, 4, 3);
        s.level_since = 90;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(0)]);
        for now in 101..=100 + s.max_wait_s {
            s.now = now; // a tick a second, critical throughout
            assert!(decide(&mut jobs, &mut s).is_empty(), "critical: not after a minute ({now})");
        }
        s.level = 2; // a blip to warn
        s.level_since = s.now;
        s.now += 2;
        assert!(decide(&mut jobs, &mut s).is_empty(), "nor on a 2 s blip to warn");
        s.now += CALM_BEFORE_RESUME_S;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Resume(0)], "calm: room for it");
    }

    #[test]
    fn an_agents_test_run_still_busy_after_30_minutes_is_stopped_only_while_memory_is_critical() {
        let mut jest = job(Kind::Test, 1800);
        jest.runner = true;
        jest.admitted = true;
        let mut mine = jest.clone();
        mine.terminal = true;
        let mut young = jest.clone();
        young.age_s = 1700;
        let mut jobs = vec![jest, mine, young];
        let mut s = sys(100, 2, 3);
        assert!(decide(&mut jobs, &mut s).is_empty(), "memory isn't critical: a long suite is fine");
        s.level = 4;
        s.now = 101;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Stop(0)], "never a terminal run or a younger one");
        s.now = 102;
        assert!(decide(&mut jobs, &mut s).is_empty(), "asked once");
    }

    #[test]
    fn paused_builds_are_resumed_one_a_minute_while_the_last_one_pulls_its_memory_back() {
        let mut jobs = vec![job(Kind::Build, 60), job(Kind::Build, 50)];
        jobs.iter_mut().for_each(|j| (j.admitted, j.terminal, j.mem) = (true, true, 4 << 30));
        let mut s = sys(100, 4, 3);
        s.freezing = true;
        decide(&mut jobs, &mut s);
        s.now = 105;
        decide(&mut jobs, &mut s);
        assert!(jobs.iter().all(|j| j.state == State::PressurePaused));
        (s.freezing, s.level, s.level_since, s.avail) = (false, 2, 110, 5 << 30);
        s.now = 130;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Resume(1)], "paused first, resumed first");
        s.now = 135; // free memory doesn't show the first one coming back yet
        assert!(decide(&mut jobs, &mut s).is_empty(), "not both at once");
        s.now = 191;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Resume(0)], "a minute later, if there's still room");
    }

    #[test]
    fn a_job_that_needed_more_memory_than_is_free_waits_before_it_starts() {
        let mut tsc = job(Kind::Build, 0);
        tsc.expect = 3 << 30; // a type check that took 3 GB last time here
        let mut vitest = job(Kind::Test, 0);
        vitest.expect = 3 << 30;
        let mut jobs = vec![tsc.clone(), vitest, job(Kind::Build, 0)];
        let mut s = sys(100, 2, 5);
        s.pause_for_memory = true;
        s.avail = 1 << 30;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(0)], "tests never wait; unknown jobs start");
        s.now = 100 + s.max_wait_s;
        assert!(decide(&mut jobs, &mut s).is_empty(), "waiting for memory outlasts the slot wait");
        s.avail = 4 << 30;
        s.now += 1;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Resume(0)], "starts once there's room, at full priority");

        // two waiting for the same room: one starts, the other keeps waiting, at most 10 minutes
        let mut jobs = vec![tsc.clone(), tsc.clone()];
        let mut s = sys(100, 2, 5);
        s.pause_for_memory = true;
        s.avail = 1 << 30;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(0), Action::Pause(1)]);
        s.avail = 4 << 30;
        s.now = 101;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Resume(0)]);
        s.avail = 1 << 30; // the first one holds it now
        s.now = 102;
        assert!(decide(&mut jobs, &mut s).is_empty());
        s.now = 100 + MAX_PRESSURE_PAUSE_S;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Resume(1), Action::Bg(1, true)], "never held for good");

        // by default it waits only as long as for a slot
        let mut jobs = vec![tsc];
        let mut s = sys(100, 2, 5);
        s.avail = 1 << 30;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(0)]);
        s.now = 100 + s.max_wait_s;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Resume(0), Action::Bg(0, true)]);

        // memory isn't short: the record doesn't matter
        let mut jobs = vec![job(Kind::Build, 0)];
        jobs[0].expect = 9 << 30;
        assert!(decide(&mut jobs, &mut sys(100, 1, 3)).is_empty());
    }

    #[test]
    fn critical_memory_moves_busy_builds_to_the_slow_lane_and_never_pauses_them() {
        let mut idle_watcher = job(Kind::Build, 20);
        idle_watcher.active = false;
        let mut jobs = vec![job(Kind::Build, 60), job(Kind::Build, 50), job(Kind::Test, 40), idle_watcher];
        jobs.iter_mut().for_each(|j| j.admitted = true);
        let mut s = sys(100, 4, 3);
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Slow(1, true)], "newest busy build first");
        s.now = 102;
        assert!(decide(&mut jobs, &mut s).is_empty(), "one step every 5 seconds");
        s.now = 105;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Slow(0, true)], "the last build too");
        for now in [110, 600, 5000] {
            s.now = now;
            assert!(decide(&mut jobs, &mut s).is_empty(), "tests and idle jobs stay; nothing is paused");
        }
        assert!(jobs.iter().all(|j| j.state == State::Running));
        s.level = 2;
        s.level_since = 5000;
        s.now = 5010;
        assert!(decide(&mut jobs, &mut s).is_empty(), "after a calm period");
        s.now = 5020;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Slow(0, false)]);
        s.now = 5025;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Slow(1, false)]);
    }

    #[test]
    fn freezing_pauses_a_slowed_build_until_there_is_room() {
        let mut jobs = vec![job(Kind::Build, 60), job(Kind::Test, 50)];
        jobs.iter_mut().for_each(|j| j.admitted = true);
        let mut s = sys(100, 4, 3);
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Slow(0, true)], "critical: slow lane");
        s.freezing = true;
        s.now = 105;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Pause(0)], "freezing: paused; tests never");
        s.freezing = false; // swapping eased but memory is still critical: stays paused
        s.now = 200;
        assert!(decide(&mut jobs, &mut s).is_empty());
        s.level = 2;
        s.level_since = 200;
        s.now = 225;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Resume(0)], "calm and room: resumed");
        s.now = 230;
        assert_eq!(
            decide(&mut jobs, &mut s),
            vec![Action::Slow(0, false)],
            "then the slow-lane rules: calm, full speed"
        );
    }

    #[test]
    fn a_test_run_idle_too_long_is_stopped_once_but_never_a_watcher_browser_or_terminal_run() {
        let mut hung = job(Kind::Test, 4000);
        (hung.runner, hung.idle_for_s) = (true, 1800);
        let mut browser = hung.clone();
        browser.runner = false; // a headless browser waits on its agent
        let mut mine = hung.clone();
        mine.terminal = true;
        let mut busy = hung.clone();
        busy.idle_for_s = 60;
        let mut jobs = vec![hung, browser, mine, busy];
        let mut s = sys(5000, 1, 4);
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Stop(0)]);
        s.now = 5001;
        assert!(decide(&mut jobs, &mut s).is_empty(), "sent once");
        s.stop_idle_tests_s = 0;
        jobs[3].idle_for_s = 99_999;
        assert!(decide(&mut jobs, &mut s).is_empty(), "0 turns it off");
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
    fn an_idle_build_server_is_asked_to_exit_not_paused_where_it_would_keep_its_memory() {
        let mut gradle = job(Kind::Build, 3000);
        (gradle.sleepable, gradle.idle_for_s) = (true, 601);
        let mut jobs = vec![gradle];
        let mut s = sys(5000, 1, 2);
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Stop(0)]);
        assert!(decide(&mut jobs, &mut s).is_empty(), "asked once");
    }

    #[test]
    fn busy_cpu_backgrounds_agent_jobs_only() {
        let mut t = job(Kind::Build, 60);
        t.terminal = true;
        let mut jobs = vec![job(Kind::Build, 60), job(Kind::Dev, 60), t];
        let mut s = sys(100, 1, 3);
        s.cpu_busy = true;
        assert_eq!(decide(&mut jobs, &mut s), vec![Action::Bg(0, true), Action::Bg(1, true)], "an agent's server too");
        assert!(decide(&mut jobs, &mut s).is_empty(), "lowered once, never the terminal job");
        s.cpu_busy = false;
        s.now = 200;
        assert!(decide(&mut jobs, &mut s).is_empty(), "stays lowered: it can't be raised without root");
    }

    #[test]
    fn agent_work_is_held_to_its_cpu_share_but_never_stopped_for_good() {
        assert_eq!(duty(2.0, 1.0, 5.0), 1.0, "under budget: full speed");
        assert!((duty(8.0, 1.0, 4.0) - 0.5).abs() < 1e-9, "twice the budget: half of each second");
        assert!((duty(2.0, 0.5, 4.0) - 1.0).abs() < 1e-9, "demand fell to the budget: full speed again");
        assert_eq!(duty(40.0, 1.0, 1.0), MIN_DUTY, "always some progress");
        let mut t = job(Kind::Build, 60);
        assert!(shares_budget(&t));
        t.terminal = true;
        assert!(!shares_budget(&t), "what you started in a terminal is never sliced");
        assert!(!shares_budget(&job(Kind::Test, 60)), "nor test runs");
        assert!(shares_budget(&job(Kind::Dev, 60)));
    }
}
