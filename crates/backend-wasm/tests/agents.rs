//! Runs real components through the backend. These need the `wasm32-unknown-unknown` target,
//! because they build the sample agent in `examples/file-agent`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use habitat_backend_wasm::WasmBackend;
use habitat_core::backend::{Backend, Event, LoadError, LoadedAgent, RunOutcome};
use habitat_core::home::{AgentStorage, Home};
use habitat_core::package::Package;
use tempfile::TempDir;

const MANIFEST: &str = "[agent]\nname = \"file-agent\"\nversion = \"0.1.0\"\n\n[limits]\nmemory_mb = 64\nrun_seconds = 10\ndisk_mb = 1\n";
const DISK_LIMIT: u64 = 16 * 1024 * 1024;

// A component with no imports. `body` is what handle-event does, and `extra` is anything else in
// its core module.
fn component_exporting(body: &str, extra: &str) -> Vec<u8> {
    wat::parse_str(format!(
        r#"(component
            (core module $m
                (memory (export "memory") 1)
                (func (export "handle-event") (param i32 i32 i32) (result i32) {body})
                (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) i32.const 1024)
                {extra}
            )
            (core instance $i (instantiate $m))
            (alias core export $i "memory" (core memory $memory))
            (type $event (variant (case "user-message" string) (case "scheduled" string)))
            (export $exported "event" (type $event))
            (func (export "handle-event") (param "event" $exported) (result (result (error string)))
                (canon lift (core func $i "handle-event")
                    (memory $memory)
                    (realloc (core func $i "cabi_realloc"))
                    string-encoding=utf8))
        )"#
    ))
    .unwrap()
}

fn file_agent_code() -> &'static [u8] {
    static CODE: OnceLock<Vec<u8>> = OnceLock::new();
    CODE.get_or_init(|| {
        // For a test run on a device, where the agent was built beforehand.
        if let Ok(path) = env::var("HABITAT_TEST_AGENT") {
            return fs::read(path).unwrap();
        }

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        // Its own folder, so that this can't wait on the build that's running these tests.
        let target_dir: PathBuf = root.join("target/test-agents");
        let output = Command::new(env::var("CARGO").unwrap_or("cargo".to_string()))
            .current_dir(&root)
            .args([
                "build",
                "-p",
                "habitat-file-agent",
                "--release",
                "--target",
                "wasm32-unknown-unknown",
                "--target-dir",
            ])
            .arg(&target_dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "couldn't build the sample agent. If the target is missing, run \
             `rustup target add wasm32-unknown-unknown`.\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let module =
            fs::read(target_dir.join("wasm32-unknown-unknown/release/habitat_file_agent.wasm"))
                .unwrap();
        wit_component::ComponentEncoder::default()
            .module(&module)
            .unwrap()
            .validate(true)
            .encode()
            .unwrap()
    })
}

struct Fixture {
    _root: TempDir,
    storage: AgentStorage,
    agent: Box<dyn LoadedAgent>,
}

impl Fixture {
    fn new(code: Vec<u8>) -> Self {
        let root = TempDir::new().unwrap();
        let storage = AgentStorage::open(root.path(), "file-agent").unwrap();
        let agent = load(code).unwrap();
        Self {
            _root: root,
            storage,
            agent,
        }
    }

    fn file_agent() -> Self {
        Self::new(file_agent_code().to_vec())
    }

    fn home(&self) -> Home {
        self.storage.home(DISK_LIMIT).unwrap()
    }

    fn home_path(&self) -> PathBuf {
        self.storage.package_path().with_file_name("home")
    }

    fn run(&self, message: &str) -> RunOutcome {
        self.agent
            .run(&Event::UserMessage(message.to_string()), self.home())
    }

    /// Runs a command that has to succeed.
    fn ok(&self, message: &str) {
        assert_eq!(self.run(message), RunOutcome::Completed, "{message}");
    }
}

fn load(code: Vec<u8>) -> Result<Box<dyn LoadedAgent>, LoadError> {
    let package = Package::new(MANIFEST, code).unwrap();
    WasmBackend::new().unwrap().load(&package)
}

fn load_error(code: Vec<u8>) -> LoadError {
    match load(code) {
        Ok(_) => panic!("the component loaded"),
        Err(error) => error,
    }
}

