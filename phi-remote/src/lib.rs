//! Authenticated local gateway and browser client for the public Phi Agent API.

mod client;
pub mod deployment;
mod gateway;
mod network;
mod observation;
pub mod pairing;
#[path = "../plugins/mod.rs"]
pub mod plugins;
pub mod protocol;
mod snapshot;
mod wire;

pub use client::{RemoteClient, RemoteWatch};
pub use gateway::scheduler::{GatewayScheduler, ScheduledWake};
pub use network::GatewayConfig;
pub use observation::{RemoteEvent, RemoteRefresh};
pub use wire::{RemoteCatalog, RemoteModel, RemoteSelection};

pub use gateway::{
    Gateway, GatewayAccess, RemoteHost, RemoteInteraction, RemoteRuntimeUpdate, RemoteSession,
    RemoteSnapshot, RemoteSubmission, SavedSession,
};

fn session_segment(id: &str) -> anyhow::Result<&str> {
    anyhow::ensure!(
        !id.is_empty()
            && id.len() <= 256
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid Session identifier"
    );
    Ok(id)
}
