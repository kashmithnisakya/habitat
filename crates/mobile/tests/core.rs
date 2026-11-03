//! Calls the core as the phone app does, which needs the `wasm32-unknown-unknown` target for the agent.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

use habitat_core::package::Package;
use habitat_mobile::{CoreError, HabitatCore, RunState};
use tempfile::TempDir;

const AGENT: &str = "hello-agent";

fn hello_package() -> &'static [u8] {
    static PACKAGE: OnceLock<Vec<u8>> = OnceLock::new();
    PACKAGE.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        // Its own folder, so that this can't wait on the build that's running these tests.
        let target_dir = root.join("target/test-agents");
        let output = Command::new(env!("CARGO"))
            .current_dir(&root)
            .args(["build", "-p", "habitat-hello-agent", "--release"])
            .args(["--target", "wasm32-unknown-unknown", "--target-dir"])
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
            fs::read(target_dir.join("wasm32-unknown-unknown/release/habitat_hello_agent.wasm"))
                .unwrap();
        let component = wit_component::ComponentEncoder::default()
            .module(&module)
            .unwrap()
            .validate(true)
            .encode()
            .unwrap();
        let manifest = fs::read_to_string(root.join("examples/hello-agent/agent.toml")).unwrap();
        let mut file = Vec::new();
        Package::new(&manifest, component)
            .unwrap()
            .write(&mut file)
            .unwrap();
        file
    })
}

struct Fixture {
    data: TempDir,
    core: Arc<HabitatCore>,
}

impl Fixture {
    fn new() -> Self {
        let data = TempDir::new().unwrap();
        let core = open(data.path());
        Self { data, core }
    }

    fn installed() -> Self {
        let fixture = Self::new();
        let path = fixture.write_file("hello.habitat", hello_package());
        fixture.core.install_package(path).unwrap();
        fixture
    }

    fn write_file(&self, name: &str, data: &[u8]) -> String {
        let path: PathBuf = self.data.path().join(name);
        fs::write(&path, data).unwrap();
        path.to_str().unwrap().to_string()
    }
}

fn open(path: &Path) -> Arc<HabitatCore> {
    HabitatCore::open(path.to_str().unwrap().to_string()).unwrap()
}

#[test]
fn installing_reports_the_agent_and_lists_it() {
    let fixture = Fixture::new();
    assert_eq!(fixture.core.list_agents().unwrap(), []);
    let path = fixture.write_file("hello.habitat", hello_package());
    let info = fixture.core.install_package(path).unwrap();
    assert_eq!(info.name, AGENT);
    assert_eq!(info.version.as_deref(), Some("0.1.0"));
    assert_eq!(info.memory_mb, Some(32));
    assert_eq!(info.run_seconds, Some(10));
    assert_eq!(info.disk_mb, Some(1));
    assert_eq!(info.problem, None);
    assert_eq!(fixture.core.list_agents().unwrap(), [info]);
}

#[test]
fn a_message_gets_the_agents_reply() {
    let fixture = Fixture::installed();
    let report = fixture
        .core
        .send_message(AGENT.to_string(), "hi".to_string())
        .unwrap();
    assert_eq!(report.state, RunState::Completed);
    assert_eq!(report.detail, None);
    assert_eq!(
        report.messages,
        ["You said: hi\nThat's message 1 since I was installed."]
    );
}

#[test]
fn an_agents_state_survives_reopening_the_core() {
    let fixture = Fixture::installed();
    fixture
        .core
        .send_message(AGENT.to_string(), "one".to_string())
        .unwrap();
    let reopened = open(fixture.data.path());
    let report = reopened
        .send_message(AGENT.to_string(), "two".to_string())
        .unwrap();
    assert!(
        report.messages[0].contains("message 2"),
        "{:?}",
        report.messages
    );
}

#[test]
fn runs_can_be_reviewed_afterwards() {
    let fixture = Fixture::installed();
    let report = fixture
        .core
        .send_message(AGENT.to_string(), "hi".to_string())
        .unwrap();
    let runs = fixture.core.recent_runs(AGENT.to_string(), 10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_id, report.run_id);
    assert_eq!(runs[0].state, RunState::Completed);
    assert!(runs[0].finished_ms.is_some());
}

#[test]
fn removing_an_agent_deletes_it() {
    let fixture = Fixture::installed();
    fixture.core.remove_agent(AGENT.to_string()).unwrap();
    assert_eq!(fixture.core.list_agents().unwrap(), []);
    assert_eq!(
        fixture
            .core
            .send_message(AGENT.to_string(), "hi".to_string()),
        Err(CoreError::NotFound)
    );
}

#[test]
fn a_bad_package_is_rejected_without_revealing_where_it_was() {
    let fixture = Fixture::new();
    let path = fixture.write_file("junk.habitat", b"not a package");
    let Err(error) = fixture.core.install_package(path) else {
        panic!("the package was installed");
    };
    assert!(matches!(error, CoreError::Rejected { .. }), "{error:?}");
    assert!(!error.to_string().contains("junk"), "{error}");
}

#[test]
fn a_missing_package_file_fails_without_revealing_its_path() {
    let fixture = Fixture::new();
    let missing = fixture
        .data
        .path()
        .join("secret-folder-name")
        .display()
        .to_string();
    let Err(error) = fixture.core.install_package(missing) else {
        panic!("the package was installed");
    };
    assert!(matches!(error, CoreError::Failed { .. }), "{error:?}");
    assert!(!error.to_string().contains("secret-folder-name"), "{error}");
}

#[test]
fn invalid_and_unknown_names_are_refused() {
    let fixture = Fixture::new();
    let send = |name: &str| fixture.core.send_message(name.to_string(), "x".to_string());
    assert_eq!(send("missing"), Err(CoreError::NotFound));
    assert!(matches!(send("../escape"), Err(CoreError::Rejected { .. })));
}
