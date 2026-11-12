use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use super::client::McpClientHandle;
use super::config::{McpLifecycle, McpServerConfig};
use super::metadata::ConnectionGeneration;

pub(super) struct McpConnection {
    pub(super) generation: ConnectionGeneration,
    pub(super) handle: McpClientHandle,
    pub(super) _tasks: McpConnectionTasks,
    pub(super) last_used: Instant,
    pub(super) in_flight: Arc<AtomicUsize>,
}

pub(super) struct McpConnectionLease {
    pub(super) generation: ConnectionGeneration,
    pub(super) peer: rmcp::service::Peer<rmcp::service::RoleClient>,
    _in_flight: McpInFlightGuard,
}

struct McpInFlightGuard(Arc<AtomicUsize>);

impl McpInFlightGuard {
    fn new(in_flight: Arc<AtomicUsize>) -> Self {
        in_flight.fetch_add(1, Ordering::AcqRel);
        Self(in_flight)
    }
}

impl Drop for McpInFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Default)]
pub(super) struct McpConnectionTasks(pub(super) Vec<JoinHandle<()>>);

impl Drop for McpConnectionTasks {
    fn drop(&mut self) {
        for task in self.0.drain(..) {
            task.abort();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct McpConnectAttempt {
    pub(super) generation: ConnectionGeneration,
}

pub(super) enum McpConnectStart {
    Connected,
    Attempt(McpConnectAttempt),
}

pub(super) struct RemovedConnection {
    pub(super) server: String,
    pub(super) generation: ConnectionGeneration,
    pub(super) reason: String,
}

#[derive(Default)]
struct McpConnectionPoolState {
    connections: HashMap<String, McpConnection>,
    attempts: HashMap<String, ConnectionGeneration>,
    cancelled: HashSet<String>,
    shutdown: bool,
}

impl McpConnectionPoolState {
    fn take_commit_slot(&mut self, server: &str, attempt: McpConnectAttempt) -> anyhow::Result<()> {
        let current = self.attempts.get(server).copied();
        if self.shutdown || self.cancelled.contains(server) || current != Some(attempt.generation) {
            self.attempts.remove(server);
            anyhow::bail!("Connection to MCP server '{server}' cancelled before commit");
        }
        self.attempts.remove(server);
        self.cancelled.remove(server);
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct McpConnectionPool {
    state: Mutex<McpConnectionPoolState>,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    next_generation: AtomicU64,
}

impl McpConnectionPool {
    pub(super) fn new() -> Self {
        Self {
            next_generation: AtomicU64::new(1),
            ..Self::default()
        }
    }

    pub(super) async fn connect_lock(&self, server: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        locks
            .entry(server.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub(super) async fn begin_connect(&self, server: &str) -> anyhow::Result<McpConnectStart> {
        let mut state = self.state.lock().await;
        if state.shutdown {
            anyhow::bail!("MCP runtime is shut down");
        }
        if state.connections.contains_key(server) {
            return Ok(McpConnectStart::Connected);
        }
        if state.cancelled.remove(server) {
            anyhow::bail!("Connection to MCP server '{server}' cancelled");
        }
        let generation = ConnectionGeneration(self.next_generation.fetch_add(1, Ordering::Relaxed));
        state.attempts.insert(server.to_string(), generation);
        Ok(McpConnectStart::Attempt(McpConnectAttempt { generation }))
    }

    pub(super) async fn commit(
        &self,
        server: &str,
        attempt: McpConnectAttempt,
        connection: McpConnection,
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock().await;
        state.take_commit_slot(server, attempt)?;
        state.connections.insert(server.to_string(), connection);
        Ok(())
    }

    pub(super) async fn fail(&self, server: &str, attempt: McpConnectAttempt) -> bool {
        let mut state = self.state.lock().await;
        let current = state.attempts.get(server) == Some(&attempt.generation);
        if current {
            state.attempts.remove(server);
        }
        current && !state.cancelled.contains(server) && !state.shutdown
    }

    pub(super) async fn cancel_connect(&self, server: &str) {
        let mut state = self.state.lock().await;
        state.cancelled.insert(server.to_string());
        state.attempts.remove(server);
    }

    pub(super) async fn clear_cancelled(&self, server: &str) {
        self.state.lock().await.cancelled.remove(server);
    }

    pub(super) async fn checkout(&self, server: &str) -> Option<McpConnectionLease> {
        let state = self.state.lock().await;
        let connection = state.connections.get(server)?;
        let guard = McpInFlightGuard::new(connection.in_flight.clone());
        Some(McpConnectionLease {
            generation: connection.generation,
            peer: connection.handle.peer(),
            _in_flight: guard,
        })
    }

    pub(super) async fn peer(
        &self,
        server: &str,
    ) -> Option<(
        ConnectionGeneration,
        rmcp::service::Peer<rmcp::service::RoleClient>,
    )> {
        let state = self.state.lock().await;
        state
            .connections
            .get(server)
            .map(|connection| (connection.generation, connection.handle.peer()))
    }

    pub(super) async fn generation(&self, server: &str) -> Option<ConnectionGeneration> {
        self.state
            .lock()
            .await
            .connections
            .get(server)
            .map(|connection| connection.generation)
    }

    pub(super) async fn generations(&self) -> HashMap<String, ConnectionGeneration> {
        self.state
            .lock()
            .await
            .connections
            .iter()
            .map(|(server, connection)| (server.clone(), connection.generation))
            .collect()
    }

    pub(super) async fn connecting(&self) -> HashSet<String> {
        self.state.lock().await.attempts.keys().cloned().collect()
    }

    pub(super) async fn remove(&self, server: &str, reason: &str) -> Option<RemovedConnection> {
        let connection = self.state.lock().await.connections.remove(server)?;
        Some(RemovedConnection {
            server: server.to_string(),
            generation: connection.generation,
            reason: reason.to_string(),
        })
    }

    pub(super) async fn remove_generation(
        &self,
        server: &str,
        generation: ConnectionGeneration,
        reason: &str,
    ) -> Option<RemovedConnection> {
        let mut state = self.state.lock().await;
        if !state
            .connections
            .get(server)
            .is_some_and(|connection| connection.generation == generation)
        {
            return None;
        }
        let connection = state.connections.remove(server)?;
        Some(RemovedConnection {
            server: server.to_string(),
            generation: connection.generation,
            reason: reason.to_string(),
        })
    }

    pub(super) async fn mark_used(&self, server: &str, generation: Option<ConnectionGeneration>) {
        let mut state = self.state.lock().await;
        let Some(connection) = state.connections.get_mut(server) else {
            return;
        };
        if generation.is_none_or(|generation| connection.generation == generation) {
            connection.last_used = Instant::now();
        }
    }

    pub(super) async fn remove_idle(
        &self,
        configs: &HashMap<String, McpServerConfig>,
    ) -> Vec<RemovedConnection> {
        let mut removed = Vec::new();
        let mut state = self.state.lock().await;
        state.connections.retain(|server, connection| {
            let reason = match configs.get(server) {
                None => Some("removed_config"),
                Some(config)
                    if config.effective_runtime().lifecycle != McpLifecycle::KeepAlive
                        && connection.in_flight.load(Ordering::Acquire) == 0
                        && config.effective_runtime().idle_timeout_secs > 0
                        && connection.last_used.elapsed()
                            >= config.effective_runtime().idle_timeout() =>
                {
                    Some("idle_timeout")
                }
                _ => None,
            };
            if let Some(reason) = reason {
                removed.push(RemovedConnection {
                    server: server.clone(),
                    generation: connection.generation,
                    reason: reason.to_string(),
                });
                false
            } else {
                true
            }
        });
        removed
    }

    pub(super) async fn shutdown(&self) -> Vec<RemovedConnection> {
        let mut state = self.state.lock().await;
        state.shutdown = true;
        state.attempts.clear();
        state
            .connections
            .drain()
            .map(|(server, connection)| RemovedConnection {
                server,
                generation: connection.generation,
                reason: "shutdown".to_string(),
            })
            .collect()
    }

    pub(super) async fn is_shutdown(&self) -> bool {
        self.state.lock().await.shutdown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_and_shutdown_attempts_cannot_commit() {
        let pool = McpConnectionPool::new();
        let McpConnectStart::Attempt(cancelled) = pool.begin_connect("cancelled").await.unwrap()
        else {
            panic!("expected connection attempt");
        };
        pool.cancel_connect("cancelled").await;
        assert!(
            pool.state
                .lock()
                .await
                .take_commit_slot("cancelled", cancelled)
                .is_err()
        );

        pool.clear_cancelled("cancelled").await;
        let McpConnectStart::Attempt(shutdown) = pool.begin_connect("shutdown").await.unwrap()
        else {
            panic!("expected connection attempt");
        };
        pool.shutdown().await;
        assert!(
            pool.state
                .lock()
                .await
                .take_commit_slot("shutdown", shutdown)
                .is_err()
        );
        assert!(pool.begin_connect("new").await.is_err());

        let task = tokio::spawn(std::future::pending::<()>());
        let abort = task.abort_handle();
        drop(McpConnectionTasks(vec![task]));
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
    }
}
