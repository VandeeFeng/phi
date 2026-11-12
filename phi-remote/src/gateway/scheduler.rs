use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::{Notify, watch};
use tokio::time::Instant;

#[derive(Clone, Copy, PartialEq)]
enum WakeState {
    Waiting,
    Ready,
    Closed,
}

struct Entry {
    deadline: Option<Instant>,
    signal: Arc<watch::Sender<WakeState>>,
    scope: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    entries: HashMap<usize, Entry>,
    closed: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Notify,
}

/// Shared Gateway deadlines; consumers retain ownership of the work they wake.
#[derive(Clone)]
pub struct GatewayScheduler {
    shared: Arc<Shared>,
    scope: Arc<AtomicBool>,
}

/// One coalesced wakeup. Dropping it cancels its outstanding deadline.
pub struct ScheduledWake {
    shared: Arc<Shared>,
    signal: Arc<watch::Sender<WakeState>>,
    receiver: watch::Receiver<WakeState>,
    scope: Arc<AtomicBool>,
}

impl GatewayScheduler {
    pub(super) fn start(shutdown: watch::Receiver<bool>) -> (Self, tokio::task::JoinHandle<()>) {
        let scheduler = Self {
            shared: Arc::new(Shared::default()),
            scope: Arc::new(AtomicBool::new(false)),
        };
        let driver = scheduler.clone();
        let task = tokio::spawn(async move { driver.run(shutdown).await });
        (scheduler, task)
    }

    pub(crate) fn scope(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            scope: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn close_scope(&self) {
        let mut state = self.shared.state.lock().expect("scheduler lock poisoned");
        self.scope.store(true, Ordering::Release);
        state.entries.retain(|_, entry| {
            if Arc::ptr_eq(&entry.scope, &self.scope) {
                entry.signal.send_replace(WakeState::Closed);
                false
            } else {
                true
            }
        });
        drop(state);
        self.shared.changed.notify_one();
    }

    pub fn wakeup(&self) -> ScheduledWake {
        let (signal, receiver) = watch::channel(WakeState::Waiting);
        let signal = Arc::new(signal);
        let mut state = self.shared.state.lock().expect("scheduler lock poisoned");
        if state.closed || self.scope.load(Ordering::Acquire) {
            signal.send_replace(WakeState::Closed);
        } else {
            state.entries.insert(
                Arc::as_ptr(&signal) as usize,
                Entry {
                    deadline: None,
                    signal: signal.clone(),
                    scope: self.scope.clone(),
                },
            );
        }
        ScheduledWake {
            shared: self.shared.clone(),
            signal,
            receiver,
            scope: self.scope.clone(),
        }
    }

    async fn run(&self, mut shutdown: watch::Receiver<bool>) {
        loop {
            let deadline = {
                let state = self.shared.state.lock().expect("scheduler lock poisoned");
                if state.closed {
                    return;
                }
                state
                    .entries
                    .values()
                    .filter_map(|entry| entry.deadline)
                    .min()
            };
            let wait = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = shutdown.wait_for(|closed| *closed) => break,
                _ = self.shared.changed.notified() => {},
                _ = wait => {
                    let now = Instant::now();
                    let mut state = self.shared.state.lock().expect("scheduler lock poisoned");
                    for entry in state.entries.values_mut() {
                        if entry.deadline.is_some_and(|deadline| deadline <= now) {
                            entry.deadline = None;
                            entry.signal.send_replace(WakeState::Ready);
                        }
                    }
                }
            }
        }
        self.close();
    }

    pub(super) fn close(&self) {
        let mut state = self.shared.state.lock().expect("scheduler lock poisoned");
        state.closed = true;
        for (_, entry) in state.entries.drain() {
            entry.signal.send_replace(WakeState::Closed);
        }
        drop(state);
        self.shared.changed.notify_one();
    }
}

impl ScheduledWake {
    pub fn arm(&self, deadline: Instant) {
        let mut state = self.shared.state.lock().expect("scheduler lock poisoned");
        if let Some(entry) = state.entries.get_mut(&(Arc::as_ptr(&self.signal) as usize)) {
            entry.deadline = Some(deadline);
            entry.signal.send_replace(WakeState::Waiting);
        }
        drop(state);
        self.shared.changed.notify_one();
    }

