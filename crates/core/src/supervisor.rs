//! Takes an agent through one run: it starts, handles one event, saves its state and exits.
//!
//! Every failure ends up in the run's record, and leaves the agent's files as they were, so the
//! next run starts normally. See `docs/architecture.md`.

use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use semver::Version;

use crate::backend::{Backend, Event, HostLimits, RunOutcome, StopSignal};
use crate::home::{AgentStorage, HomeError};
use crate::package::{Package, PackageError};
use crate::run_log::{RunEnding, RunHistory, RunLog, RunRecord};

/// Why something the supervisor was asked to do didn't happen. Messages hold no host paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorError {
    /// That can't be the name of an agent.
    InvalidName,
    /// No package is installed for this agent.
    NotInstalled,
    /// This agent is running, on this host process or another one, or something else is using
    /// its folder for a moment.
    AlreadyRunning,
    /// The host is shutting down, so it isn't starting anything new.
    ShuttingDown,
    /// The run couldn't be recorded, so the agent wasn't started.
    NotRecorded(io::ErrorKind),
    /// The agent ran, but how it ended couldn't be saved. This is how it ended.
    EndingNotSaved(Box<RunRecord>),
    /// The package couldn't be installed.
    Package(PackageError),
    /// The host's file system failed.
    Storage(io::ErrorKind),
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => write!(f, "that isn't a valid agent name"),
            Self::NotInstalled => write!(f, "that agent isn't installed"),
            Self::AlreadyRunning => write!(f, "that agent is already running"),
            Self::ShuttingDown => write!(f, "the host is shutting down"),
            Self::NotRecorded(kind) => {
                write!(
                    f,
                    "the run couldn't be recorded, so it didn't start: {kind}"
                )
            }
            Self::EndingNotSaved(record) => {
                write!(f, "the ending of run {} couldn't be saved", record.id)
            }
            Self::Package(error) => write!(f, "{error}"),
            Self::Storage(kind) => write!(f, "file system error: {kind}"),
        }
    }
}

impl std::error::Error for SupervisorError {}

/// Held for as long as an agent is running. The operating system lets go of it when the process
/// ends, however that happens, so a killed host can't leave an agent locked.
struct RunLock {
    _file: File,
}

impl RunLock {
    fn try_acquire(storage: &AgentStorage) -> Result<Self, SupervisorError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(storage.run_lock_path())
            .map_err(not_recorded)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(SupervisorError::AlreadyRunning),
            Err(TryLockError::Error(error)) => Err(not_recorded(error)),
        }
    }
}

fn not_recorded(error: io::Error) -> SupervisorError {
    SupervisorError::NotRecorded(error.kind())
}

/// What's known about one installed agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSummary {
    /// Safe to print, like everything else here.
    pub name: String,
    /// `None` if its manifest can't be read any more, which only something else could cause.
    pub version: Option<Version>,
    pub last_run: Option<RunRecord>,
}

/// Starts agents, and stops them when the host shuts down. A host should have one of these, and
/// can share it between threads. Each run takes place on the thread that asks for it.
pub struct Supervisor {
    root: PathBuf,
    backend: Box<dyn Backend>,
    limits: HostLimits,
    stop: StopSignal,
    active_runs: Mutex<usize>,
    run_ended: Condvar,
}

impl Supervisor {
    /// Makes a supervisor for the agents under `root`, which is chosen by the host and is trusted.
    pub fn new(root: &Path, backend: impl Backend + 'static, limits: HostLimits) -> Self {
        Self {
            root: root.to_path_buf(),
            backend: Box::new(backend),
            limits,
            stop: StopSignal::default(),
            active_runs: Mutex::new(0),
            run_ended: Condvar::new(),
        }
    }

    /// Installs a package, under the name in its manifest. Its home is created empty.
    pub fn install(&self, package: &Package) -> Result<(), SupervisorError> {
        let storage = AgentStorage::open(&self.root, package.manifest().name())
            .map_err(|error| storage_error(&error))?;
        let _lock = RunLock::try_acquire(&storage)?;
        package.install(&storage).map_err(SupervisorError::Package)
    }

    /// Lists the installed agents, sorted by name.
    pub fn list(&self) -> Result<Vec<AgentSummary>, SupervisorError> {
        let names = AgentStorage::names(&self.root)
            .map_err(|error| SupervisorError::Storage(error.kind()))?;
        let mut agents = Vec::new();
        for name in names {
            // Any folder without a recorded package is an install that didn't finish.
            let Ok(storage) = self.installed_agent(&name) else {
                continue;
            };
            let history = self.history(&name).ok();
            agents.push(AgentSummary {
                version: Package::installed_version(&storage).ok(),
                last_run: history.and_then(|history| history.runs.into_iter().last()),
                name,
            });
        }
        Ok(agents)
    }

    /// Deletes an agent, with its package, its home and its records, and nothing else. This is
    /// refused while the agent is running.
    pub fn remove(&self, agent_name: &str) -> Result<(), SupervisorError> {
        let storage = open_existing(&self.root, agent_name)?;
        let _lock = RunLock::try_acquire(&storage)?;
        storage
            .remove()
            .map_err(|error| SupervisorError::Storage(error.kind()))
    }

    /// Whether [`Supervisor::shut_down`] has been called.
    pub fn is_shutting_down(&self) -> bool {
        self.stop.is_stopped()
    }

