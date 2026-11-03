//! What the core needs from something that runs agents.
//!
//! Wasmtime is the only backend today. These traits hold nothing specific to it, so that a microVM
//! backend could implement them later. See `docs/architecture.md`.

use std::fmt;

use crate::home::Home;
use crate::manifest::printable;
use crate::package::Package;

/// Why an agent ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    UserMessage(String),
    Scheduled(String),
}

/// Why an agent can't be loaded. Messages are safe to print, even for a hostile package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// The agent's code is malformed.
    InvalidCode(String),
    /// The agent asks for something outside the agent API, such as WASI sockets.
    ForbiddenImport(String),
    /// The agent was built for an API that this host doesn't provide.
    Incompatible(String),
    /// The backend failed, through no fault of the agent.
    Internal(String),
}

impl LoadError {
    /// Makes an error whose message may hold text from the package.
    pub fn invalid_code(message: &str) -> Self {
        Self::InvalidCode(printable(message))
    }

    /// Makes an error from the name of the forbidden import, which comes from the package.
    pub fn forbidden_import(name: &str) -> Self {
        Self::ForbiddenImport(printable(name))
    }

    /// Makes an error whose message may hold text from the package.
    pub fn incompatible(message: &str) -> Self {
        Self::Incompatible(printable(message))
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCode(message) => write!(f, "invalid agent code: {message}"),
            Self::ForbiddenImport(name) => {
                write!(
                    f,
                    "the agent imports {name}, which isn't part of the agent API"
                )
            }
            Self::Incompatible(message) => {
                write!(f, "the agent doesn't match this host's API: {message}")
            }
            Self::Internal(message) => write!(f, "the backend failed: {message}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// How a run ended. A failure of any kind is only ever reported here, and doesn't affect the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// The agent handled the event.
    Completed,
    /// The agent reported a failure. The message is the agent's own claim, made safe to print.
    Failed(String),
    /// The agent crashed.
    Crashed(String),
    /// The backend failed, through no fault of the agent.
    Internal(String),
}

impl RunOutcome {
    /// Makes the outcome for a failure that the agent reported.
    pub fn failed(message: &str) -> Self {
        Self::Failed(printable(message))
    }
}

/// Something that can run agents.
pub trait Backend {
    /// Checks a package's code and prepares it to run. This runs none of the agent's code.
    fn load(&self, package: &Package) -> Result<Box<dyn LoadedAgent>, LoadError>;
}

/// An agent that's ready to run.
pub trait LoadedAgent {
    /// Runs the agent for one event, starting from nothing but what's in its home.
    fn run(&self, event: &Event, home: Home) -> RunOutcome;
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOSTILE: &str = "\x1b[31mfake\u{202e}\n";

    #[test]
    fn text_from_a_package_or_agent_is_made_safe_to_print() {
        let messages = [
            LoadError::invalid_code(HOSTILE).to_string(),
            LoadError::forbidden_import(HOSTILE).to_string(),
            LoadError::incompatible(HOSTILE).to_string(),
            format!("{:?}", RunOutcome::failed(HOSTILE)),
        ];
        for message in messages {
            assert!(message.is_ascii(), "{message:?}");
            assert!(!message.chars().any(|c| c.is_control()), "{message:?}");
        }
    }
}
