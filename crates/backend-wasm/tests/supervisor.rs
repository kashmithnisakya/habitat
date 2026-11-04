//! Runs real agents through the supervisor, including what happens when they fail, when the host
//! shuts down and when the host process is killed. These need the `wasm32-unknown-unknown` target,
//! because they build the sample agent in `examples/file-agent`.

mod support;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use habitat_backend_wasm::WasmBackend;
use habitat_core::backend::{Event, HostLimits, RunOutcome};
use habitat_core::home::AgentStorage;
use habitat_core::package::Package;
use habitat_core::run_log::{RunEnding, RunRecord};
use habitat_core::supervisor::{Supervisor, SupervisorError};
use tempfile::TempDir;

use support::file_agent_code;

const NAME: &str = "file-agent";
const WAIT_LIMIT: Duration = Duration::from_secs(120);
const KILLED_HOST_ROOT: &str = "HABITAT_TEST_KILLED_HOST_ROOT";

fn manifest(memory_mb: u32, run_seconds: u32) -> String {
    format!(
        "[agent]\nname = \"{NAME}\"\nversion = \"0.1.0\"\n\n[limits]\nmemory_mb = {memory_mb}\nrun_seconds = {run_seconds}\ndisk_mb = 1\n"
    )
}

fn supervisor(root: &Path) -> Supervisor {
    Supervisor::new(root, WasmBackend::new().unwrap(), HostLimits::default())
}

fn install(root: &Path, memory_mb: u32, run_seconds: u32) {
    install_code(root, memory_mb, run_seconds, file_agent_code().to_vec());
}

fn install_code(root: &Path, memory_mb: u32, run_seconds: u32, code: Vec<u8>) {
    let storage = AgentStorage::open(root, NAME).unwrap();
    Package::new(&manifest(memory_mb, run_seconds), code)
        .unwrap()
        .install(&storage)
        .unwrap();
}

fn installed() -> TempDir {
    let root = TempDir::new().unwrap();
    install(root.path(), 64, 60);
    root
}

fn message(text: &str) -> Event {
    Event::UserMessage(text.to_string())
}

fn outcome(record: &RunRecord) -> &RunOutcome {
    match &record.ending {
        Some(RunEnding::Ran(outcome)) => outcome,
        other => panic!("{other:?}"),
    }
}

fn agent_path(root: &Path, inside: &str) -> PathBuf {
    root.join("agents").join(NAME).join(inside)
}

fn record_count(root: &Path) -> usize {
    fs::read_dir(agent_path(root, "runs")).unwrap().count()
}

fn assert_completes(supervisor: &Supervisor, text: &str) {
    let record = supervisor.run(NAME, &message(text)).unwrap();
    assert_eq!(outcome(&record), &RunOutcome::Completed, "{text}");
}

/// Waits for something to become true, and fails rather than hangs if it never does.
fn wait_until(what: &str, done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < WAIT_LIMIT, "gave up waiting: {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn an_agent_goes_through_a_whole_run_and_its_state_survives_a_restart() {
    let root = installed();
    let record = supervisor(root.path())
        .run(NAME, &message("write notes/today.txt hello"))
        .unwrap();
    assert_eq!(outcome(&record), &RunOutcome::Completed);

    // Nothing is shared with the first run except the files on disk.
    let restarted = supervisor(root.path());
    assert_completes(&restarted, "expect notes/today.txt hello");
    assert_completes(&restarted, "expect-forgotten");

    let history = restarted.history(NAME).unwrap();
    assert_eq!(history.runs.len(), 3);
    assert_eq!(history.runs[0], record);
    assert_eq!(history.unreadable, 0);
}

#[test]
fn nothing_in_memory_carries_over_between_runs() {
    let root = installed();
    let supervisor = supervisor(root.path());
    assert_completes(&supervisor, "remember");
    assert_completes(&supervisor, "expect-forgotten");
}

