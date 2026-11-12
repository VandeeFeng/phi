use std::time::Duration;

use tokio::time::Instant;

use crate::{GatewayScheduler, ScheduledWake};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

pub(super) enum WorkState {
    Idle,
    Pending,
}

pub(super) struct Refresh {
    wakeup: ScheduledWake,
    active: bool,
}

impl Refresh {
    pub(super) fn new(scheduler: &GatewayScheduler) -> Self {
        Self {
            wakeup: scheduler.wakeup(),
            active: false,
        }
    }

    pub(super) fn update(&mut self, state: WorkState) {
        match state {
            WorkState::Idle => {
                if self.active {
                    self.wakeup.cancel();
                    self.active = false;
                }
            }
            WorkState::Pending => {
                if !self.active {
                    self.wakeup.arm(Instant::now());
                    self.active = true;
                }
            }
        }
    }

    pub(super) async fn wait(&mut self) {
        if self.wakeup.wait().await.is_none() {
            return std::future::pending().await;
        }
        self.wakeup.arm(Instant::now() + REFRESH_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn idle_sleeps_and_pending_work_wakes_immediately() {
        let (shutdown, receiver) = tokio::sync::watch::channel(false);
        let (scheduler, task) = crate::gateway::scheduler::test_scheduler(receiver);
        let mut refresh = Refresh::new(&scheduler);
        assert!(
            tokio::time::timeout(Duration::from_secs(3600), refresh.wait())
                .await
                .is_err()
        );
        refresh.update(WorkState::Pending);
        let started = Instant::now();
        refresh.wait().await;
        assert_eq!(Instant::now(), started);
        refresh.wait().await;
        assert_eq!(Instant::now() - started, REFRESH_INTERVAL);
        refresh.update(WorkState::Idle);
        assert!(
            tokio::time::timeout(Duration::from_secs(3600), refresh.wait())
                .await
                .is_err()
        );
        refresh.update(WorkState::Pending);
        let resumed = Instant::now();
        refresh.wait().await;
        assert_eq!(Instant::now(), resumed);
        shutdown.send(true).unwrap();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn ingress_does_not_postpone_pending_refresh() {
        let (shutdown, receiver) = tokio::sync::watch::channel(false);
        let (scheduler, task) = crate::gateway::scheduler::test_scheduler(receiver);
        let mut refresh = Refresh::new(&scheduler);
        refresh.update(WorkState::Pending);
        refresh.wait().await;
        let started = Instant::now();
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            refresh.update(WorkState::Pending);
        }
        refresh.wait().await;
        assert_eq!(Instant::now() - started, REFRESH_INTERVAL);
        shutdown.send(true).unwrap();
        task.await.unwrap();
    }
}
