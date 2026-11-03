//! Installs agents, and starts, limits and records each run. See `docs/architecture.md`.

use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::backend::{Backend, Event, HostLimits, LoadError, RunLimits, RunOutcome, RunOutput};
use crate::home::{AgentStorage, HomeError};
use crate::manifest::{is_agent_name, printable};
use crate::package::{Package, PackageError};

const MAX_RUNS_LOG_BYTES: u64 = 256 * 1024;

const STARTED: &str = "started";
const COMPLETED: &str = "completed";
const FAILED: &str = "failed";
const TIME_LIMIT: &str = "time-limit";
const MEMORY_LIMIT: &str = "memory-limit";
const CRASHED: &str = "crashed";
const INTERNAL: &str = "internal";

/// Why the supervisor refused or couldn't do something. Messages hold no host paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorError {
    Home(HomeError),
    Package(PackageError),
    Load(LoadError),
    /// The agent is already running, and only one run at a time is allowed.
    Busy,
    Io(io::ErrorKind),
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Home(error) => error.fmt(f),
            Self::Package(error) => error.fmt(f),
            Self::Load(error) => error.fmt(f),
            Self::Busy => write!(f, "the agent is already running"),
            Self::Io(kind) => write!(f, "file system error: {kind}"),
        }
    }
}

impl std::error::Error for SupervisorError {}

impl From<HomeError> for SupervisorError {
    fn from(error: HomeError) -> Self {
        Self::Home(error)
    }
}

impl From<PackageError> for SupervisorError {
    fn from(error: PackageError) -> Self {
        Self::Package(error)
    }
}

impl From<LoadError> for SupervisorError {
    fn from(error: LoadError) -> Self {
        Self::Load(error)
    }
}

impl From<io::Error> for SupervisorError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

/// An installed agent that passed its checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    pub name: String,
    pub version: String,
    /// What it actually gets: its manifest's limits, up to this host's maximum.
    pub limits: RunLimits,
}

/// An entry in the list of installed agents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstalledAgent {
    Ready(AgentInfo),
    /// Its package changed or can't be read, so it won't run, but the user can still remove it.
    Damaged {
        name: String,
        reason: String,
    },
}

/// The result of one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// Unique to this run, so that it can later be linked to audit log entries.
    pub run_id: String,
    pub output: RunOutput,
}

/// How a past run stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    /// It never finished, because the host stopped or was killed during it.
    Interrupted,
    Finished(RunOutcome),
}

/// One past run. This never holds what the user or the agent said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub run_id: String,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
    pub status: RunStatus,
}

/// Held for as long as an agent is in use. The system releases it when the process dies, however that happens.
struct RunLock {
    _file: File,
}

impl RunLock {
    fn acquire(storage: &AgentStorage) -> Result<Self, SupervisorError> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(storage.run_lock_path())?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(SupervisorError::Busy),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

/// Runs agents from the folder at `root`, one run at a time for each agent.
pub struct Supervisor<B> {
    root: PathBuf,
    backend: B,
    host_limits: HostLimits,
}

impl<B: Backend> Supervisor<B> {
    pub fn new(root: impl Into<PathBuf>, backend: B, host_limits: HostLimits) -> Self {
        Self {
            root: root.into(),
            backend,
            host_limits,
        }
    }

    /// Checks a package completely, and installs it only if all of it can be used.
    pub fn install(&self, package_file: impl Read) -> Result<AgentInfo, SupervisorError> {
        let package = Package::read(package_file)?;
        self.backend.load(&package)?;
        let storage = AgentStorage::open(&self.root, package.manifest().name())?;
        let _lock = RunLock::acquire(&storage)?;
        package.install(&storage)?;
        Ok(self.info(&package))
    }

    /// Lists installed agents by name. A folder from an install that never finished isn't listed.
    pub fn agents(&self) -> Result<Vec<InstalledAgent>, SupervisorError> {
        let entries = match fs::read_dir(AgentStorage::agents_dir(&self.root)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            other => other?,
        };
        let mut agents = Vec::new();
        for entry in entries {
            let name = entry?.file_name();
            let Some(name) = name.to_str().filter(|name| is_agent_name(name)) else {
                continue;
            };
            let storage = AgentStorage::open_existing(&self.root, name)?;
            match Package::load_installed(&storage) {
                Ok(package) => agents.push(InstalledAgent::Ready(self.info(&package))),
                Err(PackageError::NotInstalled) => {}
                Err(error) => agents.push(InstalledAgent::Damaged {
                    name: name.to_string(),
                    reason: error.to_string(),
                }),
            }
        }
        agents.sort_by(|a, b| agent_name(a).cmp(agent_name(b)));
        Ok(agents)
    }

