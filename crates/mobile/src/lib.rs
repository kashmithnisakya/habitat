//! What the phone app calls in the Rust core, through UniFFI. It makes no decisions of its own.

use std::fmt;
use std::fs::File;
use std::sync::Arc;

use habitat_backend_wasm::WasmBackend;
use habitat_core::backend::{Event, HostLimits, RunLimits, RunOutcome};
use habitat_core::home::HomeError;
use habitat_core::package::PackageError;
use habitat_core::supervisor::{self, InstalledAgent, Supervisor, SupervisorError};

uniffi::setup_scaffolding!();

const MB: u64 = 1024 * 1024;

/// Why a call failed. The text never holds host paths, so it's safe to show to the user.
#[derive(Debug, PartialEq, Eq, uniffi::Error)]
pub enum CoreError {
    /// The agent is already running.
    Busy,
    /// No such agent is installed.
    NotFound,
    /// The package or request isn't acceptable, and trying again won't help.
    Rejected { reason: String },
    /// Something went wrong on this device.
    Failed { reason: String },
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => write!(f, "the agent is already running"),
            Self::NotFound => write!(f, "that agent isn't installed"),
            Self::Rejected { reason } | Self::Failed { reason } => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for CoreError {}

impl From<SupervisorError> for CoreError {
    fn from(error: SupervisorError) -> Self {
        let reason = error.to_string();
        match error {
            SupervisorError::Busy => Self::Busy,
            SupervisorError::Home(HomeError::NotFound)
            | SupervisorError::Package(PackageError::NotInstalled) => Self::NotFound,
            SupervisorError::Home(HomeError::InvalidPath(_)) => Self::Rejected { reason },
            SupervisorError::Package(PackageError::Io(_))
            | SupervisorError::Load(habitat_core::backend::LoadError::Internal(_))
            | SupervisorError::Home(_)
            | SupervisorError::Io(_) => Self::Failed { reason },
            SupervisorError::Package(_) | SupervisorError::Load(_) => Self::Rejected { reason },
        }
    }
}

/// An installed agent. If `problem` is set, it's damaged and can only be removed.
#[derive(Debug, PartialEq, Eq, uniffi::Record)]
pub struct AgentInfo {
    pub name: String,
    pub version: Option<String>,
    pub memory_mb: Option<u32>,
    pub run_seconds: Option<u32>,
    pub disk_mb: Option<u32>,
    pub problem: Option<String>,
}

/// How a run ended, or how a past run stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum RunState {
    Completed,
    Failed,
    TimeLimit,
    MemoryLimit,
    Crashed,
    Internal,
    Interrupted,
    Running,
}

/// The result of sending an agent a message.
#[derive(Debug, PartialEq, Eq, uniffi::Record)]
pub struct RunReport {
    pub run_id: String,
    pub state: RunState,
    /// The reason for a failure. If the state is `Failed`, this is the agent's own claim.
    pub detail: Option<String>,
    /// What the agent said, which is its own claim and has to be shown as plain text.
    pub messages: Vec<String>,
}

/// One past run.
#[derive(Debug, PartialEq, Eq, uniffi::Record)]
pub struct RunSummary {
    pub run_id: String,
    pub state: RunState,
    pub detail: Option<String>,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
}

#[derive(uniffi::Object)]
pub struct HabitatCore {
    supervisor: Supervisor<WasmBackend>,
}

#[uniffi::export]
impl HabitatCore {
    /// Opens the core, with its data in `data_dir`, which has to be private to the app.
    #[uniffi::constructor]
    pub fn open(data_dir: String) -> Result<Arc<Self>, CoreError> {
        let backend = WasmBackend::new().map_err(SupervisorError::Load)?;
        Ok(Arc::new(Self {
            supervisor: Supervisor::new(data_dir, backend, HostLimits::default()),
        }))
    }

    /// Checks the package file at `path`, which is untrusted, and installs it.
    pub fn install_package(&self, path: String) -> Result<AgentInfo, CoreError> {
        let file = File::open(path).map_err(|error| CoreError::Failed {
            reason: format!("couldn't read the package: {}", error.kind()),
        })?;
        let info = self.supervisor.install(file)?;
        Ok(ready_agent(info))
    }

    pub fn list_agents(&self) -> Result<Vec<AgentInfo>, CoreError> {
        let agents = self.supervisor.agents()?;
        Ok(agents
            .into_iter()
            .map(|agent| match agent {
                InstalledAgent::Ready(info) => ready_agent(info),
                InstalledAgent::Damaged { name, reason } => AgentInfo {
                    name,
                    version: None,
                    memory_mb: None,
                    run_seconds: None,
                    disk_mb: None,
                    problem: Some(reason),
                },
            })
            .collect())
    }

    pub fn remove_agent(&self, name: String) -> Result<(), CoreError> {
        Ok(self.supervisor.remove(&name)?)
    }

    /// Runs the agent for one message. This blocks until it ends, so don't call it from a UI thread.
    pub fn send_message(&self, agent: String, text: String) -> Result<RunReport, CoreError> {
        let report = self.supervisor.run(&agent, &Event::UserMessage(text))?;
        let (state, detail) = from_outcome(report.output.outcome);
        Ok(RunReport {
            run_id: report.run_id,
            state,
            detail,
            messages: report.output.messages,
        })
    }

    /// The agent's most recent runs, newest first.
    pub fn recent_runs(&self, agent: String, limit: u32) -> Result<Vec<RunSummary>, CoreError> {
        let runs = self.supervisor.recent_runs(&agent, limit as usize)?;
        Ok(runs
            .into_iter()
            .map(|run| {
                let (state, detail) = match run.status {
                    supervisor::RunStatus::Running => (RunState::Running, None),
                    supervisor::RunStatus::Interrupted => (RunState::Interrupted, None),
                    supervisor::RunStatus::Finished(outcome) => from_outcome(outcome),
                };
                RunSummary {
                    run_id: run.run_id,
                    state,
                    detail,
                    started_ms: run.started_ms,
                    finished_ms: run.finished_ms,
                }
            })
            .collect())
    }
}

fn ready_agent(info: supervisor::AgentInfo) -> AgentInfo {
    let RunLimits {
        memory_bytes,
        run_time,
        disk_bytes,
    } = info.limits;
    AgentInfo {
        name: info.name,
        version: Some(info.version),
        memory_mb: Some((memory_bytes / MB) as u32),
        run_seconds: Some(run_time.as_secs() as u32),
        disk_mb: Some((disk_bytes / MB) as u32),
        problem: None,
    }
}

fn from_outcome(outcome: RunOutcome) -> (RunState, Option<String>) {
    match outcome {
        RunOutcome::Completed => (RunState::Completed, None),
        RunOutcome::Failed(message) => (RunState::Failed, Some(message)),
        RunOutcome::TimeLimit => (RunState::TimeLimit, None),
        RunOutcome::MemoryLimit => (RunState::MemoryLimit, None),
        RunOutcome::Crashed(reason) => (RunState::Crashed, Some(reason)),
        RunOutcome::Internal(reason) => (RunState::Internal, Some(reason)),
    }
}
