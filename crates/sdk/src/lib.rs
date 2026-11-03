//! Rust library for building Habitat agents.
//!
//! An agent wakes up, handles one event and exits, and only its files carry over to the next run:
//!
//! ```ignore
//! use habitat_sdk::{Agent, BoxError, Event, state};
//!
//! struct Counter;
//!
//! impl Agent for Counter {
//!     fn handle(_event: Event) -> Result<(), BoxError> {
//!         let count: u64 = state::load("count")?.unwrap_or_default();
//!         state::save("count", &(count + 1))?;
//!         Ok(())
//!     }
//! }
//!
//! habitat_sdk::agent!(Counter);
//! ```
//!
//! `docs/writing-an-agent.md` is a tutorial. Agents are built for `wasm32-unknown-unknown`, and
//! calling anything here on another target panics, because there's no host to answer.

use std::fmt;

pub mod files;
pub mod state;

/// The agent API, as generated from `wit/agent-home.wit`. Use this for anything that the rest of
/// this library doesn't cover.
pub mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "agent",
        pub_export_macro: true,
        export_macro_name: "export_agent",
        default_bindings_module: "habitat_sdk::bindings",
    });
}

pub use bindings::habitat::host::types::{DirEntry, EntryKind, Event, HomeError, LogLevel};

use bindings::habitat::host::agent_home as api;

/// What an agent's `handle` can fail with. `?` turns any error into this, and so can `.into()` on
/// a string. Its text is shown as the agent's own claim, not as something the host decided.
pub type BoxError = Box<dyn std::error::Error>;

/// Why something in this library failed.
#[derive(Debug)]
pub enum Error {
    /// The host refused or failed a call.
    Host(HomeError),
    /// The name of a saved value breaks the rules. See [`state`].
    InvalidStateName,
    /// A saved value can't be read back, or can't be saved in the first place.
    InvalidState(String),
    /// A file that was meant to hold text doesn't.
    NotText,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host(error) => write!(f, "{error}"),
            Self::InvalidStateName => write!(
                f,
                "a state name can only use letters, digits, '-' and '_', and at most 64 of them"
            ),
            Self::InvalidState(reason) => write!(f, "invalid saved state: {reason}"),
            Self::NotText => write!(f, "the file doesn't hold UTF-8 text"),
        }
    }
}

impl std::error::Error for Error {}

impl From<HomeError> for Error {
    fn from(error: HomeError) -> Self {
        Self::Host(error)
    }
}

/// An agent. It gets a new start for each event, so anything it needs next time has to be saved in
/// its home first.
pub trait Agent {
    /// Handles one event. Returning an error counts as the run failing, and anything already saved
    /// stays saved.
    fn handle(event: Event) -> Result<(), BoxError>;
}

/// Exports a type as this package's agent. Use this once, for a type that implements [`Agent`].
#[macro_export]
macro_rules! agent {
    ($agent:ty) => {
        const _: () = {
            struct Exported;

            impl $crate::bindings::Guest for Exported {
                fn handle_event(
                    event: $crate::Event,
                ) -> ::core::result::Result<(), ::std::string::String> {
                    <$agent as $crate::Agent>::handle(event).map_err(|error| error.to_string())
                }
            }

            $crate::bindings::export_agent!(Exported);
        };
    };
}

/// The time now, in milliseconds since 1970.
pub fn now_ms() -> u64 {
    api::now_ms()
}

/// Gets `count` random bytes from the host, up to 1024 at a time.
pub fn random_bytes(count: u32) -> Result<Vec<u8>, Error> {
    Ok(api::random_bytes(count)?)
}

/// Adds to the agent's own log, which isn't shown to the user. The host may drop messages.
pub fn log(level: LogLevel, message: &str) {
    api::log(level, message);
}