#[test]
fn an_agent_reads_and_writes_files_in_its_home() {
    let fixture = Fixture::file_agent();
    fixture.ok("write notes/today.txt hello there");
    fixture.ok("expect notes/today.txt hello there");

    // Really in the home, as the home store sees it.
    assert_eq!(
        fixture.home().read("notes/today.txt").unwrap(),
        b"hello there"
    );

    fixture.ok("write notes/today.txt changed");
    fixture.ok("expect notes/today.txt changed");
    fixture.ok("write top.txt 12345");
    fixture.ok("expect-list . notes/ top.txt:5");
    fixture.ok("expect-list notes today.txt:7");
    fixture.ok("remove notes/today.txt");
    fixture.ok("expect-list . top.txt:5");
    fixture.ok("expect-error notes/today.txt not-found");
}

#[test]
fn files_persist_between_runs_but_memory_does_not() {
    let fixture = Fixture::file_agent();
    fixture.ok("write state.txt saved");
    fixture.ok("remember");
    fixture.ok("expect-forgotten");
    fixture.ok("expect state.txt saved");
}

#[test]
fn the_agent_is_told_what_kind_of_event_it_is() {
    let fixture = Fixture::file_agent();
    let run = |event: Event| fixture.agent.run(&event, fixture.home());
    let scheduled = |name: &str| Event::Scheduled(name.to_string());
    assert_eq!(run(scheduled("expect-scheduled")), RunOutcome::Completed);
    assert_ne!(run(scheduled("expect-user-message")), RunOutcome::Completed);
    fixture.ok("expect-user-message");
    assert_ne!(fixture.run("expect-scheduled"), RunOutcome::Completed);
}

#[test]
fn large_files_are_read_in_pieces() {
    let fixture = Fixture::file_agent();
    fixture.ok(&format!("expect-big big.bin {}", 3 * 1024 * 1024));
}

#[test]
fn an_agent_cant_leave_its_home() {
    let fixture = Fixture::file_agent();
    let outside = fixture.home_path().with_file_name("grants.db");
    fs::write(&outside, "secret").unwrap();
    std::os::unix::fs::symlink(&outside, fixture.home_path().join("link")).unwrap();

    fixture.ok("expect-error ../grants.db invalid-request");
    fixture.ok("expect-error ../../../../../etc/passwd invalid-request");
    fixture.ok("expect-error /etc/passwd invalid-request");
    fixture.ok("expect-error link denied");

    let RunOutcome::Failed(_) = fixture.run("write ../grants.db changed") else {
        panic!("the write wasn't refused");
    };
    let RunOutcome::Failed(_) = fixture.run("write link changed") else {
        panic!("the write through a symlink wasn't refused");
    };
    assert_eq!(fs::read_to_string(&outside).unwrap(), "secret");
}

#[test]
fn the_disk_limit_applies_to_agents() {
    let fixture = Fixture::file_agent();
    let small_home = fixture.storage.home(100).unwrap();
    let outcome = fixture.agent.run(
        &Event::UserMessage("write a.txt hello".to_string()),
        small_home,
    );
    let RunOutcome::Failed(message) = outcome else {
        panic!("the write wasn't refused: {outcome:?}");
    };
    assert!(message.contains("disk limit"), "{message}");
}

#[test]
fn calls_that_need_the_broker_are_denied() {
    Fixture::file_agent().ok("expect-denied-outside");
}

#[test]
fn agents_get_a_clock_and_random_bytes() {
    Fixture::file_agent().ok("check-clock-and-random");
}

#[test]
fn a_failure_reported_by_an_agent_is_returned_safely() {
    let fixture = Fixture::file_agent();
    assert_eq!(
        fixture.run("fail - it went wrong"),
        RunOutcome::Failed("it went wrong".to_string())
    );

    // The message is the agent's own, so it can't carry terminal escape codes.
    let RunOutcome::Failed(message) = fixture.run("fail - \x1b[31mfake\u{202e}") else {
        panic!("the agent didn't fail");
    };
    assert!(message.is_ascii());
    assert!(!message.chars().any(|c| c.is_control()), "{message}");
}

#[test]
fn a_panicking_agent_is_reported_and_the_next_run_still_works() {
    let fixture = Fixture::file_agent();
    fixture.ok("write state.txt saved");

    let RunOutcome::Crashed(reason) = fixture.run("panic") else {
        panic!("the crash wasn't reported");
    };
    assert!(reason.contains("unreachable"), "{reason}");

    fixture.ok("expect state.txt saved");
}

#[test]
fn traps_are_reported_without_crashing_the_host() {
    for (body, expected) in [
        ("unreachable", "unreachable"),
        ("i32.const 0 i32.const 0 i32.div_s", "divide by zero"),
        ("i32.const 1000000 i32.load", "out of bounds"),
    ] {
        let fixture = Fixture::new(component_exporting(body, ""));
        let RunOutcome::Crashed(reason) = fixture.run("anything") else {
            panic!("{body} wasn't reported");
        };
        assert!(reason.contains(expected), "{body}: {reason}");
    }
}

