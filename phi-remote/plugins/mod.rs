//! Plugin registration and on-demand access to a Phi Gateway.
//!
//! Pass registrations to [`crate::deployment::start_with_plugins`]. Registration and
//! activation do not create clients. Plugins choose connection timing and retry policy.
//! Device client creation currently requires a local managed deployment on Unix.
//! Hosts continuously poll [`crate::Gateway::wait`] and use [`PluginControl`] to stop
//! or restart background runs. Controls do not remove statically composed routes.
//!
//! ```
//! use phi_remote::plugins::{Plugin, PluginRegistry};
//! use std::path::PathBuf;
//!
//! # #[cfg(unix)]
//! fn register(identity_directory: PathBuf) -> anyhow::Result<PluginRegistry> {
//!     let mut plugins = PluginRegistry::new();
//!     plugins.register("example:chat", Plugin::new().with_task(move |context| {
//!         let identity_directory = identity_directory.clone();
//!         async move {
//!             let mut shutdown = context.shutdown();
//!             let client = tokio::select! {
//!                 result = context.client(&identity_directory) => result?,
//!                 _ = shutdown.wait_for(|stopping| *stopping) => return Ok(()),
//!             };
//!             let _sessions = client.sessions().await?;
//!             let _ = shutdown.wait_for(|stopping| *stopping).await;
//!             Ok(())
//!         }
//!     }))?;
//!     Ok(plugins)
//! }
//! ```

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::ensure;
use axum::Router;
use tokio::sync::watch;

#[cfg(unix)]
mod activity;
#[cfg(unix)]
mod backoff;
mod builtin;
#[cfg(unix)]
mod client;
mod feishu;
mod lifecycle;
#[cfg(unix)]
mod store;
mod telegram;
mod web;

pub(crate) use builtin::{Configuration, Selection};
#[cfg(unix)]
pub use client::{PluginClient, PluginWatch};
pub(crate) use feishu::Config as FeishuConfig;
pub use lifecycle::PluginControl;
pub(crate) use lifecycle::PluginTasks;
pub(crate) use telegram::Config as TelegramConfig;

const MAX_PLUGIN_NAME_BYTES: usize = 128;

#[derive(Debug)]
struct CleanupFailure;

impl std::fmt::Display for CleanupFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("remote plugin cleanup failed")
    }
}

impl std::error::Error for CleanupFailure {}

type PluginFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;
type PluginTask = std::sync::Arc<dyn Fn(PluginContext) -> PluginFuture + Send + Sync>;

/// A plugin may provide HTTP routes, a background task, or both.
#[derive(Default)]
pub struct Plugin {
    routes: Router,
    task: Option<PluginTask>,
}

impl Plugin {
    pub fn new() -> Self {
        Self::default()
    }

    /// Routes inherit Gateway ingress checks; non-API routes need plugin-owned authentication.
    pub fn with_routes(mut self, routes: Router) -> Self {
        self.routes = routes;
        self
    }

    /// Registers a repeatable factory; every run must release its resources before returning.
    /// The plugin owns connection timing, retries, and shutdown cleanup.
    /// Errors during stopping block restart; panics also retain a cleanup failure.
    pub fn with_task<F, Work>(mut self, task: F) -> Self
    where
        F: Fn(PluginContext) -> Work + Send + Sync + 'static,
        Work: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        self.task = Some(std::sync::Arc::new(move |context| Box::pin(task(context))));
        self
    }
}

/// Registration does not execute plugins, connect, or create storage.
#[derive(Default)]
pub struct PluginRegistry {
    entries: Vec<(String, Plugin)>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Names use `namespace:name`; duplicate names are rejected without replacement.
    pub fn register(&mut self, name: &str, plugin: Plugin) -> anyhow::Result<()> {
        ensure!(valid_name(name), "invalid remote plugin name");
        ensure!(
            !self.entries.iter().any(|(existing, _)| existing == name),
            "remote plugin already registered: {name}"
        );
        self.entries.push((name.to_owned(), plugin));
        Ok(())
    }

    pub(crate) fn merge(mut self, other: Self) -> anyhow::Result<Self> {
        for (name, _) in &other.entries {
            ensure!(
                !self.entries.iter().any(|(existing, _)| existing == name),
                "remote plugin already registered: {name}"
            );
        }
        self.entries.extend(other.entries);
        Ok(self)
    }

    pub(crate) fn take_routes(&mut self) -> Router {
        self.entries
            .iter_mut()
            .fold(Router::new(), |router, (_, plugin)| {
                router.merge(std::mem::take(&mut plugin.routes))
            })
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PLUGIN_NAME_BYTES
        && name.split(':').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        })
        && name.split(':').count() == 2
}

/// Gateway access and lifecycle context, not plugin business state.
#[derive(Clone)]
pub struct PluginContext {
    gateway_directory: PathBuf,
    endpoint: String,
    shutdown: watch::Receiver<bool>,
    scheduler: crate::GatewayScheduler,
}

impl PluginContext {
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Signals this run's stop, restart, or Gateway shutdown, not sibling plugin stops.
    pub fn shutdown(&self) -> watch::Receiver<bool> {
        self.shutdown.clone()
    }

    /// Retained handles become permanently closed when this run stops.
    pub fn scheduler(&self) -> crate::GatewayScheduler {
        self.scheduler.clone()
    }

    /// Creates a device-authenticated client only when the plugin requests it.
    /// The identity directory must be private; the client retains its exclusive lock.
    #[cfg(unix)]
    pub async fn client(&self, identity_directory: &Path) -> anyhow::Result<PluginClient> {
        self.client_with_store(std::sync::Arc::new(store::Store::open(identity_directory)?))
            .await
    }

    #[cfg(unix)]
    async fn client_with_store(
        &self,
        store: std::sync::Arc<store::Store>,
    ) -> anyhow::Result<PluginClient> {
        PluginClient::connect(&self.gateway_directory, &self.endpoint, store).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn names_are_unique_and_invalid_names_never_register(
            namespace in "[a-z][a-z0-9_-]{0,20}",
            local_name in "[a-z][a-z0-9_-]{0,30}",
        ) {
            let name = format!("{namespace}:{local_name}");
            let mut registry = PluginRegistry::new();
            registry.register(&name, Plugin::new()).unwrap();
            prop_assert!(registry.register(&name, Plugin::new()).is_err());
            for invalid in [
                format!("{namespace}/{local_name}"),
                format!(":{local_name}"),
                format!("{namespace}:"),
                format!("{name}:extra"),
                format!("{name}\n"),
                format!("{}:{local_name}", "x".repeat(MAX_PLUGIN_NAME_BYTES)),
            ] {
                prop_assert!(registry.register(&invalid, Plugin::new()).is_err());
            }
            prop_assert_eq!(registry.entries.len(), 1);
        }
    }
}
