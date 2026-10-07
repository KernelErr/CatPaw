//! The event loop: tasks, timers, animation frames, and the driver that
//! runs them until the page is idle.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use catpaw_js::{Callback, Value};

use crate::net;
use crate::page::{Cx, PageState};

/// A unit of work run by the event loop with script quiescent.
pub struct Task {
    pub label: &'static str,
    pub run: Box<dyn FnOnce(&mut Cx<'_>)>,
}

/// Runs the next queued task, if there is one, and performs a microtask
/// checkpoint after it.
pub fn run_one_task(cx: &mut Cx<'_>) -> bool {
    let task = cx.page.tasks.borrow_mut().pop_front();
    let Some(task) = task else {
        return false;
    };
    (task.run)(cx);
    cx.checkpoint();
    true
}

/// Queues `f` to run as a task.
pub fn queue_task(page: &PageState, label: &'static str, f: impl FnOnce(&mut Cx<'_>) + 'static) {
    page.tasks.borrow_mut().push_back(Task {
        label,
        run: Box::new(f),
    });
}

#[derive(Clone)]
pub enum TimerAction {
    Call(Callback, Vec<Value>),
    /// A string handler, evaluated as a classic script.
    Eval(String),
    /// Work scheduled by the platform itself (request timeouts and such).
    Native(std::rc::Rc<dyn Fn(&mut Cx<'_>)>),
}

pub struct Timer {
    pub id: i32,
    pub action: TimerAction,
    /// `Some` for `setInterval`: the repeat interval in milliseconds.
    pub interval: Option<f64>,
    pub nesting: u32,
}

/// Pending timers, ordered by deadline and then by creation.
#[derive(Default)]
pub struct Timers {
    next_id: i32,
    seq: u64,
    by_time: BTreeMap<(u64, u64), Timer>,
    index: HashMap<i32, (u64, u64)>,
}

fn micros(ms: f64) -> u64 {
    (ms.max(0.0) * 1000.0).round() as u64
}

impl Timers {
    fn insert(&mut self, deadline_ms: f64, timer: Timer) {
        self.seq += 1;
        let key = (micros(deadline_ms), self.seq);
        self.index.insert(timer.id, key);
        self.by_time.insert(key, timer);
    }

    fn remove(&mut self, id: i32) {
        if let Some(key) = self.index.remove(&id) {
            self.by_time.remove(&key);
        }
    }

    /// The earliest deadline, in milliseconds since the time origin.
    pub fn next_deadline(&self) -> Option<f64> {
        self.by_time.keys().next().map(|(t, _)| *t as f64 / 1000.0)
    }

    fn pop_due(&mut self, now_ms: f64) -> Option<(f64, Timer)> {
        let (&key, _) = self.by_time.iter().next()?;
        if key.0 > micros(now_ms) {
            return None;
        }
        let timer = self.by_time.remove(&key)?;
        self.index.remove(&timer.id);
        Some((key.0 as f64 / 1000.0, timer))
    }

    pub fn len(&self) -> usize {
        self.by_time.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_time.is_empty()
    }
}

/// Timers nested deeper than this are clamped to [`NESTED_TIMER_MIN_MS`].
const TIMER_NESTING_CLAMP: u32 = 5;
const NESTED_TIMER_MIN_MS: f64 = 4.0;

fn clamp_delay(delay: f64, nesting: u32) -> f64 {
    if nesting > TIMER_NESTING_CLAMP && delay < NESTED_TIMER_MIN_MS {
        NESTED_TIMER_MIN_MS
    } else {
        delay
    }
}

/// `setTimeout` / `setInterval`.
pub fn set_timer(page: &PageState, action: TimerAction, delay_ms: i32, repeat: bool) -> i32 {
    let nesting = page.timer_nesting.get() + 1;
    let delay = clamp_delay(f64::from(delay_ms.max(0)), nesting);
    let mut timers = page.timers.borrow_mut();
    timers.next_id += 1;
    let id = timers.next_id;
    let deadline = page.clock.peek() + delay;
    timers.insert(
        deadline,
        Timer {
            id,
            action,
            interval: repeat.then_some(f64::from(delay_ms.max(0))),
            nesting,
        },
    );
    id
}

/// `clearTimeout` / `clearInterval` (they share one id space).
pub fn clear_timer(page: &PageState, id: i32) {
    page.timers.borrow_mut().remove(id);
}

fn run_timer(cx: &mut Cx<'_>, fired_at: f64, timer: Timer) {
    // Re-arm an interval before running it, so the callback can clear it.
    if let Some(interval) = timer.interval {
        let nesting = timer.nesting + 1;
        let next = fired_at + clamp_delay(interval, nesting).max(0.001);
        cx.page.timers.borrow_mut().insert(
            next,
            Timer {
                id: timer.id,
                action: timer.action.clone(),
                interval: timer.interval,
                nesting,
            },
        );
    }
    cx.page.timer_nesting.set(timer.nesting);
    let result = match &timer.action {
        TimerAction::Call(callback, args) => {
            cx.script.call(callback, &Value::Window, args).map(drop)
        }
        TimerAction::Eval(source) => {
            let url = cx.page.url.borrow().to_string();
            cx.script.eval_script(source, &url, 1).map(drop)
        }
        TimerAction::Native(run) => {
            run(cx);
            Ok(())
        }
    };
    cx.page.timer_nesting.set(0);
    if let Err(e) = result {
        cx.report_exception(&e);
    }
    cx.checkpoint();
}

/// The length of an animation frame, in milliseconds.
const FRAME_MS: f64 = 1000.0 / 60.0;

#[derive(Default)]
pub struct RafState {
    next_id: u32,
    callbacks: Vec<(u32, Callback)>,
    /// When the next frame runs, if anything is waiting for it.
    deadline: Option<f64>,
    /// Callbacks of the frame being run that were cancelled before their turn.
    cancelled: Vec<u32>,
}

/// Makes sure a frame is coming: animation frame callbacks, then the
/// observers that look at the rendered document.
pub(crate) fn request_frame(page: &PageState) {
    let mut raf = page.raf.borrow_mut();
    if raf.deadline.is_none() {
        let now = page.clock.peek();
        raf.deadline = Some(((now / FRAME_MS).floor() + 1.0) * FRAME_MS);
    }
}

pub fn request_animation_frame(page: &PageState, callback: Callback) -> u32 {
    let id = {
        let mut raf = page.raf.borrow_mut();
        raf.next_id += 1;
        let id = raf.next_id;
        raf.callbacks.push((id, callback));
        id
    };
    request_frame(page);
    id
}

pub fn cancel_animation_frame(page: &PageState, id: u32) {
    let mut raf = page.raf.borrow_mut();
    raf.callbacks.retain(|(i, _)| *i != id);
    raf.cancelled.push(id);
}

fn run_frame(cx: &mut Cx<'_>) {
    let callbacks = {
        let mut raf = cx.page.raf.borrow_mut();
        raf.deadline = None;
        raf.cancelled.clear();
        std::mem::take(&mut raf.callbacks)
    };
    let timestamp = Value::Number(cx.page.clock.peek());
    for (id, callback) in callbacks {
        // An earlier callback of this frame may have cancelled this one.
        if cx.page.raf.borrow().cancelled.contains(&id) {
            continue;
        }
        if let Err(e) = cx
            .script
            .call(&callback, &Value::Window, std::slice::from_ref(&timestamp))
        {
            cx.report_exception(&e);
        }
    }
    cx.checkpoint();
    crate::intersection_observer::update(cx);
    crate::resize_observer::update(cx);
}

/// Bounds on one run of the event loop.
#[derive(Clone, Debug)]
pub struct LoopLimits {
    /// Real time the run may take.
    pub wall: Duration,
    /// Virtual time the run may skip ahead, in milliseconds.
    pub virtual_ms: f64,
    /// Number of tasks, timers and frames the run may execute.
    pub max_steps: u64,
}

impl Default for LoopLimits {
    fn default() -> Self {
        Self {
            wall: Duration::from_secs(15),
            virtual_ms: 10_000.0,
            max_steps: 200_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// Nothing left to do: no tasks, timers, frames or requests.
    Idle,
    WallBudget,
    VirtualBudget,
    StepBudget,
    /// Script asked to navigate away.
    Navigation,
}

#[derive(Clone, Debug)]
pub struct LoopReport {
    pub stop: StopReason,
    pub steps: u64,
    pub virtual_advanced_ms: f64,
    pub pending_timers: usize,
    pub inflight_requests: usize,
}

/// How long the loop listens to open WebSockets with nothing else to do
/// before the page counts as idle.
const SOCKET_GRACE: Duration = Duration::from_secs(1);

/// Runs the event loop until the page is idle or a limit is reached.
pub fn run(cx: &mut Cx<'_>, limits: &LoopLimits) -> LoopReport {
    let started = Instant::now();
    let mut steps = 0u64;
    let mut advanced = 0.0f64;
    // Real time spent listening to sockets since the last step.
    let mut socket_silence = Duration::ZERO;
    let mut steps_at_silence = 0u64;
    // Whatever ran before the loop (a script evaluated by the embedder,
    // say) may have queued microtasks; they run before the loop can be
    // found idle.
    cx.checkpoint();

    let stop = loop {
        if cx.page.navigation.borrow().is_some() {
            break StopReason::Navigation;
        }
        if started.elapsed() >= limits.wall {
            break StopReason::WallBudget;
        }
        if steps >= limits.max_steps {
            break StopReason::StepBudget;
        }

        if net::deliver(cx, None) > 0 {
            steps += 1;
            continue;
        }

        let task = cx.page.tasks.borrow_mut().pop_front();
        if let Some(task) = task {
            (task.run)(cx);
            cx.checkpoint();
            steps += 1;
            continue;
        }

        let now = cx.page.clock.peek();
        let due_at = cx
            .page
            .timers
            .borrow()
            .next_deadline()
            .filter(|t| *t <= now);
        if let Some(due_at) = due_at
            && cx.page.clock.is_virtual()
            && let Some(wait) = net::real_time_before(cx.page, due_at)
        {
            // The clock ran ahead of real time while a response was on
            // its way: give the response the time it would have had.
            let wait = wait.min(limits.wall.saturating_sub(started.elapsed()));
            if net::deliver(cx, Some(wait)) > 0 {
                steps += 1;
                continue;
            }
        }
        let due = cx.page.timers.borrow_mut().pop_due(now);
        if let Some((fired_at, timer)) = due {
            run_timer(cx, fired_at, timer);
            steps += 1;
            continue;
        }

        crate::intersection_observer::request_frame_if_stale(cx.page);
        crate::resize_observer::request_frame_if_stale(cx.page);
        let frame = cx.page.raf.borrow().deadline;
        if frame.is_some_and(|t| t <= now) {
            run_frame(cx);
            steps += 1;
            continue;
        }

        // Nothing is runnable right now: wait for whatever comes next.
        let next_timer = cx.page.timers.borrow().next_deadline();
        let next = match (next_timer, frame) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let remaining = limits.wall.saturating_sub(started.elapsed());

        if net::inflight(cx.page) > 0 {
            // The network runs in real time. While waiting for it, a virtual
            // clock follows real time instead of jumping ahead: timers then
            // neither overtake a response that is about to arrive nor stall
            // behind one that never does.
            let mut wait = Duration::from_millis(50).min(remaining);
            if let Some(t) = next {
                wait = wait.min(Duration::from_secs_f64(((t - now) / 1000.0).max(0.0)));
            }
            let waiting_since = Instant::now();
            if net::deliver(cx, Some(wait)) == 0 && cx.page.clock.is_virtual() {
                let waited = waiting_since.elapsed().as_secs_f64() * 1000.0;
                cx.page.clock.advance_to(now + waited.max(0.001));
            }
            continue;
        }

        // An open WebSocket may speak at any time, but nothing says when:
        // the loop listens a while after the last step, then counts the
        // page as idle.
        if steps != steps_at_silence {
            steps_at_silence = steps;
            socket_silence = Duration::ZERO;
        }
        if net::open_sockets(cx.page) > 0 && socket_silence < SOCKET_GRACE {
            let mut wait = Duration::from_millis(50).min(remaining);
            if let Some(t) = next {
                wait = wait.min(Duration::from_secs_f64(((t - now) / 1000.0).max(0.0)));
            }
            let waiting_since = Instant::now();
            let delivered = net::deliver(cx, Some(wait));
            let waited = waiting_since.elapsed();
            if delivered > 0 {
                steps += 1;
            } else {
                socket_silence += waited;
                if cx.page.clock.is_virtual() {
                    cx.page
                        .clock
                        .advance_to(now + (waited.as_secs_f64() * 1000.0).max(0.001));
                }
            }
            continue;
        }

        let Some(next) = next else {
            break StopReason::Idle;
        };
        if cx.page.clock.is_virtual() {
            let jump = (next - now).max(0.0);
            if advanced + jump > limits.virtual_ms {
                break StopReason::VirtualBudget;
            }
            advanced += jump;
            cx.page.clock.advance_to(next);
        } else {
            let wait = Duration::from_secs_f64(((next - now) / 1000.0).max(0.0));
            if wait >= remaining {
                std::thread::sleep(remaining);
                break StopReason::WallBudget;
            }
            std::thread::sleep(wait);
        }
    };

    LoopReport {
        stop,
        steps,
        virtual_advanced_ms: advanced,
        pending_timers: cx.page.timers.borrow().len(),
        inflight_requests: net::inflight(cx.page),
    }
}