#[test]
fn every_way_of_failing_keeps_the_saved_state_and_lets_the_next_run_start() {
    let root = TempDir::new().unwrap();
    install(root.path(), 16, 1);
    let supervisor = supervisor(root.path());
    assert_completes(&supervisor, "write saved.txt state");

    let record = supervisor.run(NAME, &message("fail - no")).unwrap();
    assert_eq!(outcome(&record), &RunOutcome::Failed("no".to_string()));
    assert_completes(&supervisor, "expect saved.txt state");

    let record = supervisor.run(NAME, &message("panic")).unwrap();
    assert!(matches!(outcome(&record), RunOutcome::Crashed(_)));
    assert_completes(&supervisor, "expect saved.txt state");

    let record = supervisor.run(NAME, &message("loop-forever")).unwrap();
    assert_eq!(outcome(&record), &RunOutcome::TimeLimit);
    assert_completes(&supervisor, "expect saved.txt state");

    let record = supervisor.run(NAME, &message("allocate-forever")).unwrap();
    assert_eq!(outcome(&record), &RunOutcome::MemoryLimit);
    assert_completes(&supervisor, "expect saved.txt state");

    let history = supervisor.history(NAME).unwrap();
    assert_eq!(history.runs.len(), 9);
    assert_eq!(history.unreadable, 0);
}

#[test]
fn what_a_failed_run_saved_stays_saved() {
    let root = TempDir::new().unwrap();
    install(root.path(), 16, 1);
    let supervisor = supervisor(root.path());
    let record = supervisor
        .run(NAME, &message("write-then-loop saved.txt state"))
        .unwrap();
    assert_eq!(outcome(&record), &RunOutcome::TimeLimit);
    assert_completes(&supervisor, "expect saved.txt state");
}

#[test]
fn a_package_that_wont_load_is_recorded_and_doesnt_stop_later_attempts() {
    let root = TempDir::new().unwrap();
    install_code(root.path(), 16, 1, b"\0asm\x0d\0\x01\0 not really".to_vec());
    let supervisor = supervisor(root.path());
    for _ in 0..2 {
        let record = supervisor.run(NAME, &message("x")).unwrap();
        let Some(RunEnding::NotStarted(reason)) = &record.ending else {
            panic!("{record:?}");
        };
        assert!(reason.starts_with("invalid agent code"), "{reason}");
    }
    assert_eq!(record_count(root.path()), 2);
}

#[test]
fn a_package_too_costly_to_compile_is_recorded() {
    let root = TempDir::new().unwrap();
    let nested = wat::parse_str("(component (component))").unwrap();
    install_code(root.path(), 16, 1, nested);
    let record = supervisor(root.path()).run(NAME, &message("x")).unwrap();
    let Some(RunEnding::NotStarted(reason)) = &record.ending else {
        panic!("{record:?}");
    };
    assert!(reason.contains("too costly"), "{reason}");
}

#[test]
fn a_package_changed_after_installing_doesnt_run() {
    let root = installed();
    let supervisor = supervisor(root.path());
    assert_completes(&supervisor, "write saved.txt state");

    fs::write(agent_path(root.path(), "package.sha256"), "0\n").unwrap();
    let record = supervisor
        .run(NAME, &message("write saved.txt changed"))
        .unwrap();
    assert!(matches!(record.ending, Some(RunEnding::NotStarted(_))));
    assert_eq!(
        fs::read_to_string(agent_path(root.path(), "home/saved.txt")).unwrap(),
        "state"
    );
}

#[test]
fn shutting_down_stops_an_agent_that_wont_stop_by_itself() {
    for command in ["write-then-loop", "write-then-call-host"] {
        let root = installed();
        let supervisor = supervisor(root.path());
        let saved = agent_path(root.path(), "home/saved.txt");
        thread::scope(|scope| {
            let run = scope.spawn(|| {
                let text = format!("{command} saved.txt state");
                supervisor.run(NAME, &message(&text))
            });
            wait_until("the agent to save its file", || saved.exists());

            let start = Instant::now();
            assert!(supervisor.shut_down(WAIT_LIMIT), "{command}");
            assert!(start.elapsed() < Duration::from_secs(10), "{command}");

            let record = run.join().unwrap().unwrap();
            assert_eq!(outcome(&record), &RunOutcome::Stopped, "{command}");
        });
        assert_eq!(
            supervisor.run(NAME, &message("x")),
            Err(SupervisorError::ShuttingDown)
        );

        // The next time the host starts, everything is as the agent saved it.
        let restarted = self::supervisor(root.path());
        assert_completes(&restarted, "expect saved.txt state");
    }
}

#[test]
fn only_one_run_of_an_agent_at_a_time() {
    let root = installed();
    let supervisor = supervisor(root.path());
    let saved = agent_path(root.path(), "home/saved.txt");
    thread::scope(|scope| {
        let run = scope.spawn(|| supervisor.run(NAME, &message("write-then-loop saved.txt x")));
        wait_until("the agent to save its file", || saved.exists());
        assert_eq!(
            supervisor.run(NAME, &message("expect saved.txt x")),
            Err(SupervisorError::AlreadyRunning)
        );
        assert!(supervisor.shut_down(WAIT_LIMIT));
        run.join().unwrap().unwrap();
    });
    // The refused run left no record.
    assert_eq!(record_count(root.path()), 1);
}

