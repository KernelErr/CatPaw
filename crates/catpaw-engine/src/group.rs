//! A browsing-context group on a thread of its own.
//!
//! Pages are not `Send`: a page and everything built over it (refs,
//! snapshot history) stay on the thread that created them. A
//! [`GroupHandle`] owns such a thread and runs closures on its state, one
//! at a time, from any other thread. A closure that panics takes the group
//! down (the state may be half-updated), not the caller.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;

use crate::page::{EngineError, PAGE_STACK_SIZE};

type Job<S> = Box<dyn FnOnce(&mut S) + Send>;

/// A thread holding a state `S` and running jobs on it.
pub struct GroupHandle<S> {
    jobs: Option<Sender<Job<S>>>,
    thread: Option<JoinHandle<()>>,
    alive: Arc<AtomicBool>,
}

impl<S: 'static> GroupHandle<S> {
    /// Starts the thread and builds the state on it with `init`. Fails when
    /// the thread cannot start or `init` fails.
    pub fn spawn(
        name: &str,
        init: impl FnOnce() -> Result<S, EngineError> + Send + 'static,
    ) -> Result<Self, EngineError> {
        let (jobs_tx, jobs_rx) = channel::<Job<S>>();
        let (ready_tx, ready_rx) = channel::<Result<(), EngineError>>();
        let alive = Arc::new(AtomicBool::new(true));
        let thread_alive = alive.clone();
        let thread = std::thread::Builder::new()
            .name(name.to_string())
            .stack_size(PAGE_STACK_SIZE)
            .spawn(move || {
                let mut state = match catch_unwind(AssertUnwindSafe(init)) {
                    Ok(Ok(state)) => {
                        let _ = ready_tx.send(Ok(()));
                        state
                    }
                    Ok(Err(e)) => {
                        thread_alive.store(false, Ordering::SeqCst);
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                    Err(_) => {
                        thread_alive.store(false, Ordering::SeqCst);
                        let _ = ready_tx.send(Err(EngineError::Panicked));
                        return;
                    }
                };
                while let Ok(job) = jobs_rx.recv() {
                    job(&mut state);
                    // A job that panicked marked the group dead before
                    // replying; its state may be half-updated, so stop.
                    if !thread_alive.load(Ordering::SeqCst) {
                        return;
                    }
                }
            })
            .map_err(EngineError::Thread)?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                jobs: Some(jobs_tx),
                thread: Some(thread),
                alive,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(EngineError::Panicked)
            }
        }
    }

    /// Runs `f` on the state and waits for its result.
    pub fn call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut S) -> R + Send + 'static,
    ) -> Result<R, EngineError> {
        let jobs = self.jobs.as_ref().ok_or(EngineError::Panicked)?;
        run_job(jobs, &self.alive, f)
    }

    /// Whether the thread is still running (no job panicked).
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// A way to run jobs on the group from another thread, which does not
    /// keep the group open: once the handle is dropped, its calls fail.
    pub fn caller(&self) -> Option<GroupCaller<S>> {
        Some(GroupCaller {
            jobs: self.jobs.as_ref()?.clone(),
            alive: self.alive.clone(),
        })
    }
}

/// Runs jobs on a group from any thread (see [`GroupHandle::caller`]).
pub struct GroupCaller<S> {
    jobs: Sender<Job<S>>,
    alive: Arc<AtomicBool>,
}

impl<S> Clone for GroupCaller<S> {
    fn clone(&self) -> Self {
        Self {
            jobs: self.jobs.clone(),
            alive: self.alive.clone(),
        }
    }
}

impl<S: 'static> GroupCaller<S> {
    /// Runs `f` on the group's state and waits for its result.
    pub fn call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut S) -> R + Send + 'static,
    ) -> Result<R, EngineError> {
        run_job(&self.jobs, &self.alive, f)
    }
}

fn run_job<S: 'static, R: Send + 'static>(
    jobs: &Sender<Job<S>>,
    alive: &Arc<AtomicBool>,
    f: impl FnOnce(&mut S) -> R + Send + 'static,
) -> Result<R, EngineError> {
    if !alive.load(Ordering::SeqCst) {
        return Err(EngineError::Panicked);
    }
    let (reply_tx, reply_rx) = channel::<Result<R, EngineError>>();
    let alive = alive.clone();
    let job: Job<S> =
        Box::new(
            move |state: &mut S| match catch_unwind(AssertUnwindSafe(|| f(state))) {
                Ok(result) => {
                    let _ = reply_tx.send(Ok(result));
                }
                Err(_) => {
                    alive.store(false, Ordering::SeqCst);
                    let _ = reply_tx.send(Err(EngineError::Panicked));
                }
            },
        );
    jobs.send(job).map_err(|_| EngineError::Panicked)?;
    reply_rx.recv().map_err(|_| EngineError::Panicked)?
}

impl<S> Drop for GroupHandle<S> {
    fn drop(&mut self) {
        // Callers may keep the channel open: the group is marked closed and
        // woken with a job that does nothing, after which its loop ends and
        // the state is dropped on its own thread.
        self.alive.store(false, Ordering::SeqCst);
        if let Some(jobs) = self.jobs.take() {
            let _ = jobs.send(Box::new(|_| {}));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[test]
    fn runs_jobs_on_a_state_that_is_not_send() {
        let group =
            GroupHandle::spawn("test-group", || Ok(Rc::new(std::cell::Cell::new(1)))).unwrap();
        group.call(|state| state.set(state.get() + 41)).unwrap();
        assert_eq!(group.call(|state| state.get()).unwrap(), 42);
    }

    #[test]
    fn a_panicking_job_takes_the_group_down_not_the_caller() {
        let group = GroupHandle::spawn("test-group", || Ok(0u32)).unwrap();
        let result = group.call(|_| -> u32 { panic!("boom") });
        assert!(matches!(result, Err(EngineError::Panicked)));
        assert!(!group.is_alive());
        assert!(matches!(group.call(|s| *s), Err(EngineError::Panicked)));
    }

    #[test]
    fn a_caller_works_from_another_thread_until_the_group_goes() {
        let group = GroupHandle::spawn("test-group", || Ok(1u32)).unwrap();
        let caller = group.caller().unwrap();
        let other = caller.clone();
        let seen = std::thread::spawn(move || other.call(|s| *s + 1).unwrap())
            .join()
            .unwrap();
        assert_eq!(seen, 2);
        drop(group);
        assert!(caller.call(|s| *s).is_err());
    }

    #[test]
    fn a_failing_init_is_reported() {
        let result =
            GroupHandle::<u32>::spawn("test-group", || Err(EngineError::Script("no".into())));
        assert!(matches!(result, Err(EngineError::Script(_))));
    }
}
