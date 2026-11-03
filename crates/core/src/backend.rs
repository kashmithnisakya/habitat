//! What the core needs from something that runs agents.
//!
//! Wasmtime is the only backend today. These traits hold nothing specific to it, so that a microVM
//! backend could implement them later. See `docs/architecture.md`.

use std::fmt;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::home::{AgentStorage, Home};
use crate::manifest::{Limits, Manifest, printable};
use crate::package::Package;
use crate::run_log::RunId;

/// Why an agent ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    UserMessage(String),
    Scheduled(String),
}

/// Tells running agents to stop, because the host is shutting down. Clones share the same signal.
#[derive(Debug, Clone, Default)]
pub struct StopSignal(Arc<AtomicBool>);

impl StopSignal {
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Something outside an agent's home that it can be given access to. The text in here comes from
/// the agent, so it's untrusted until the broker has checked it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    NetworkHost(String),
    Model(String),
}

/// How long access lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lifetime {
    /// Covers a single request.
    Once,
    /// Ends when the run does.
    ThisRun,
    /// Lasts until the user removes it.
    UntilRevoked,
}

/// Why an agent wasn't given access. Text in here comes from the host, never from the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessRefused {
    /// The user, or the rules, said no. Asking again won't help.
    Denied(String),
    /// Too many requests, or nobody's there to ask.
    LimitExceeded(String),
    /// The host failed.
    Internal,
}

/// An HTTP method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMethod {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
}

/// A network request from an agent. All of this is untrusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    pub method: FetchMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// A response on its way back to an agent. Reading its body fails, instead of ending early, if it
/// goes over a limit or takes too long.
pub struct FetchResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn Read + Send>,
}

/// Why a network request didn't work. Text in here comes from the host, never from the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// This needs approval first, which the agent can ask for.
    NotGranted,
    Denied(String),
    InvalidRequest(String),
    LimitExceeded(String),
    TimedOut,
    Unavailable(String),
    Internal,
}

/// What one run of an agent can ask of the world outside its home. A backend passes each request
/// here, and never decides anything itself.
pub trait RunBroker {
    /// Asks for access to something. The reason is the agent's own claim, and it's untrusted.
    fn request_access(&mut self, target: &Target, reason: &str) -> Result<Lifetime, AccessRefused>;

    /// Makes a network request.
    fn http_fetch(&mut self, _request: FetchRequest) -> Result<FetchResponse, FetchError> {
        Err(FetchError::Denied("this isn't available".to_string()))
    }
}

/// A broker that refuses everything, for a host that has nothing else.
#[derive(Debug, Default)]
pub struct DenyEverything;

impl RunBroker for DenyEverything {
    fn request_access(&mut self, _: &Target, _: &str) -> Result<Lifetime, AccessRefused> {
        Err(AccessRefused::Denied("this isn't available".to_string()))
    }
}

/// Makes a broker for each run.
pub trait BrokerFactory: Send + Sync {
    fn for_run(
        &self,
        storage: &AgentStorage,
        manifest: &Manifest,
        limits: &RunLimits,
        run_id: RunId,
    ) -> Box<dyn RunBroker + Send>;

    /// Called when a run has ended, however it ended, with a short description safe to store.
    fn run_ended(&self, _storage: &AgentStorage, _run_id: RunId, _how: &str) {}
}

impl BrokerFactory for DenyEverything {
    fn for_run(
        &self,
        _: &AgentStorage,
        _: &Manifest,
        _: &RunLimits,
        _: RunId,
    ) -> Box<dyn RunBroker + Send> {
        Box::new(Self)
    }
}

/// The most this host lets any agent use, whatever its manifest asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostLimits {
    pub memory_mb: u32,
    pub run_seconds: u32,
    pub disk_mb: u32,
}

impl Default for HostLimits {
    /// Modest enough for a phone. A host with more room can raise these.
    fn default() -> Self {
        Self {
            memory_mb: 1024,
            run_seconds: 300,
            disk_mb: 1024,
        }
    }
}

impl HostLimits {
    /// Works out the limits for one agent: what its manifest asks for, up to this host's maximum.
    pub fn limits_for(&self, requested: &Limits) -> RunLimits {
        const MB: u64 = 1024 * 1024;
        RunLimits {
            memory_bytes: u64::from(requested.memory_mb.min(self.memory_mb)) * MB,
            run_time: Duration::from_secs(u64::from(requested.run_seconds.min(self.run_seconds))),
            disk_bytes: u64::from(requested.disk_mb.min(self.disk_mb)) * MB,
        }
    }
}

