mod capabilities;
mod event;
mod guard;

pub(crate) use capabilities::hyperlinks_supported;

pub(crate) use event::DEFAULT_POLL_TIMEOUT;
pub use event::{KeyInput, poll_key_input, poll_key_input_with_timeout};
pub use guard::TerminalGuard;