/// Stops the host process if a test ends early, so that it can't be left running.
struct HostProcess(Child);

impl Drop for HostProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "run by killing_the_host_during_a_run_loses_nothing_that_was_saved"]
fn killed_host() {
    let root = env::var(KILLED_HOST_ROOT).expect("only for the test that kills it");
    let supervisor = supervisor(Path::new(&root));
    assert_completes(&supervisor, "write saved.txt state");
    let _ = supervisor.run(NAME, &message("write-then-loop running.txt x"));
}

#[test]
fn killing_the_host_during_a_run_loses_nothing_that_was_saved() {
    let root = installed();
    let running = agent_path(root.path(), "home/running.txt");

    let mut host = HostProcess(
        Command::new(env::current_exe().unwrap())
            .args(["--exact", "killed_host", "--ignored"])
            .env(KILLED_HOST_ROOT, root.path())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("the agent in the other process to start", || {
        running.exists()
    });

    // The lock holds across processes.
    let supervisor = supervisor(root.path());
    assert_eq!(
        supervisor.run(NAME, &message("x")),
        Err(SupervisorError::AlreadyRunning)
    );
    let history = supervisor.history(NAME).unwrap();
    assert_eq!(history.runs.len(), 2);
    assert_eq!(history.runs[1].ending, None);

    host.0.kill().unwrap();
    host.0.wait().unwrap();

    // The lock went with the process, and the run shows as what it was.
    let history = supervisor.history(NAME).unwrap();
    assert_eq!(history.runs.len(), 2);
    assert_eq!(outcome(&history.runs[0]), &RunOutcome::Completed);
    assert_eq!(history.runs[1].ending, Some(RunEnding::Interrupted));
    assert_eq!(history.runs[1].ended_at_ms, None);
    let killed_id = history.runs[1].id;

    assert_completes(&supervisor, "expect saved.txt state");
    assert_completes(&supervisor, "expect running.txt x");
    let history = supervisor.history(NAME).unwrap();
    assert_eq!(history.runs.len(), 4);
    assert_eq!(history.runs[1].id, killed_id);
    assert_eq!(history.runs[1].ending, Some(RunEnding::Interrupted));
}

#[test]
fn an_agent_cant_reach_its_records_or_its_lock() {
    let root = installed();
    let supervisor = supervisor(root.path());
    let first = supervisor.run(NAME, &message("write a.txt b")).unwrap();
    let record_path = format!("../runs/{}.toml", first.id);
    let record_file = agent_path(root.path(), &format!("runs/{}.toml", first.id));
    let before = fs::read(&record_file).unwrap();

    for path in [
        record_path.as_str(),
        "../../../locks/file-agent.lock",
        "../runs/new.toml",
    ] {
        assert_completes(&supervisor, &format!("expect-error {path} invalid-request"));
        let record = supervisor
            .run(NAME, &message(&format!("write {path} changed")))
            .unwrap();
        assert!(matches!(outcome(&record), RunOutcome::Failed(_)), "{path}");
    }
    assert_completes(&supervisor, "expect-list . a.txt:1");
    assert_eq!(fs::read(&record_file).unwrap(), before);
    assert!(!agent_path(root.path(), "runs/new.toml").exists());
}

#[test]
fn an_agent_cant_write_what_it_likes_into_its_record() {
    let root = installed();
    let supervisor = supervisor(root.path());
    let hostile = format!(
        "fail - {}\nending = \"completed\"\n\x1b[31m\u{202e}",
        "x".repeat(10 * 1024 * 1024)
    );
    let record = supervisor.run(NAME, &message(&hostile)).unwrap();
    let RunOutcome::Failed(reason) = outcome(&record) else {
        panic!("{record:?}");
    };
    assert!(reason.is_ascii() && !reason.chars().any(char::is_control));

    // It reads back as what it was, and takes up little space however long the message was.
    let file = agent_path(root.path(), &format!("runs/{}.toml", record.id));
    assert!(fs::metadata(file).unwrap().len() < 4096);
    let history = supervisor.history(NAME).unwrap();
    assert_eq!(history.runs, vec![record]);
    assert_eq!(history.unreadable, 0);
}