/// The limits for one agent. Use `disk_bytes` when opening its home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLimits {
    pub memory_bytes: u64,
    pub run_time: Duration,
    pub disk_bytes: u64,
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
    /// Compiling the agent's code would cost the host more than it allows. The reason holds
    /// nothing from the package.
    TooCostly(String),
    /// Another package is still being compiled, and it may be a hostile one. This one hasn't been
    /// checked at all, so try again later.
    Busy,
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
            Self::TooCostly(reason) => {
                write!(f, "the agent's code is too costly to compile: {reason}")
            }
            Self::Busy => write!(f, "another agent's code is still being compiled"),
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
    /// The agent ran out of time, and was stopped.
    TimeLimit,
    /// The agent needed more memory than its limit allows, and crashed because of it.
    MemoryLimit,
    /// The host told the agent to stop, because it's shutting down.
    Stopped,
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
pub trait Backend: Send + Sync {
    /// Checks a package's code and prepares it to run. This runs none of the agent's code.
    fn load(&self, package: &Package) -> Result<Box<dyn LoadedAgent>, LoadError>;
}

/// An agent that's ready to run.
pub trait LoadedAgent {
    /// Runs the agent for one event, starting from nothing but what's in its home. It has to stop
    /// soon after `stop` is set, and report that as [`RunOutcome::Stopped`].
    fn run(
        &self,
        event: &Event,
        home: Home,
        limits: &RunLimits,
        stop: &StopSignal,
        broker: Box<dyn RunBroker + Send>,
    ) -> RunOutcome;
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOSTILE: &str = "\x1b[31mfake\u{202e}\n";
    const MB: u64 = 1024 * 1024;

    fn requested(memory_mb: u32, run_seconds: u32, disk_mb: u32) -> Limits {
        Limits {
            memory_mb,
            run_seconds,
            disk_mb,
            model_budget_tokens: 0,
        }
    }

    #[test]
    fn a_manifest_cant_raise_its_limits_above_the_hosts() {
        let host = HostLimits {
            memory_mb: 32,
            run_seconds: 5,
            disk_mb: 10,
        };
        assert_eq!(
            host.limits_for(&requested(4096, 3600, 10240)),
            RunLimits {
                memory_bytes: 32 * MB,
                run_time: Duration::from_secs(5),
                disk_bytes: 10 * MB,
            }
        );
    }

    #[test]
    fn a_manifest_can_ask_for_less_than_the_hosts_maximum() {
        assert_eq!(
            HostLimits::default().limits_for(&requested(64, 2, 3)),
            RunLimits {
                memory_bytes: 64 * MB,
                run_time: Duration::from_secs(2),
                disk_bytes: 3 * MB,
            }
        );
    }

    #[test]
    fn each_limit_is_worked_out_on_its_own() {
        let host = HostLimits {
            memory_mb: 32,
            run_seconds: 5,
            disk_mb: 10,
        };
        let limits = host.limits_for(&requested(8, 3600, 2));
        assert_eq!(limits.memory_bytes, 8 * MB);
        assert_eq!(limits.run_time, Duration::from_secs(5));
        assert_eq!(limits.disk_bytes, 2 * MB);
    }

    #[test]
    fn the_largest_allowed_values_dont_overflow() {
        let host = HostLimits {
            memory_mb: u32::MAX,
            run_seconds: u32::MAX,
            disk_mb: u32::MAX,
        };
        let limits = host.limits_for(&requested(u32::MAX, u32::MAX, u32::MAX));
        assert_eq!(limits.memory_bytes, u64::from(u32::MAX) * MB);
        assert_eq!(limits.disk_bytes, u64::from(u32::MAX) * MB);
    }

    #[test]
    fn refusals_to_compile_say_why() {
        assert_eq!(
            LoadError::TooCostly("modules: 2, over the limit of 1".to_string()).to_string(),
            "the agent's code is too costly to compile: modules: 2, over the limit of 1"
        );
        assert_eq!(
            LoadError::Busy.to_string(),
            "another agent's code is still being compiled"
        );
    }

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