    /// Deletes an agent and all of its files, unless it's running.
    pub fn remove(&self, name: &str) -> Result<(), SupervisorError> {
        let storage = AgentStorage::open_existing(&self.root, name)?;
        let _lock = RunLock::acquire(&storage)?;
        fs::remove_dir_all(storage.folder())?;
        Ok(())
    }

    /// Runs an agent for one event. A failed run is a normal result here, and only ends that run.
    pub fn run(&self, name: &str, event: &Event) -> Result<RunReport, SupervisorError> {
        let storage = AgentStorage::open_existing(&self.root, name)?;
        let _lock = RunLock::acquire(&storage)?;
        let package = Package::load_installed(&storage)?;
        let limits = self.host_limits.limits_for(&package.manifest().limits());
        let run_id = new_run_id()?;

        // Nothing runs unless it's on record, so a run that dies can be spotted afterwards.
        append_record(&storage, &run_id, STARTED, "")?;
        let output = self.run_package(&storage, &package, event, &limits);
        let (kind, detail) = record_of(&output.outcome);
        // The run has happened, so its result still matters if this fails. It then shows as interrupted.
        let _ = append_record(&storage, &run_id, kind, detail);
        Ok(RunReport { run_id, output })
    }

    /// The agent's most recent runs, newest first.
    pub fn recent_runs(&self, name: &str, limit: usize) -> Result<Vec<RunRecord>, SupervisorError> {
        let storage = AgentStorage::open_existing(&self.root, name)?;
        let running = match RunLock::acquire(&storage) {
            Ok(_) => false,
            Err(SupervisorError::Busy) => true,
            Err(error) => return Err(error),
        };

        let mut runs: Vec<RunRecord> = Vec::new();
        for path in [old_runs_path(&storage), storage.runs_path()] {
            let text = match fs::read_to_string(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                other => other?,
            };
            for line in text.lines() {
                apply_record(&mut runs, line);
            }
        }
        if running && let Some(last) = runs.last_mut().filter(|run| run.finished_ms.is_none()) {
            last.status = RunStatus::Running;
        }
        runs.reverse();
        runs.truncate(limit);
        Ok(runs)
    }

    fn run_package(
        &self,
        storage: &AgentStorage,
        package: &Package,
        event: &Event,
        limits: &RunLimits,
    ) -> RunOutput {
        let prepared = self
            .backend
            .load(package)
            .map_err(|error| error.to_string())
            .and_then(|agent| {
                let home = storage.home(limits.disk_bytes);
                home.map(|home| (agent, home))
                    .map_err(|error| error.to_string())
            });
        match prepared {
            Ok((agent, home)) => agent.run(event, home, limits),
            Err(reason) => RunOutput {
                outcome: RunOutcome::Internal(reason),
                messages: Vec::new(),
            },
        }
    }

