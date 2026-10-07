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
        if !self.is_alive() {
            return Err(EngineError::Panicked);
        }
        let jobs = self.jobs.as_ref().ok_or(EngineError::Panicked)?;
        let (reply_tx, reply_rx) = channel::<Result<R, EngineError>>();
        let alive = self.alive.clone();
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

    /// Whether the thread is still running (no job panicked).
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

impl<S> Drop for GroupHandle<S> {
    fn drop(&mut self) {
        // Closing the channel ends the thread's loop; the state is dropped
        // on its own thread.
        self.jobs.take();
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
    fn a_failing_init_is_reported() {
        let result =
            GroupHandle::<u32>::spawn("test-group", || Err(EngineError::Script("no".into())));
        assert!(matches!(result, Err(EngineError::Script(_))));
    }
}