#[test]
fn a_crash_reports_nothing_written_by_the_agent() {
    // A function name that would be a terminal escape code, if it reached the error.
    let code = component_exporting(
        "unreachable",
        r#"(@custom "name" "\01\08\01\00\05\1b[31m")"#,
    );
    let fixture = Fixture::new(code);
    let RunOutcome::Crashed(reason) = fixture.run("anything") else {
        panic!("the crash wasn't reported");
    };
    assert!(!reason.contains('\x1b'), "{reason:?}");
    assert!(reason.is_ascii(), "{reason:?}");
}

#[test]
fn a_trap_while_starting_is_reported() {
    let code = component_exporting("i32.const 0", "(func $start unreachable) (start $start)");

    // Loading runs none of the agent's code, so this only fails once it runs.
    let fixture = Fixture::new(code);
    let RunOutcome::Crashed(reason) = fixture.run("anything") else {
        panic!("the crash wasn't reported");
    };
    assert!(reason.contains("unreachable"), "{reason}");
}

#[test]
fn imports_outside_the_api_fail_to_load_with_a_clear_error() {
    for (import, name) in [
        (
            r#"(import "wasi:sockets/tcp@0.2.0" (instance))"#,
            "wasi:sockets/tcp@0.2.0",
        ),
        (
            r#"(import "wasi:filesystem/types@0.2.0" (instance))"#,
            "wasi:filesystem/types@0.2.0",
        ),
        (
            r#"(import "wasi:http/outgoing-handler@0.2.0" (instance))"#,
            "wasi:http/outgoing-handler@0.2.0",
        ),
        (r#"(import "open-socket" (func))"#, "open-socket"),
        // Looks like the API, but isn't.
        (
            r#"(import "habitat:hostile/agent-home@0.1.0" (instance))"#,
            "habitat:hostile/agent-home@0.1.0",
        ),
        (
            r#"(import "habitat:host/secrets@0.1.0" (instance))"#,
            "habitat:host/secrets@0.1.0",
        ),
    ] {
        let code = wat::parse_str(format!("(component {import})")).unwrap();
        let error = load_error(code);
        assert_eq!(
            error,
            LoadError::ForbiddenImport(name.to_string()),
            "{import}"
        );
        assert!(error.to_string().contains(name));
    }
}

#[test]
fn a_function_that_the_api_lacks_fails_to_load() {
    let code = wat::parse_str(
        r#"(component
            (import "habitat:host/agent-home@0.1.0" (instance
                (export "open-socket" (func))
            ))
        )"#,
    )
    .unwrap();
    let error = load_error(code);
    assert!(matches!(error, LoadError::Incompatible(_)), "{error:?}");
    assert!(error.to_string().contains("open-socket"), "{error}");
}

#[test]
fn an_api_version_this_host_lacks_fails_to_load() {
    let code = wat::parse_str(r#"(component (import "habitat:host/agent-home@0.2.0" (instance)))"#)
        .unwrap();
    let error = load_error(code);
    assert!(matches!(error, LoadError::Incompatible(_)), "{error:?}");
}

#[test]
fn a_component_without_handle_event_fails_to_load() {
    let error = load_error(wat::parse_str("(component)").unwrap());
    assert!(matches!(error, LoadError::Incompatible(_)), "{error:?}");
}

#[test]
fn a_handle_event_with_the_wrong_type_fails_to_load() {
    let code = wat::parse_str(
        r#"(component
            (core module $m (func (export "handle-event")))
            (core instance $i (instantiate $m))
            (func (export "handle-event") (canon lift (core func $i "handle-event")))
        )"#,
    )
    .unwrap();
    let error = load_error(code);
    assert!(matches!(error, LoadError::Incompatible(_)), "{error:?}");
}

#[test]
fn malformed_code_fails_to_load() {
    // The right header, so that it passes the package's quick check.
    let mut code = b"\0asm\x0d\0\x01\0".to_vec();
    code.extend_from_slice(&[0xff; 64]);
    let error = load_error(code);
    assert!(matches!(error, LoadError::InvalidCode(_)), "{error:?}");
    assert!(error.to_string().is_ascii());
}

#[test]
fn a_hostile_import_name_is_made_safe_to_print() {
    // The name holds a terminal escape code, which the component parser refuses.
    let code = wat::parse_str(r#"(component (import "\1b[31m" (func)))"#).unwrap();
    let message = load_error(code).to_string();
    assert!(message.is_ascii(), "{message}");
    assert!(!message.chars().any(|c| c.is_control()), "{message}");
}