    fn info(&self, package: &Package) -> AgentInfo {
        let manifest = package.manifest();
        AgentInfo {
            name: manifest.name().to_string(),
            version: manifest.version().to_string(),
            limits: self.host_limits.limits_for(&manifest.limits()),
        }
    }
}

fn agent_name(agent: &InstalledAgent) -> &str {
    match agent {
        InstalledAgent::Ready(info) => &info.name,
        InstalledAgent::Damaged { name, .. } => name,
    }
}

fn new_run_id() -> Result<String, SupervisorError> {
    let mut bytes = [0; 8];
    getrandom::fill(&mut bytes).map_err(|_| SupervisorError::Io(io::ErrorKind::Other))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn old_runs_path(storage: &AgentStorage) -> PathBuf {
    storage.runs_path().with_extension("log.old")
}

fn record_of(outcome: &RunOutcome) -> (&'static str, &str) {
    match outcome {
        RunOutcome::Completed => (COMPLETED, ""),
        RunOutcome::Failed(message) => (FAILED, message),
        RunOutcome::TimeLimit => (TIME_LIMIT, ""),
        RunOutcome::MemoryLimit => (MEMORY_LIMIT, ""),
        RunOutcome::Crashed(reason) => (CRASHED, reason),
        RunOutcome::Internal(reason) => (INTERNAL, reason),
    }
}

fn append_record(
    storage: &AgentStorage,
    run_id: &str,
    kind: &str,
    detail: &str,
) -> Result<(), SupervisorError> {
    let path = storage.runs_path();
    if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_RUNS_LOG_BYTES) {
        fs::rename(&path, old_runs_path(storage))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    // Printable text can't hold a tab or a line break, so a detail can't forge another record.
    let line = format!("{run_id}\t{}\t{kind}\t{}\n", now_ms(), printable(detail));
    file.write_all(line.as_bytes())?;
    Ok(())
}

// Skips any line it can't read, because a crash can leave the last one cut short.
fn apply_record(runs: &mut Vec<RunRecord>, line: &str) {
    let mut fields = line.split('\t');
    let (Some(run_id), Some(time), Some(kind), Some(detail), None) = (
        fields.next(),
        fields.next().and_then(|time| time.parse().ok()),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return;
    };
    let outcome = match kind {
        STARTED => {
            runs.push(RunRecord {
                run_id: run_id.to_string(),
                started_ms: time,
                finished_ms: None,
                status: RunStatus::Interrupted,
            });
            return;
        }
        COMPLETED => RunOutcome::Completed,
        FAILED => RunOutcome::Failed(detail.to_string()),
        TIME_LIMIT => RunOutcome::TimeLimit,
        MEMORY_LIMIT => RunOutcome::MemoryLimit,
        CRASHED => RunOutcome::Crashed(detail.to_string()),
        INTERNAL => RunOutcome::Internal(detail.to_string()),
        _ => return,
    };
    if let Some(run) = runs.iter_mut().rev().find(|run| run.run_id == run_id) {
        run.finished_ms = Some(time);
        run.status = RunStatus::Finished(outcome);
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::backend::LoadedAgent;
    use crate::home::Home;

    const CRASH_DIR_VAR: &str = "HABITAT_TEST_SUPERVISOR_CRASH_DIR";
    const COMPONENT: &[u8] = b"\0asm\x0d\0\x01\0";
    const LIMITS: &str = "[limits]\nmemory_mb = 64\nrun_seconds = 10\ndisk_mb = 1\n";

    fn package_file(name: &str) -> Vec<u8> {
        let manifest = format!("[agent]\nname = \"{name}\"\nversion = \"1.2.3\"\n\n{LIMITS}");
        let mut file = Vec::new();
        Package::new(&manifest, COMPONENT.to_vec())
            .unwrap()
            .write(&mut file)
            .unwrap();
        file
    }

    type BlockedRun = (Sender<()>, Receiver<()>);

    // Does what each message says, so that tests can make a run end in every way.
    #[derive(Clone, Default)]
    struct FakeBackend {
        refuse: bool,
        block: Arc<Mutex<Option<BlockedRun>>>,
    }

    struct FakeAgent(FakeBackend);

    impl Backend for FakeBackend {
        fn load(&self, _package: &Package) -> Result<Box<dyn LoadedAgent>, LoadError> {
            if self.refuse {
                return Err(LoadError::invalid_code("refused"));
            }
            Ok(Box::new(FakeAgent(self.clone())))
        }
    }

    impl LoadedAgent for FakeAgent {
        fn run(&self, event: &Event, mut home: Home, _limits: &RunLimits) -> RunOutput {
            let Event::UserMessage(text) = event else {
                panic!("the tests only send messages");
            };
            let outcome = match text.as_str() {
                "save" => {
                    home.write("state", b"saved").unwrap();
                    RunOutcome::Completed
                }
                "save-and-hang" => {
                    home.write("state", b"before the kill").unwrap();
                    loop {
                        thread::sleep(Duration::from_secs(1));
                    }
                }
                "block" => {
                    let (started, finish) = self.0.block.lock().unwrap().take().unwrap();
                    started.send(()).unwrap();
                    finish.recv().unwrap();
                    RunOutcome::Completed
                }
                "fail" => RunOutcome::failed("line one\n\tinjected\t2\tcompleted\t"),
                "time" => RunOutcome::TimeLimit,
                "memory" => RunOutcome::MemoryLimit,
                "crash" => RunOutcome::Crashed("trap".to_string()),
                _ => RunOutcome::Completed,
            };
            RunOutput {
                outcome,
                messages: vec![format!("heard {text}")],
            }
        }
    }

    struct Fixture {
        root: TempDir,
        supervisor: Supervisor<FakeBackend>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = TempDir::new().unwrap();
            let supervisor = supervisor_at(root.path(), FakeBackend::default());
            Self { root, supervisor }
        }

        fn installed() -> Self {
            let fixture = Self::new();
            fixture
                .supervisor
                .install(&package_file("one")[..])
                .unwrap();
            fixture
        }

        fn send(&self, text: &str) -> RunReport {
            self.supervisor.run("one", &message(text)).unwrap()
        }

        fn storage(&self) -> AgentStorage {
            AgentStorage::open_existing(self.root.path(), "one").unwrap()
        }

        fn read_state(&self) -> Vec<u8> {
            self.storage()
                .home(1024 * 1024)
                .unwrap()
                .read("state")
                .unwrap()
        }
    }

    fn supervisor_at(root: &Path, backend: FakeBackend) -> Supervisor<FakeBackend> {
        Supervisor::new(root, backend, HostLimits::default())
    }

    fn message(text: &str) -> Event {
        Event::UserMessage(text.to_string())
    }

    #[test]
    fn an_installed_agent_is_listed_with_the_limits_it_actually_gets() {
        let root = TempDir::new().unwrap();
        let host = HostLimits {
            memory_mb: 8,
            run_seconds: 10,
            disk_mb: 1,
        };
        let supervisor = Supervisor::new(root.path(), FakeBackend::default(), host);
        let info = supervisor.install(&package_file("one")[..]).unwrap();
        assert_eq!(info.name, "one");
        assert_eq!(info.version, "1.2.3");
        assert_eq!(info.limits.memory_bytes, 8 * 1024 * 1024);
        assert_eq!(supervisor.agents().unwrap(), [InstalledAgent::Ready(info)]);
    }

    #[test]
    fn nothing_is_installed_yet_before_the_first_install() {
        assert_eq!(Fixture::new().supervisor.agents().unwrap(), []);
    }

    #[test]
    fn agents_are_listed_in_name_order() {
        let fixture = Fixture::new();
        for name in ["zebra", "apple", "mango"] {
            fixture.supervisor.install(&package_file(name)[..]).unwrap();
        }
        let agents = fixture.supervisor.agents().unwrap();
        let names: Vec<&str> = agents.iter().map(agent_name).collect();
        assert_eq!(names, ["apple", "mango", "zebra"]);
    }

    #[test]
    fn a_package_the_backend_refuses_leaves_nothing_behind() {
        let root = TempDir::new().unwrap();
        let backend = FakeBackend {
            refuse: true,
            ..FakeBackend::default()
        };
        let supervisor = supervisor_at(root.path(), backend);
        let error = supervisor.install(&package_file("one")[..]).unwrap_err();
        assert!(matches!(error, SupervisorError::Load(_)), "{error:?}");
        assert!(!AgentStorage::agents_dir(root.path()).join("one").exists());
    }

    #[test]
    fn a_malformed_package_is_refused() {
        let fixture = Fixture::new();
        let error = fixture
            .supervisor
            .install(&b"not a package"[..])
            .unwrap_err();
        assert!(matches!(error, SupervisorError::Package(_)), "{error:?}");
        assert_eq!(fixture.supervisor.agents().unwrap(), []);
    }

    #[test]
    fn installing_over_an_agent_is_refused_and_keeps_its_files() {
        let fixture = Fixture::installed();
        fixture.send("save");
        let error = fixture
            .supervisor
            .install(&package_file("one")[..])
            .unwrap_err();
        assert_eq!(
            error,
            SupervisorError::Package(PackageError::AlreadyInstalled)
        );
        assert_eq!(fixture.read_state(), b"saved");
    }

    #[test]
    fn a_run_reports_its_outcome_and_messages() {
        let report = Fixture::installed().send("hello");
        assert_eq!(report.output.outcome, RunOutcome::Completed);
        assert_eq!(report.output.messages, ["heard hello"]);
    }

    #[test]
    fn each_run_has_its_own_id() {
        let fixture = Fixture::installed();
        let (first, second) = (fixture.send("a").run_id, fixture.send("b").run_id);
        assert_ne!(first, second);
        assert_eq!(first.len(), 16);
    }

    #[test]
    fn unknown_or_invalid_agents_are_refused_and_create_nothing() {
        let fixture = Fixture::new();
        for name in ["missing", "../outside", "UPPER", ""] {
            let run = fixture.supervisor.run(name, &message("x")).unwrap_err();
            assert!(matches!(run, SupervisorError::Home(_)), "{name}: {run:?}");
            let remove = fixture.supervisor.remove(name).unwrap_err();
            assert!(
                matches!(remove, SupervisorError::Home(_)),
                "{name}: {remove:?}"
            );
            let runs = fixture.supervisor.recent_runs(name, 5).unwrap_err();
            assert!(matches!(runs, SupervisorError::Home(_)), "{name}: {runs:?}");
        }
        assert!(!AgentStorage::agents_dir(fixture.root.path()).exists());
    }

    #[test]
    fn a_failed_run_doesnt_stop_the_next_one_or_lose_state() {
        let fixture = Fixture::installed();
        fixture.send("save");
        for text in ["fail", "time", "memory", "crash"] {
            assert_ne!(fixture.send(text).output.outcome, RunOutcome::Completed);
            assert_eq!(fixture.read_state(), b"saved");
        }
        assert_eq!(fixture.send("ok").output.outcome, RunOutcome::Completed);
    }

    #[test]
    fn a_backend_that_cant_load_the_agent_is_recorded_as_an_internal_failure() {
        let root = TempDir::new().unwrap();
        supervisor_at(root.path(), FakeBackend::default())
            .install(&package_file("one")[..])
            .unwrap();
        let backend = FakeBackend {
            refuse: true,
            ..FakeBackend::default()
        };
        let supervisor = supervisor_at(root.path(), backend);
        let report = supervisor.run("one", &message("x")).unwrap();
        assert!(matches!(report.output.outcome, RunOutcome::Internal(_)));
        let runs = supervisor.recent_runs("one", 1).unwrap();
        assert!(matches!(
            runs[0].status,
            RunStatus::Finished(RunOutcome::Internal(_))
        ));
    }

    #[test]
    fn runs_are_recorded_newest_first_and_up_to_the_limit() {
        let fixture = Fixture::installed();
        let first = fixture.send("a");
        let second = fixture.send("time");
        let third = fixture.send("crash");
        let runs = fixture.supervisor.recent_runs("one", 2).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].run_id, third.run_id);
        assert_eq!(
            runs[0].status,
            RunStatus::Finished(RunOutcome::Crashed("trap".to_string()))
        );
        assert_eq!(runs[1].run_id, second.run_id);
        assert_eq!(runs[1].status, RunStatus::Finished(RunOutcome::TimeLimit));
        assert!(runs[1].finished_ms.unwrap() >= runs[1].started_ms);
        let all = fixture.supervisor.recent_runs("one", 10).unwrap();
        assert_eq!(all[2].run_id, first.run_id);
    }