    /// Runs an agent for one event, and waits for it to end. Whatever the agent does, this returns
    /// a record of it, and the next run can start. Anything the agent saved before it failed is
    /// still there, because each write to its home is saved as it happens.
    pub fn run(&self, agent_name: &str, event: &Event) -> Result<RunRecord, SupervisorError> {
        let _active = self.count_this_run()?;
        let storage = self.installed_agent(agent_name)?;
        let _lock = RunLock::try_acquire(&storage)?;

        // Nothing runs unless there's a record of it first.
        let log = RunLog::open(&storage).map_err(not_recorded)?;
        let mut record = log.begin(event).map_err(not_recorded)?;

        // A panic is a bug in the host, but it shouldn't let one agent take down the others. Its
        // message isn't kept, because it could hold text from the agent.
        let ending = panic::catch_unwind(AssertUnwindSafe(|| self.load_and_run(&storage, event)))
            .unwrap_or_else(|_| {
                RunEnding::Ran(RunOutcome::Internal("the host panicked".to_string()))
            });
        record.end(ending);

        let saved = log.save(&record);
        // Only matters over time, and another run will try again, so this can't fail the run.
        let _ = log.prune();
        match saved {
            Ok(()) => Ok(record),
            Err(_) => Err(SupervisorError::EndingNotSaved(Box::new(record))),
        }
    }

    fn load_and_run(&self, storage: &AgentStorage, event: &Event) -> RunEnding {
        // This checks that the package hasn't changed since it was installed.
        let package = match Package::load_installed(storage) {
            Ok(package) => package,
            Err(error) => return RunEnding::NotStarted(error.to_string()),
        };
        let limits = self.limits.limits_for(&package.manifest().limits());
        let agent = match self.backend.load(&package) {
            Ok(agent) => agent,
            Err(error) => return RunEnding::NotStarted(error.to_string()),
        };
        // Compiling can take a while, and can't be interrupted.
        if self.stop.is_stopped() {
            return RunEnding::NotStarted(SupervisorError::ShuttingDown.to_string());
        }
        let home = match storage.home(limits.disk_bytes) {
            Ok(home) => home,
            Err(error) => return RunEnding::NotStarted(error.to_string()),
        };
        RunEnding::Ran(agent.run(event, home, &limits, &self.stop))
    }

    /// An agent's recent runs. A run that's going has no ending yet, and one that was interrupted
    /// is marked as that. This holds the agent's lock while it reads, so a run asked for at that
    /// moment is refused with [`SupervisorError::AlreadyRunning`], and can be asked for again.
    pub fn history(&self, agent_name: &str) -> Result<RunHistory, SupervisorError> {
        let storage = open_existing(&self.root, agent_name)?;
        let log = RunLog::open(&storage).map_err(not_recorded)?;
        let lock = match RunLock::try_acquire(&storage) {
            Ok(lock) => {
                log.close_interrupted().map_err(not_recorded)?;
                Some(lock)
            }
            Err(SupervisorError::AlreadyRunning) => None,
            Err(error) => return Err(error),
        };
        let history = log.history().map_err(not_recorded)?;
        drop(lock);
        Ok(history)
    }

    /// Tells every run to stop, refuses any new ones, and waits up to `wait` for them to end.
    /// Returns whether they all did. If not, one is still compiling or in a host call. The host
    /// can exit anyway, as that run's record will be marked as interrupted, and the agent's files
    /// are safe.
    pub fn shut_down(&self, wait: Duration) -> bool {
        self.stop.stop();
        let deadline = Instant::now() + wait;
        let mut active = self.active_runs();
        while *active > 0 {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            active = self
                .run_ended
                .wait_timeout(active, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Checks under the same lock that `shut_down` waits on, so that it can't miss a run that's
    /// just starting.
    fn count_this_run(&self) -> Result<ActiveRun<'_>, SupervisorError> {
        let mut active = self.active_runs();
        if self.stop.is_stopped() {
            return Err(SupervisorError::ShuttingDown);
        }
        *active += 1;
        Ok(ActiveRun { supervisor: self })
    }

    fn active_runs(&self) -> MutexGuard<'_, usize> {
        self.active_runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn installed_agent(&self, agent_name: &str) -> Result<AgentStorage, SupervisorError> {
        let storage = open_existing(&self.root, agent_name)?;
        match storage.package_hash_path().symlink_metadata() {
            Ok(_) => Ok(storage),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Err(SupervisorError::NotInstalled)
            }
            Err(error) => Err(not_recorded(error)),
        }
    }
}

fn open_existing(root: &Path, agent_name: &str) -> Result<AgentStorage, SupervisorError> {
    AgentStorage::open_existing(root, agent_name).map_err(|error| match error {
        HomeError::InvalidPath(_) => SupervisorError::InvalidName,
        HomeError::NotFound => SupervisorError::NotInstalled,
        other => storage_error(&other),
    })
}

fn storage_error(error: &HomeError) -> SupervisorError {
    SupervisorError::Storage(match error {
        HomeError::Io(kind) => *kind,
        _ => io::ErrorKind::Other,
    })
}

/// Counts a run for as long as this exists, even if something panics.
struct ActiveRun<'a> {
    supervisor: &'a Supervisor,
}

