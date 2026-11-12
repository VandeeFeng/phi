#[cfg(unix)]
mod api;
#[cfg(unix)]
mod bridge;
#[cfg(unix)]
mod feedback;
#[cfg(unix)]
mod polling;
mod store;

#[cfg(unix)]
use {
    super::PluginContext,
    api::Telegram,
    bridge::Bridge,
    std::{path::Path, sync::Arc},
};

pub(crate) use store::Config;

#[cfg(unix)]
pub(super) async fn run(
    config: Config,
    directory: &Path,
    context: PluginContext,
) -> anyhow::Result<()> {
    let store = Arc::new(store::Store::open(directory)?);
    let working_dir = store.workspace()?;
    let mut shutdown = context.shutdown();
    let gateway = tokio::select! {
        result = context.client_with_store(store.clone()) => result?,
        _ = shutdown.wait_for(|closed| *closed) => return Ok(()),
    };
    let telegram = Telegram::new(config.bot_token.clone())?;
    let state = store.load("state.json")?.unwrap_or_default();
    let mut bridge = Bridge {
        feedback: Default::default(),
        working_dir,
        config,
        store,
        state,
        gateway,
        telegram,
    };
    let scheduler = context.scheduler();
    let result = tokio::select! {
        result = bridge.run(&scheduler) => result,
        _ = shutdown.wait_for(|closed| *closed) => Ok(()),
    };
    for feedback in bridge.feedback.values_mut() {
        feedback.shutdown().await;
    }
    result
}