    #[test]
    fn a_hostile_failure_message_cant_forge_a_run_record() {
        let fixture = Fixture::installed();
        fixture.send("fail");
        let runs = fixture.supervisor.recent_runs("one", 10).unwrap();
        assert_eq!(runs.len(), 1);
        let RunStatus::Finished(RunOutcome::Failed(detail)) = &runs[0].status else {
            panic!("{:?}", runs[0].status);
        };
        assert!(detail.contains("injected"), "{detail}");
        assert_eq!(
            fs::read_to_string(fixture.storage().runs_path())
                .unwrap()
                .lines()
                .count(),
            2
        );
    }

    #[test]
    fn the_run_record_holds_neither_side_of_the_conversation() {
        let fixture = Fixture::installed();
        fixture.send("my private words");
        let record = fs::read_to_string(fixture.storage().runs_path()).unwrap();
        assert!(!record.contains("private"), "{record}");
        assert!(!record.contains("heard"), "{record}");
    }

    #[test]
    fn the_run_record_stays_a_bounded_size_and_keeps_the_latest_runs() {
        let fixture = Fixture::installed();
        let path = fixture.storage().runs_path();
        fs::write(&path, "x".repeat(MAX_RUNS_LOG_BYTES as usize + 1)).unwrap();
        let report = fixture.send("a");
        assert!(fs::metadata(&path).unwrap().len() < MAX_RUNS_LOG_BYTES);
        let runs = fixture.supervisor.recent_runs("one", 1).unwrap();
        assert_eq!(runs[0].run_id, report.run_id);
    }