    pub fn cancel(&self) {
        let mut state = self.shared.state.lock().expect("scheduler lock poisoned");
        if let Some(entry) = state.entries.get_mut(&(Arc::as_ptr(&self.signal) as usize)) {
            entry.deadline = None;
            entry.signal.send_replace(WakeState::Waiting);
        }
        drop(state);
        self.shared.changed.notify_one();
    }

    /// Returns `None` when this plugin run or Gateway scheduling has stopped.
    pub async fn wait(&mut self) -> Option<()> {
        loop {
            self.receiver
                .wait_for(|state| *state != WakeState::Waiting)
                .await
                .ok()?;
            let state = self.shared.state.lock().expect("scheduler lock poisoned");
            if state.closed || self.scope.load(Ordering::Acquire) {
                return None;
            }
            if *self.signal.borrow() == WakeState::Ready {
                self.signal.send_replace(WakeState::Waiting);
                return Some(());
            }
        }
    }
}

impl Drop for ScheduledWake {
    fn drop(&mut self) {
        self.shared
            .state
            .lock()
            .expect("scheduler lock poisoned")
            .entries
            .remove(&(Arc::as_ptr(&self.signal) as usize));
        self.shared.changed.notify_one();
    }
}

#[cfg(test)]
pub(crate) fn test_scheduler(
    shutdown: watch::Receiver<bool>,
) -> (GatewayScheduler, tokio::task::JoinHandle<()>) {
    GatewayScheduler::start(shutdown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn deadlines_rearm_cancel_and_coalesce() {
        let (shutdown, receiver) = watch::channel(false);
        let (scheduler, task) = GatewayScheduler::start(receiver);
        let mut first = scheduler.wakeup();
        let mut second = scheduler.wakeup();
        let started = Instant::now();
        first.arm(started + Duration::from_secs(60));
        second.arm(started + Duration::from_secs(10));
        first.arm(started + Duration::from_secs(10));
        assert_eq!(first.wait().await, Some(()));
        assert_eq!(second.wait().await, Some(()));
        assert_eq!(Instant::now() - started, Duration::from_secs(10));
        assert!(
            tokio::time::timeout(Duration::from_secs(100), first.wait())
                .await
                .is_err()
        );
        first.arm(Instant::now());
        first.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(100), first.wait())
                .await
                .is_err()
        );
        first.arm(Instant::now());
        assert_eq!(first.wait().await, Some(()));
        shutdown.send(true).unwrap();
        task.await.unwrap();
        assert_eq!(first.wait().await, None);
        assert_eq!(scheduler.wakeup().wait().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn closed_scope_retires_retained_handles_without_stopping_siblings() {
        let (shutdown, receiver) = watch::channel(false);
        let (scheduler, task) = GatewayScheduler::start(receiver);
        let first = scheduler.scope();
        let retained = first.clone();
        let sibling = scheduler.scope();
        let mut wakeup = first.wakeup();
        wakeup.arm(Instant::now());
        first.close_scope();
        assert_eq!(wakeup.wait().await, None);
        assert_eq!(retained.wakeup().wait().await, None);
        let mut other = sibling.wakeup();
        other.arm(Instant::now());
        assert_eq!(other.wait().await, Some(()));
        shutdown.send(true).unwrap();
        task.await.unwrap();
        assert_eq!(other.wait().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_and_dropped_wakeups_leave_no_deadlines() {
        let (shutdown, receiver) = watch::channel(false);
        let (scheduler, task) = GatewayScheduler::start(receiver);
        let idle = scheduler.wakeup();
        let dropped = scheduler.wakeup();
        dropped.arm(Instant::now() + Duration::from_secs(1));
        drop(dropped);
        tokio::task::yield_now().await;
        let state = scheduler.shared.state.lock().unwrap();
        assert_eq!(state.entries.len(), 1);
        assert!(state.entries.values().all(|entry| entry.deadline.is_none()));
        drop(state);
        shutdown.send(true).unwrap();
        task.await.unwrap();
        drop(idle);
    }
}
