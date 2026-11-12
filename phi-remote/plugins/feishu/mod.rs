#[cfg(unix)]
mod api;
#[cfg(unix)]
mod bridge;
#[cfg(unix)]
mod presentation;
mod store;
#[cfg(unix)]
mod transport;

#[cfg(unix)]
use {
    super::PluginContext,
    anyhow::Context,
    api::Feishu,
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
    let feishu = Feishu::new(config.app_id.clone(), config.app_secret.clone())?;
    let bot = tokio::select! {
        result = feishu.identity() => result?,
        _ = shutdown.wait_for(|closed| *closed) => return Ok(()),
    };
    let state = store.load("state.json")?.unwrap_or_default();
    let mut bridge = Bridge {
        working_dir,
        config,
        store,
        state,
        gateway: gateway.clone(),
        feishu: feishu.clone(),
        bot,
        presenter: Some(presentation::Presenter::new(feishu.clone(), gateway)),
    };
    bridge.prepare()?;
    let (incoming, receiver) = tokio::sync::mpsc::channel(transport::CHANNEL_CAPACITY);
    let (acknowledgements, responses) = tokio::sync::mpsc::channel(transport::CHANNEL_CAPACITY);
    let mut connection = tokio::task::JoinSet::new();
    connection.spawn(transport::run(feishu, incoming, responses));
    let scheduler = context.scheduler();
    let result = tokio::select! {
        result = bridge.run(receiver, acknowledgements, &scheduler) => result,
        result = connection.join_next() => result.context("Feishu connection task missing")
            .and_then(|result| result.context("Feishu connection task failed"))
            .and_then(|result| result),
        _ = shutdown.wait_for(|closed| *closed) => Ok(()),
    };
    connection.abort_all();
    while connection.join_next().await.is_some() {}
    if let Some(presenter) = &mut bridge.presenter {
        presenter.shutdown().await.context(super::CleanupFailure)?;
    }
    result
}