impl Drop for ActiveRun<'_> {
    fn drop(&mut self) {
        *self.supervisor.active_runs() -= 1;
        self.supervisor.run_ended.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread;

    use tempfile::TempDir;

    use super::*;
    use crate::backend::{LoadError, LoadedAgent, RunLimits};
    use crate::home::Home;
    use crate::run_log::{EventKind, MAX_RECORDS, RunId};

    const NAME: &str = "agent";
    const MANIFEST: &str = "[agent]\nname = \"agent\"\nversion = \"0.1.0\"\n\n[limits]\nmemory_mb = 8\nrun_seconds = 7\ndisk_mb = 1\n";
    const COMPONENT: &[u8] = b"\0asm\x0d\0\x01\0";
    /// How long a test waits for something, so that it fails rather than hangs.
    const WAIT_LIMIT: Duration = Duration::from_secs(60);

    type Behaviour = dyn Fn(&Event, &mut Home, &RunLimits, &StopSignal) -> RunOutcome + Send + Sync;

    /// A backend that does what a test tells it to, with no real agent.
    #[derive(Clone)]
    struct FakeBackend {
        behaviour: Arc<Behaviour>,
        before_load: Arc<dyn Fn() -> Result<(), LoadError> + Send + Sync>,
    }

    struct FakeAgent(Arc<Behaviour>);

    impl FakeBackend {
        fn new(
            behaviour: impl Fn(&Event, &mut Home, &RunLimits, &StopSignal) -> RunOutcome
            + Send
            + Sync
            + 'static,
        ) -> Self {
            Self {
                behaviour: Arc::new(behaviour),
                before_load: Arc::new(|| Ok(())),
            }
        }
    }

    impl Backend for FakeBackend {
        fn load(&self, _: &Package) -> Result<Box<dyn LoadedAgent>, LoadError> {
            (self.before_load)()?;
            Ok(Box::new(FakeAgent(Arc::clone(&self.behaviour))))
        }
    }

    impl LoadedAgent for FakeAgent {
        fn run(
            &self,
            event: &Event,
            mut home: Home,
            limits: &RunLimits,
            stop: &StopSignal,
        ) -> RunOutcome {
            (self.0)(event, &mut home, limits, stop)
        }
    }

    fn message(text: &str) -> Event {
        Event::UserMessage(text.to_string())
    }

    fn text(event: &Event) -> &str {
        match event {
            Event::UserMessage(text) | Event::Scheduled(text) => text,
        }
    }

    /// Saves a file, checks that it's there, panics or completes, depending on the message.
    fn scripted() -> FakeBackend {
        FakeBackend::new(|event, home, _, _| match text(event) {
            "write" => {
                home.write("saved", b"state").unwrap();
                RunOutcome::Completed
            }
            "check" => {
                assert_eq!(home.read("saved").unwrap(), b"state");
                RunOutcome::Completed
            }
            "panic" => panic!("told to"),
            _ => RunOutcome::Completed,
        })
    }

    struct Fixture {
        root: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let fixture = Self {
                root: TempDir::new().unwrap(),
            };
            fixture.install(NAME, MANIFEST);
            fixture
        }

        fn install(&self, name: &str, manifest: &str) {
            let storage = AgentStorage::open(self.root.path(), name).unwrap();
            Package::new(manifest, COMPONENT.to_vec())
                .unwrap()
                .install(&storage)
                .unwrap();
        }

        fn supervisor(&self, backend: FakeBackend) -> Supervisor {
            Supervisor::new(self.root.path(), backend, HostLimits::default())
        }

        fn agent_path(&self) -> PathBuf {
            self.root.path().join("agents").join(NAME)
        }

        fn record_count(&self) -> usize {
            fs::read_dir(self.agent_path().join("runs"))
                .unwrap()
                .count()
        }
    }

    fn ending(record: &RunRecord) -> &RunOutcome {
        match &record.ending {
            Some(RunEnding::Ran(outcome)) => outcome,
            other => panic!("{other:?}"),
        }
    }

    /// A backend whose agent holds still until the test lets it go, and signals when it starts.
    fn held(started: Sender<()>, release: Receiver<()>) -> FakeBackend {
        let started = Mutex::new(started);
        let release = Mutex::new(release);
        FakeBackend::new(move |_, _, _, _| {
            started.lock().unwrap().send(()).unwrap();
            let _ = release.lock().unwrap().recv_timeout(WAIT_LIMIT);
            RunOutcome::Completed
        })
    }

    #[test]
    fn a_run_is_recorded_from_start_to_end() {
        let fixture = Fixture::new();
        let supervisor = fixture.supervisor(scripted());
        let record = supervisor.run(NAME, &message("anything")).unwrap();

        assert_eq!(record.event, EventKind::UserMessage);
        assert_eq!(ending(&record), &RunOutcome::Completed);
        assert!(record.started_at_ms > 0);
        assert!(record.ended_at_ms.unwrap() >= record.started_at_ms);
        let history = supervisor.history(NAME).unwrap();
        assert_eq!(history.runs, vec![record]);
        assert_eq!(history.unreadable, 0);
    }

    #[test]
    fn the_run_is_on_record_before_the_agent_starts() {
        let fixture = Fixture::new();
        let runs = fixture.agent_path().join("runs");
        let backend = FakeBackend::new(move |_, _, _, _| {
            assert_eq!(fs::read_dir(&runs).unwrap().count(), 1);
            RunOutcome::Completed
        });
        let supervisor = fixture.supervisor(backend);
        let record = supervisor.run(NAME, &message("x")).unwrap();
        assert_eq!(ending(&record), &RunOutcome::Completed);
    }

    #[test]
    fn the_kind_of_event_is_recorded() {
        let fixture = Fixture::new();
        let supervisor = fixture.supervisor(scripted());
        let record = supervisor
            .run(NAME, &Event::Scheduled("wake".to_string()))
            .unwrap();
        assert_eq!(record.event, EventKind::Scheduled);
    }

    #[test]
    fn the_agent_gets_the_event_and_its_limits_within_the_hosts() {
        let fixture = Fixture::new();
        let backend = FakeBackend::new(|event, _, limits, _| {
            assert_eq!(text(event), "hello");
            assert_eq!(
                *limits,
                RunLimits {
                    memory_bytes: 4 * 1024 * 1024,
                    run_time: Duration::from_secs(7),
                    disk_bytes: 1024 * 1024,
                }
            );
            RunOutcome::Completed
        });
        let host = HostLimits {
            memory_mb: 4,
            run_seconds: 100,
            disk_mb: 100,
        };
        let supervisor = Supervisor::new(fixture.root.path(), backend, host);
        let record = supervisor.run(NAME, &message("hello")).unwrap();
        assert_eq!(ending(&record), &RunOutcome::Completed);
    }

    #[test]
    fn the_homes_disk_limit_comes_from_the_manifest() {
        let fixture = Fixture::new();
        let backend = FakeBackend::new(|_, home, _, _| {
            let too_big = vec![0; 1024 * 1024 + 1];
            assert!(matches!(
                home.write("big", &too_big),
                Err(HomeError::DiskLimit { .. })
            ));
            RunOutcome::Completed
        });
        let supervisor = fixture.supervisor(backend);
        let record = supervisor.run(NAME, &message("x")).unwrap();
        assert_eq!(ending(&record), &RunOutcome::Completed);
    }

    #[test]
    fn state_survives_a_restart() {
        let fixture = Fixture::new();
        fixture
            .supervisor(scripted())
            .run(NAME, &message("write"))
            .unwrap();
        let record = fixture
            .supervisor(scripted())
            .run(NAME, &message("check"))
            .unwrap();
        assert_eq!(ending(&record), &RunOutcome::Completed);
    }

    #[test]
    fn a_failed_run_keeps_what_it_saved_and_the_next_run_starts() {
        let fixture = Fixture::new();
        let backend = FakeBackend::new(|event, home, _, _| match text(event) {
            "fail" => {
                home.write("saved", b"before failing").unwrap();
                RunOutcome::failed("it went wrong")
            }
            _ => {
                assert_eq!(home.read("saved").unwrap(), b"before failing");
                RunOutcome::Completed
            }
        });
        let supervisor = fixture.supervisor(backend);
        let failed = supervisor.run(NAME, &message("fail")).unwrap();
        assert_eq!(
            ending(&failed),
            &RunOutcome::Failed("it went wrong".to_string())
        );
        let next = supervisor.run(NAME, &message("check")).unwrap();
        assert_eq!(ending(&next), &RunOutcome::Completed);
        assert_eq!(supervisor.history(NAME).unwrap().runs, vec![failed, next]);
    }

    #[test]
    fn every_kind_of_ending_lets_the_next_run_start() {
        let outcomes = [
            RunOutcome::failed("no"),
            RunOutcome::TimeLimit,
            RunOutcome::MemoryLimit,
            RunOutcome::Crashed("trap".to_string()),
            RunOutcome::Internal("oops".to_string()),
        ];
        for outcome in outcomes {
            let fixture = Fixture::new();
            let expected = outcome.clone();
            let backend = FakeBackend::new(move |event, _, _, _| match text(event) {
                "bad" => outcome.clone(),
                _ => RunOutcome::Completed,
            });
            let supervisor = fixture.supervisor(backend);
            let bad = supervisor.run(NAME, &message("bad")).unwrap();
            assert_eq!(ending(&bad), &expected);
            let next = supervisor.run(NAME, &message("next")).unwrap();
            assert_eq!(ending(&next), &RunOutcome::Completed);
        }
    }

    #[test]
    fn every_run_has_its_own_id_in_order() {
        let fixture = Fixture::new();
        let supervisor = fixture.supervisor(scripted());
        let ids: Vec<RunId> = (0..5)
            .map(|_| supervisor.run(NAME, &message("x")).unwrap().id)
            .collect();
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "{ids:?}");
        let later = fixture
            .supervisor(scripted())
            .run(NAME, &message("x"))
            .unwrap();
        assert!(ids[4] < later.id);
    }

    #[test]
    fn a_panic_is_recorded_and_the_next_run_starts() {
        let fixture = Fixture::new();
        let supervisor = fixture.supervisor(scripted());
        let record = supervisor.run(NAME, &message("panic")).unwrap();
        assert_eq!(
            ending(&record),
            &RunOutcome::Internal("the host panicked".to_string())
        );
        let next = supervisor.run(NAME, &message("x")).unwrap();
        assert_eq!(ending(&next), &RunOutcome::Completed);
        assert!(supervisor.shut_down(Duration::ZERO));
    }

    #[test]
    fn a_refused_package_is_recorded_and_doesnt_run() {
        let fixture = Fixture::new();
        let mut backend = FakeBackend::new(|_, _, _, _| panic!("this shouldn't run"));
        backend.before_load = Arc::new(|| Err(LoadError::TooCostly("too much".to_string())));
        let supervisor = fixture.supervisor(backend);
        let record = supervisor.run(NAME, &message("x")).unwrap();
        assert_eq!(
            record.ending,
            Some(RunEnding::NotStarted(
                "the agent's code is too costly to compile: too much".to_string()
            ))
        );
        let again = supervisor.run(NAME, &message("x")).unwrap();
        assert!(matches!(again.ending, Some(RunEnding::NotStarted(_))));
    }

    #[test]
    fn a_changed_package_doesnt_run() {
        let fixture = Fixture::new();
        let hash_file = fixture.agent_path().join("package.sha256");
        fs::write(&hash_file, format!("{}\n", "0".repeat(64))).unwrap();
        let backend = FakeBackend::new(|_, _, _, _| panic!("this shouldn't run"));
        let record = fixture
            .supervisor(backend)
            .run(NAME, &message("x"))
            .unwrap();
        let Some(RunEnding::NotStarted(reason)) = &record.ending else {
            panic!("{record:?}");
        };
        assert!(reason.contains("hash"), "{reason}");
    }

    #[test]
    fn a_home_that_cant_be_opened_is_recorded_and_doesnt_run() {
        let fixture = Fixture::new();
        // Opening a home clears out its temporary files, and it can't remove a folder.
        fs::create_dir(fixture.agent_path().join("tmp/folder")).unwrap();
        let backend = FakeBackend::new(|_, _, _, _| panic!("this shouldn't run"));
        let record = fixture
            .supervisor(backend)
            .run(NAME, &message("x"))
            .unwrap();
        assert!(matches!(record.ending, Some(RunEnding::NotStarted(_))));
    }

    #[test]
    fn an_agent_that_isnt_installed_doesnt_run_or_leave_anything_behind() {
        let fixture = Fixture::new();
        let backend = FakeBackend::new(|_, _, _, _| panic!("this shouldn't run"));
        let supervisor = fixture.supervisor(backend);
        assert_eq!(
            supervisor.run("nobody", &message("x")),
            Err(SupervisorError::NotInstalled)
        );
        assert_eq!(
            supervisor.history("nobody").map(|_| ()),
            Err(SupervisorError::NotInstalled)
        );
        assert!(!fixture.root.path().join("agents/nobody").exists());

        AgentStorage::open(fixture.root.path(), "empty").unwrap();
        assert_eq!(
            supervisor.run("empty", &message("x")),
            Err(SupervisorError::NotInstalled)
        );
    }

    #[test]
    fn bad_names_are_refused() {
        let fixture = Fixture::new();
        let supervisor = fixture.supervisor(scripted());
        for name in ["", "..", "../agent", "a/b", "/etc", "Agent\n"] {
            assert_eq!(
                supervisor.run(name, &message("x")),
                Err(SupervisorError::InvalidName),
                "{name:?}"
            );
            assert_eq!(
                supervisor.history(name).map(|_| ()),
                Err(SupervisorError::InvalidName),
                "{name:?}"
            );
        }
    }

    #[test]
    fn a_run_that_cant_be_recorded_doesnt_start() {
        let fixture = Fixture::new();
        let runs = fixture.agent_path().join("runs");
        let unfinished = "event = \"scheduled\"\nstarted_at_ms = 1\n";
        let id = "0".repeat(32);
        fs::write(runs.join(format!("{id}.toml")), unfinished).unwrap();
        // This is where its ending would be written to first, and a folder can't be.
        fs::create_dir(runs.join(format!("{id}.new"))).unwrap();
        let backend = FakeBackend::new(|_, _, _, _| panic!("this shouldn't run"));
        let supervisor = fixture.supervisor(backend);
        let error = supervisor.run(NAME, &message("x")).unwrap_err();
        assert!(
            matches!(error, SupervisorError::NotRecorded(_)),
            "{error:?}"
        );

        // The same goes if there's nowhere for records to go at all.
        fs::remove_dir_all(&runs).unwrap();
        fs::write(&runs, "not a folder").unwrap();
        let error = supervisor.run(NAME, &message("x")).unwrap_err();
        assert!(matches!(error, SupervisorError::Storage(_)), "{error:?}");
    }

    #[test]
    fn an_ending_that_cant_be_saved_is_still_reported() {
        let fixture = Fixture::new();
        let runs = fixture.agent_path().join("runs");
        let backend = FakeBackend::new(move |_, _, _, _| {
            fs::remove_dir_all(&runs).unwrap();
            RunOutcome::failed("how it went")
        });
        let supervisor = fixture.supervisor(backend);
        let Err(SupervisorError::EndingNotSaved(record)) = supervisor.run(NAME, &message("x"))
        else {
            panic!("the run should have reported this");
        };
        assert_eq!(
            ending(&record),
            &RunOutcome::Failed("how it went".to_string())
        );
    }

    #[test]
    fn only_one_run_of_an_agent_at_a_time() {
        let fixture = Fixture::new();
        let (started, has_started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let supervisor = fixture.supervisor(held(started, released));
        thread::scope(|scope| {
            let first = scope.spawn(|| supervisor.run(NAME, &message("first")));
            has_started.recv().unwrap();

            assert_eq!(
                supervisor.run(NAME, &message("second")),
                Err(SupervisorError::AlreadyRunning)
            );
            // Nor can a second supervisor get around it, as another process would try to.
            assert_eq!(
                fixture.supervisor(scripted()).run(NAME, &message("second")),
                Err(SupervisorError::AlreadyRunning)
            );
            assert_eq!(fixture.record_count(), 1);

            release.send(()).unwrap();
            let record = first.join().unwrap().unwrap();
            assert_eq!(ending(&record), &RunOutcome::Completed);
        });
        // The refused runs left no trace and no lock.
        assert_eq!(fixture.record_count(), 1);
        let (release_again, released_again) = mpsc::channel();
        let (started_again, _has_started_again) = mpsc::channel();
        drop(release_again);
        let supervisor = fixture.supervisor(held(started_again, released_again));
        supervisor.run(NAME, &message("third")).unwrap();
    }

    #[test]
    fn different_agents_can_run_at_the_same_time() {
        let fixture = Fixture::new();
        fixture.install(
            "other",
            "[agent]\nname = \"other\"\nversion = \"0.1.0\"\n\n[limits]\nmemory_mb = 8\nrun_seconds = 7\ndisk_mb = 1\n",
        );
        // Each waits for the other, so this only completes if they're both running at once.
        let running = AtomicUsize::new(0);
        let supervisor = fixture.supervisor(FakeBackend::new(move |_, _, _, _| {
            running.fetch_add(1, Ordering::SeqCst);
            let start = Instant::now();
            while running.load(Ordering::SeqCst) < 2 {
                if start.elapsed() > WAIT_LIMIT {
                    return RunOutcome::failed("the other agent never started");
                }
                thread::sleep(Duration::from_millis(1));
            }
            RunOutcome::Completed
        }));
        thread::scope(|scope| {
            let first = scope.spawn(|| supervisor.run(NAME, &message("x")));
            let second = scope.spawn(|| supervisor.run("other", &message("x")));
            assert_eq!(
                ending(&first.join().unwrap().unwrap()),
                &RunOutcome::Completed
            );
            assert_eq!(
                ending(&second.join().unwrap().unwrap()),
                &RunOutcome::Completed
            );
        });
    }

    #[test]
    fn a_run_that_never_recorded_an_ending_shows_as_interrupted() {
        let fixture = Fixture::new();
        let storage = AgentStorage::open_existing(fixture.root.path(), NAME).unwrap();
        // What a killed host leaves behind.
        let killed = RunLog::open(&storage)
            .unwrap()
            .begin(&message("x"))
            .unwrap();

        let supervisor = fixture.supervisor(scripted());
        let history = supervisor.history(NAME).unwrap();
        assert_eq!(history.runs[0].id, killed.id);
        assert_eq!(history.runs[0].ending, Some(RunEnding::Interrupted));
        assert_eq!(history.runs[0].ended_at_ms, None);

        let next = supervisor.run(NAME, &message("x")).unwrap();
        assert_eq!(ending(&next), &RunOutcome::Completed);
    }

    #[test]
    fn a_run_going_on_has_no_ending_in_the_history() {
        let fixture = Fixture::new();
        let (started, has_started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let supervisor = fixture.supervisor(held(started, released));
        thread::scope(|scope| {
            let run = scope.spawn(|| supervisor.run(NAME, &message("x")));
            has_started.recv().unwrap();
            let history = supervisor.history(NAME).unwrap();
            assert_eq!(history.runs.len(), 1);
            assert_eq!(history.runs[0].ending, None);
            release.send(()).unwrap();
            run.join().unwrap().unwrap();
        });
    }

    #[test]
    fn reading_the_history_keeps_nothing_locked() {
        let fixture = Fixture::new();
        let supervisor = fixture.supervisor(scripted());
        supervisor.history(NAME).unwrap();
        supervisor.run(NAME, &message("x")).unwrap();
    }

    #[test]
    fn old_records_are_removed_as_new_ones_arrive() {
        let fixture = Fixture::new();
        let supervisor = fixture.supervisor(scripted());
        let first = supervisor.run(NAME, &message("x")).unwrap();
        for index in 0..MAX_RECORDS {
            let name = format!("{:032x}.toml", index + 1);
            fs::write(
                fixture.agent_path().join("runs").join(name),
                "event = \"scheduled\"\nstarted_at_ms = 1\nending = \"completed\"\n",
            )
            .unwrap();
        }
        let latest = supervisor.run(NAME, &message("x")).unwrap();
        assert_eq!(fixture.record_count(), MAX_RECORDS);
        let history = supervisor.history(NAME).unwrap();
        assert_eq!(history.runs.last(), Some(&latest));
        assert!(first.id < latest.id);
    }

    #[test]
    fn shutting_down_with_nothing_running_is_immediate_and_refuses_new_runs() {
        let fixture = Fixture::new();
        let backend = FakeBackend::new(|_, _, _, _| panic!("this shouldn't run"));
        let supervisor = fixture.supervisor(backend);
        assert!(!supervisor.is_shutting_down());
        assert!(supervisor.shut_down(Duration::ZERO));
        assert!(supervisor.is_shutting_down());
        assert_eq!(
            supervisor.run(NAME, &message("x")),
            Err(SupervisorError::ShuttingDown)
        );
        assert_eq!(fixture.record_count(), 0);
        // The history can still be read.
        supervisor.history(NAME).unwrap();
    }

    #[test]
    fn shutting_down_tells_a_run_to_stop_and_waits_for_it() {
        let fixture = Fixture::new();
        let (started, has_started) = mpsc::channel();
        let started = Mutex::new(started);
        let supervisor = fixture.supervisor(FakeBackend::new(move |_, home, _, stop| {
            home.write("saved", b"state").unwrap();
            started.lock().unwrap().send(()).unwrap();
            while !stop.is_stopped() {
                thread::sleep(Duration::from_millis(1));
            }
            RunOutcome::Stopped
        }));
        thread::scope(|scope| {
            let run = scope.spawn(|| supervisor.run(NAME, &message("x")));
            has_started.recv().unwrap();
            assert!(supervisor.shut_down(WAIT_LIMIT));
            // It has already ended by the time that returned.
            assert_eq!(*supervisor.active_runs(), 0);
            let record = run.join().unwrap().unwrap();
            assert_eq!(ending(&record), &RunOutcome::Stopped);
        });
        let storage = AgentStorage::open_existing(fixture.root.path(), NAME).unwrap();
        assert_eq!(
            storage.home(1024 * 1024).unwrap().read("saved").unwrap(),
            b"state"
        );
    }

    /// So that a shutdown that never returns fails the test, rather than hanging it.
    fn shut_down_on_its_own_thread(supervisor: &Arc<Supervisor>, wait: Duration) -> bool {
        let supervisor = Arc::clone(supervisor);
        let (result, finished) = mpsc::channel();
        thread::spawn(move || result.send(supervisor.shut_down(wait)));
        finished.recv_timeout(WAIT_LIMIT).unwrap()
    }

    #[test]
    fn shutting_down_stops_waiting_for_a_run_that_doesnt_end() {
        let fixture = Fixture::new();
        let (started, has_started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let supervisor = Arc::new(fixture.supervisor(held(started, released)));
        let running = Arc::clone(&supervisor);
        let run = thread::spawn(move || running.run(NAME, &message("x")));
        has_started.recv().unwrap();

        assert!(!shut_down_on_its_own_thread(
            &supervisor,
            Duration::from_millis(50)
        ));

        release.send(()).unwrap();
        run.join().unwrap().unwrap();
        assert!(supervisor.shut_down(WAIT_LIMIT));
    }

    #[test]
    fn a_run_doesnt_start_if_the_host_shuts_down_while_it_compiles() {
        let fixture = Fixture::new();
        let (compiling, is_compiling) = mpsc::channel::<()>();
        let (release, released) = mpsc::channel::<()>();
        let compiling = Mutex::new(compiling);
        let released = Mutex::new(released);
        let mut backend = FakeBackend::new(|_, _, _, _| panic!("this shouldn't run"));
        backend.before_load = Arc::new(move || {
            compiling.lock().unwrap().send(()).unwrap();
            let _ = released.lock().unwrap().recv_timeout(WAIT_LIMIT);
            Ok(())
        });
        let supervisor = Arc::new(fixture.supervisor(backend));
        let running = Arc::clone(&supervisor);
        let run = thread::spawn(move || running.run(NAME, &message("x")));
        is_compiling.recv_timeout(WAIT_LIMIT).unwrap();
        assert!(!shut_down_on_its_own_thread(&supervisor, Duration::ZERO));
        release.send(()).unwrap();
        let record = run.join().unwrap().unwrap();
        assert_eq!(
            record.ending,
            Some(RunEnding::NotStarted(
                "the host is shutting down".to_string()
            ))
        );
    }

    fn version(text: &str) -> Option<Version> {
        Some(Version::parse(text).unwrap())
    }

    fn other_manifest(name: &str, version: &str) -> String {
        format!(
            "[agent]\nname = \"{name}\"\nversion = \"{version}\"\n\n[limits]\nmemory_mb = 8\nrun_seconds = 7\ndisk_mb = 1\n"
        )
    }

    fn package(manifest: &str) -> Package {
        Package::new(manifest, COMPONENT.to_vec()).unwrap()
    }

    #[test]
    fn installing_makes_an_agent_that_can_run() {
        let root = TempDir::new().unwrap();
        let supervisor = Supervisor::new(root.path(), scripted(), HostLimits::default());
        assert_eq!(
            supervisor.run(NAME, &message("x")),
            Err(SupervisorError::NotInstalled)
        );
        supervisor.install(&package(MANIFEST)).unwrap();
        let record = supervisor.run(NAME, &message("x")).unwrap();
        assert_eq!(ending(&record), &RunOutcome::Completed);
    }

    #[test]
    fn installing_twice_is_refused_and_keeps_the_agents_files() {
        let fixture = Fixture::new();
        let supervisor = fixture.supervisor(scripted());
        supervisor.run(NAME, &message("write")).unwrap();
        assert_eq!(
            supervisor.install(&package(MANIFEST)),
            Err(SupervisorError::Package(PackageError::AlreadyInstalled))
        );
        let record = supervisor.run(NAME, &message("check")).unwrap();
        assert_eq!(ending(&record), &RunOutcome::Completed);
    }

    #[test]
    fn listing_shows_installed_agents_in_order() {
        let root = TempDir::new().unwrap();
        let supervisor = Supervisor::new(root.path(), scripted(), HostLimits::default());
        assert_eq!(supervisor.list(), Ok(Vec::new()));

        supervisor
            .install(&package(&other_manifest("zebra", "2.0.0")))
            .unwrap();
        supervisor
            .install(&package(&other_manifest("ant", "1.2.3")))
            .unwrap();
        supervisor.run("zebra", &message("x")).unwrap();
        let record = supervisor.run("zebra", &message("x")).unwrap();

        // None of these are agents.
        AgentStorage::open(root.path(), "unfinished-install").unwrap();
        fs::write(root.path().join("agents/a-file"), "x").unwrap();
        fs::create_dir(root.path().join("agents/Not-A-Name")).unwrap();

        assert_eq!(
            supervisor.list().unwrap(),
            vec![
                AgentSummary {
                    name: "ant".to_string(),
                    version: version("1.2.3"),
                    last_run: None,
                },
                AgentSummary {
                    name: "zebra".to_string(),
                    version: version("2.0.0"),
                    last_run: Some(record),
                },
            ]
        );
    }

    #[test]
    fn a_broken_agent_is_still_listed() {
        let fixture = Fixture::new();
        let manifest = fixture.agent_path().join("package/agent.toml");
        fs::remove_file(&manifest).unwrap();
        fs::write(&manifest, "not a manifest").unwrap();
        let agents = fixture.supervisor(scripted()).list().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, NAME);
        assert_eq!(agents[0].version, None);
    }

    #[test]
    fn removing_deletes_that_agent_and_nothing_else() {
        let fixture = Fixture::new();
        fixture.install("other", &other_manifest("other", "0.1.0"));
        let supervisor = fixture.supervisor(scripted());
        supervisor.run(NAME, &message("write")).unwrap();
        supervisor.run("other", &message("write")).unwrap();
        let beside = fixture.root.path().join("beside-agents.txt");
        fs::write(&beside, "keep").unwrap();

        supervisor.remove(NAME).unwrap();

        assert!(!fixture.agent_path().exists());
        assert_eq!(
            fs::read_dir(fixture.root.path().join("removed"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            supervisor.run(NAME, &message("x")),
            Err(SupervisorError::NotInstalled)
        );
        assert_eq!(fs::read_to_string(&beside).unwrap(), "keep");
        let other = supervisor.run("other", &message("check")).unwrap();
        assert_eq!(ending(&other), &RunOutcome::Completed);
    }

    #[test]
    fn a_new_agent_with_the_same_name_gets_nothing_from_the_old_one() {
        let fixture = Fixture::new();
        let backend = FakeBackend::new(|event, home, _, _| match text(event) {
            "write" => {
                home.write("saved", b"state").unwrap();
                RunOutcome::Completed
            }
            _ => {
                assert_eq!(home.read("saved"), Err(HomeError::NotFound));
                RunOutcome::Completed
            }
        });
        let supervisor = fixture.supervisor(backend);
        supervisor.run(NAME, &message("write")).unwrap();
        supervisor.remove(NAME).unwrap();
        supervisor.install(&package(MANIFEST)).unwrap();
        let record = supervisor.run(NAME, &message("check")).unwrap();
        assert_eq!(ending(&record), &RunOutcome::Completed);
        assert_eq!(supervisor.history(NAME).unwrap().runs, vec![record]);
    }

    #[test]
    fn an_agent_is_gone_in_one_step_even_if_deleting_its_files_fails() {
        let fixture = Fixture::new();
        let stuck = fixture.agent_path().join("home/stuck");
        fs::create_dir(&stuck).unwrap();
        fs::write(stuck.join("file"), "x").unwrap();
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o500)).unwrap();
        // The superuser can delete it anyway, and then there's nothing to see here.
        let can_delete_anyway = fs::write(stuck.join("probe"), "x").is_ok();

        let supervisor = fixture.supervisor(scripted());
        let result = supervisor.remove(NAME);
        assert!(!fixture.agent_path().exists());
        assert_eq!(
            supervisor.run(NAME, &message("x")),
            Err(SupervisorError::NotInstalled)
        );
        assert_eq!(supervisor.list(), Ok(Vec::new()));
        supervisor.install(&package(MANIFEST)).unwrap();
        let home = fixture.agent_path().join("home");
        assert_eq!(fs::read_dir(home).unwrap().count(), 0);

        if !can_delete_anyway {
            assert!(
                matches!(result, Err(SupervisorError::Storage(_))),
                "{result:?}"
            );
            let removed = fixture.root.path().join("removed");
            let left = fs::read_dir(&removed).unwrap().next().unwrap().unwrap();
            let stuck = left.path().join("home/stuck");
            fs::set_permissions(stuck, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn installing_is_refused_while_the_agent_runs() {
        let fixture = Fixture::new();
        let (started, has_started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let supervisor = fixture.supervisor(held(started, released));
        thread::scope(|scope| {
            let run = scope.spawn(|| supervisor.run(NAME, &message("x")));
            has_started.recv().unwrap();
            assert_eq!(
                supervisor.install(&package(MANIFEST)),
                Err(SupervisorError::AlreadyRunning)
            );
            release.send(()).unwrap();
            run.join().unwrap().unwrap();
        });
    }

    #[test]
    fn removing_doesnt_follow_symlinks_out_of_the_agents_folder() {
        let fixture = Fixture::new();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("file.txt");
        fs::write(&file, "keep").unwrap();
        let home = fixture.agent_path().join("home");
        symlink(outside.path(), home.join("linked-folder")).unwrap();
        symlink(&file, home.join("linked-file")).unwrap();

        fixture.supervisor(scripted()).remove(NAME).unwrap();

        assert!(!fixture.agent_path().exists());
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep");
    }

    #[test]
    fn a_symlink_in_place_of_an_agents_folder_is_removed_without_touching_its_target() {
        let fixture = Fixture::new();
        let outside = TempDir::new().unwrap();
        let file = outside.path().join("file.txt");
        fs::write(&file, "keep").unwrap();
        symlink(outside.path(), fixture.root.path().join("agents/linked")).unwrap();

        fixture.supervisor(scripted()).remove("linked").unwrap();

        assert!(!fixture.root.path().join("agents/linked").exists());
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep");
    }

    #[test]
    fn removing_is_refused_while_the_agent_runs() {
        let fixture = Fixture::new();
        let (started, has_started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let supervisor = fixture.supervisor(held(started, released));
        thread::scope(|scope| {
            let run = scope.spawn(|| supervisor.run(NAME, &message("x")));
            has_started.recv().unwrap();
            assert_eq!(
                supervisor.remove(NAME),
                Err(SupervisorError::AlreadyRunning)
            );
            assert!(fixture.agent_path().join("package").exists());
            release.send(()).unwrap();
            run.join().unwrap().unwrap();
        });
        supervisor.remove(NAME).unwrap();
    }

    #[test]
    fn what_a_crash_left_while_removing_is_cleared_and_doesnt_count_as_an_agent() {
        let fixture = Fixture::new();
        let left_behind = fixture.root.path().join("removed/agent-0123");
        fs::create_dir_all(left_behind.join("home")).unwrap();
        fs::write(left_behind.join("home/file"), "x").unwrap();
        let supervisor = fixture.supervisor(scripted());
        assert_eq!(supervisor.list().unwrap().len(), 1);

        supervisor.remove(NAME).unwrap();
        assert_eq!(
            fs::read_dir(fixture.root.path().join("removed"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(supervisor.list(), Ok(Vec::new()));
    }

    #[test]
    fn removing_refuses_bad_names_and_unknown_agents() {
        let fixture = Fixture::new();
        let supervisor = fixture.supervisor(scripted());
        for name in ["", ".", "..", "../agents", "agent/home", "/etc", "Agent"] {
            assert_eq!(
                supervisor.remove(name),
                Err(SupervisorError::InvalidName),
                "{name:?}"
            );
        }
        assert_eq!(
            supervisor.remove("nobody"),
            Err(SupervisorError::NotInstalled)
        );
        assert!(fixture.agent_path().exists());
    }

    #[test]
    fn errors_hold_no_host_paths() {
        let fixture = Fixture::new();
        let root = fixture.root.path().to_string_lossy().to_string();
        let record = RunRecord {
            id: RunId::parse(&"0".repeat(32)).unwrap(),
            event: EventKind::Scheduled,
            started_at_ms: 0,
            ended_at_ms: None,
            ending: None,
        };
        for error in [
            SupervisorError::InvalidName,
            SupervisorError::NotInstalled,
            SupervisorError::AlreadyRunning,
            SupervisorError::ShuttingDown,
            SupervisorError::NotRecorded(io::ErrorKind::PermissionDenied),
            SupervisorError::EndingNotSaved(Box::new(record)),
            SupervisorError::Package(PackageError::NotInstalled),
            SupervisorError::Storage(io::ErrorKind::PermissionDenied),
        ] {
            let text = error.to_string();
            assert!(!text.contains(&root) && !text.contains('/'), "{text}");
        }
    }
}
