//! The job queue: promise reactions and other engine jobs, run at the
//! page's microtask checkpoints.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context as TaskContext, Waker};

use boa_engine::job::{GenericJob, Job, JobExecutor, NativeAsyncJob, PromiseJob};
use boa_engine::{Context, JsResult};

/// How often a pending native future is polled before it is given up on.
/// Host futures here never wait for an outside wake-up; they only wait for
/// promise jobs, which are run between polls.
const MAX_ASYNC_POLLS: usize = 10_000;

/// A [`JobExecutor`] that only runs jobs when the embedder asks for a
/// microtask checkpoint. Timers are not handled here: the page has its own.
#[derive(Default)]
pub struct Jobs {
    promise_jobs: RefCell<VecDeque<PromiseJob>>,
    async_jobs: RefCell<VecDeque<NativeAsyncJob>>,
    generic_jobs: RefCell<VecDeque<GenericJob>>,
}

impl Jobs {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
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

    fn run_async_job(&self, job: NativeAsyncJob, context: &mut Context) {
        let cell = RefCell::new(context);
        let mut future = pin!(job.call(&cell));
        let mut task = TaskContext::from_waker(Waker::noop());
        for _ in 0..MAX_ASYNC_POLLS {
            if future.as_mut().poll(&mut task).is_ready() {
                return;
            }
            self.run_promise_jobs(&mut cell.borrow_mut());
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
            let pending = self.async_jobs.borrow_mut().pop_front();
            if let Some(job) = pending {
                self.run_async_job(job, context);
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
