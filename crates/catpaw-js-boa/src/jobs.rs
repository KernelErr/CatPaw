//! The job queue: promise reactions and other engine jobs, run at the
//! page's microtask checkpoints.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context as TaskContext, Waker};

use boa_engine::job::{GenericJob, Job, JobExecutor, NativeAsyncJob, PromiseJob};
use boa_engine::{Context, JsResult, JsValue};

/// How many rounds pending native futures are polled, with nothing moving
/// and nothing arriving from outside, before they are given up on.
const MAX_IDLE_ROUNDS: usize = 10_000;

/// What the executor does while every native future waits for something
/// from outside the engine (the response a module's import is loaded
/// from): brings in what has arrived, waiting a little for it, and says
/// whether anything did.
pub type IdleHook = Box<dyn Fn(&mut Context) -> bool>;

/// A [`JobExecutor`] that only runs jobs when the embedder asks for a
/// microtask checkpoint. Timers are not handled here: the page has its own.
#[derive(Default)]
pub struct Jobs {
    promise_jobs: RefCell<VecDeque<PromiseJob>>,
    async_jobs: RefCell<VecDeque<NativeAsyncJob>>,
    generic_jobs: RefCell<VecDeque<GenericJob>>,
    idle: RefCell<Option<IdleHook>>,
    /// Whether native futures are being run: a checkpoint within that run
    /// (from a callback the idle hook delivered) leaves new futures to it,
    /// rather than nest runs until the stack is gone.
    running_async: Cell<bool>,
}

impl Jobs {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    /// Sets what runs while native futures wait for the outside; without
    /// it they are polled until they are given up on.
    pub fn set_idle(&self, hook: IdleHook) {
        *self.idle.borrow_mut() = Some(hook);
    }

    /// Queues a microtask.
    pub fn enqueue_microtask(&self, job: PromiseJob) {
        self.promise_jobs.borrow_mut().push_back(job);
    }

    pub fn is_empty(&self) -> bool {
        self.promise_jobs.borrow().is_empty()
            && self.async_jobs.borrow().is_empty()
            && self.generic_jobs.borrow().is_empty()
    }

    fn run_promise_jobs(&self, context: &mut Context) {
        loop {
            let job = self.promise_jobs.borrow_mut().pop_front();
            let Some(job) = job else { break };
            // A promise job reports failures through the promise it settles;
            // an error here is an engine-level abort of that one job.
            let _ = job.call(context);
        }
    }

    /// Runs the native futures queued so far side by side, to completion:
    /// the imports of a module are loaded together, as a browser loads a
    /// module graph, and a module that finishes loading has its own
    /// imports join in. Promise jobs run between polls; while every future
    /// waits for the outside, the idle hook brings it in.
    fn run_async_jobs(&self, context: &mut Context) {
        self.running_async.set(true);
        self.run_async_jobs_inner(context);
        self.running_async.set(false);
    }

    fn run_async_jobs_inner(&self, context: &mut Context) {
        let cell = RefCell::new(context);
        let mut active: Vec<Pin<Box<dyn Future<Output = JsResult<JsValue>> + '_>>> = Vec::new();
        let mut task = TaskContext::from_waker(Waker::noop());
        let mut idle_rounds = 0;
        loop {
            loop {
                let job = self.async_jobs.borrow_mut().pop_front();
                let Some(job) = job else { break };
                active.push(Box::pin(job.call(&cell)));
            }
            if active.is_empty() {
                return;
            }
            let before = active.len();
            active.retain_mut(|future| future.as_mut().poll(&mut task).is_pending());
            let progressed = active.len() != before;
            self.run_promise_jobs(&mut cell.borrow_mut());
            if progressed || !self.async_jobs.borrow().is_empty() {
                idle_rounds = 0;
                continue;
            }
            let delivered = match &*self.idle.borrow() {
                Some(hook) => hook(&mut cell.borrow_mut()),
                None => false,
            };
            if delivered {
                idle_rounds = 0;
                continue;
            }
            idle_rounds += 1;
            if idle_rounds >= MAX_IDLE_ROUNDS {
                // Given up on: dropped unfinished.
                return;
            }
        }
    }

    /// Performs a microtask checkpoint: runs jobs until none are left,
    /// including jobs queued by the jobs it runs.
    pub fn checkpoint(&self, context: &mut Context) {
        loop {
            self.run_promise_jobs(context);
            let generic = self.generic_jobs.borrow_mut().pop_front();
            if let Some(job) = generic {
                let _ = job.call(context);
                continue;
            }
            if !self.async_jobs.borrow().is_empty() {
                if self.running_async.get() {
                    // The run under way picks them up on its next round.
                    break;
                }
                self.run_async_jobs(context);
                continue;
            }
            if self.promise_jobs.borrow().is_empty() {
                break;
            }
        }
    }
}

impl JobExecutor for Jobs {
    fn enqueue_job(self: Rc<Self>, job: Job, _context: &mut Context) {
        match job {
            Job::PromiseJob(job) => self.promise_jobs.borrow_mut().push_back(job),
            Job::AsyncJob(job) => self.async_jobs.borrow_mut().push_back(job),
            Job::GenericJob(job) => self.generic_jobs.borrow_mut().push_back(job),
            // Engine-level timers (`Atomics.waitAsync` and the like) are not
            // supported; the page's own timers do not come through here.
            _ => {}
        }
    }

    fn run_jobs(self: Rc<Self>, context: &mut Context) -> JsResult<()> {
        self.checkpoint(context);
        Ok(())
    }
}