    #[test]
    fn a_line_cut_short_by_a_crash_is_skipped() {
        let fixture = Fixture::installed();
        let report = fixture.send("a");
        let path = fixture.storage().runs_path();
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(b"deadbeef\t12").unwrap();
        let runs = fixture.supervisor.recent_runs("one", 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, report.run_id);
    }

    #[test]
    fn only_one_run_at_a_time_and_a_running_agent_cant_be_removed() {
        let fixture = Fixture::new();
        fixture
            .supervisor
            .install(&package_file("one")[..])
            .unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        *fixture.supervisor.backend.block.lock().unwrap() = Some((started_tx, finish_rx));

        thread::scope(|scope| {
            let running = scope.spawn(|| fixture.send("block"));
            started_rx.recv().unwrap();

            let second = fixture.supervisor.run("one", &message("x")).unwrap_err();
            assert_eq!(second, SupervisorError::Busy);
            assert_eq!(
                fixture.supervisor.remove("one").unwrap_err(),
                SupervisorError::Busy
            );
            let runs = fixture.supervisor.recent_runs("one", 5).unwrap();
            assert_eq!(runs[0].status, RunStatus::Running);

            finish_tx.send(()).unwrap();
            assert_eq!(
                running.join().unwrap().output.outcome,
                RunOutcome::Completed
            );
        });
        assert_eq!(fixture.send("next").output.outcome, RunOutcome::Completed);
    }

