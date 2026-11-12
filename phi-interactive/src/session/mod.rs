//! Durable application Session envelope, file policy, discovery, locks and JSONL storage.
//! Application runtime coordinates live associations/checkpoints; SDK owns canonical live history.
//! Saved data remains independently readable without preparing an Agent runtime.

pub(crate) mod discovery;
mod journal;
mod model;
pub mod storage;
pub(crate) mod workflow;

pub(crate) use model::default_switch;
pub use model::*;