    #[test]
    fn removing_an_agent_deletes_only_that_agent() {
        let fixture = Fixture::installed();
        fixture.send("save");
        fixture
            .supervisor
            .install(&package_file("two")[..])
            .unwrap();
        fixture.supervisor.remove("one").unwrap();

        let agents = fixture.supervisor.agents().unwrap();
        let names: Vec<&str> = agents.iter().map(agent_name).collect();
        assert_eq!(names, ["two"]);
        assert!(
            !AgentStorage::agents_dir(fixture.root.path())
                .join("one")
                .exists()
        );
        assert!(matches!(
            fixture.supervisor.run("one", &message("x")),
            Err(SupervisorError::Home(HomeError::NotFound))
        ));
        fixture
            .supervisor
            .install(&package_file("one")[..])
            .unwrap();
    }

    #[test]
    fn a_changed_package_is_listed_as_damaged_and_wont_run_but_can_be_removed() {
        let fixture = Fixture::installed();
        let component = fixture.storage().package_path().join("agent.wasm");
        fs::remove_file(&component).unwrap();
        fs::write(&component, b"\0asm\x0d\0\x01\0changed").unwrap();

        let agents = fixture.supervisor.agents().unwrap();
        assert!(matches!(&agents[0], InstalledAgent::Damaged { name, .. } if name == "one"));
        let error = fixture.supervisor.run("one", &message("x")).unwrap_err();
        assert!(matches!(error, SupervisorError::Package(_)), "{error:?}");
        assert!(fixture.supervisor.recent_runs("one", 5).unwrap().is_empty());
        fixture.supervisor.remove("one").unwrap();
    }

    #[test]
    fn an_interrupted_install_can_be_run_again_and_isnt_listed() {
        let fixture = Fixture::new();
        AgentStorage::open(fixture.root.path(), "one").unwrap();
        assert_eq!(fixture.supervisor.agents().unwrap(), []);
        fixture
            .supervisor
            .install(&package_file("one")[..])
            .unwrap();
        assert_eq!(fixture.supervisor.agents().unwrap().len(), 1);
    }

    #[test]
    #[ignore = "run by killing_a_run_keeps_saved_state_and_is_recorded"]
    fn crash_test_runner() {
        let dir = env::var(CRASH_DIR_VAR).expect("only for the crash test");
        let supervisor = supervisor_at(Path::new(&dir), FakeBackend::default());
        supervisor.run("one", &message("save-and-hang")).unwrap();
    }

    #[test]
    fn killing_a_run_keeps_saved_state_and_is_recorded() {
        let fixture = Fixture::installed();
        let state = fixture
            .storage()
            .package_path()
            .with_file_name("home")
            .join("state");

        let mut child = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "supervisor::tests::crash_test_runner",
                "--ignored",
            ])
            .env(CRASH_DIR_VAR, fixture.root.path())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        while !state.exists() {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            fixture.supervisor.run("one", &message("x")).unwrap_err(),
            SupervisorError::Busy
        );
        child.kill().unwrap();
        child.wait().unwrap();

        // A restarted host sees the saved state, a record of what happened, and no leftover lock.
        let restarted = supervisor_at(fixture.root.path(), FakeBackend::default());
        assert_eq!(fixture.read_state(), b"before the kill");
        let runs = restarted.recent_runs("one", 5).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Interrupted);
        assert_eq!(runs[0].finished_ms, None);
        let next = restarted.run("one", &message("x")).unwrap();
        assert_eq!(next.output.outcome, RunOutcome::Completed);
    }
}
