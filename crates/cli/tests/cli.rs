//! Drives the real command line, which needs the `wasm32-unknown-unknown` target for the sample agent.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

use tempfile::TempDir;

const MANIFEST: &str = "../../examples/hello-agent/agent.toml";

fn hello_agent_code() -> &'static Path {
    static CODE: OnceLock<PathBuf> = OnceLock::new();
    CODE.get_or_init(|| {
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
        target_dir.join("wasm32-unknown-unknown/release/habitat_hello_agent.wasm")
    })
}

struct Fixture {
    data: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            data: TempDir::new().unwrap(),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_habitat"));
        command.args(args).env("HABITAT_DATA", self.data.path());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn package(&self) -> PathBuf {
        let path = self.data.path().join("hello.habitat");
        let code = hello_agent_code().to_str().unwrap();
        self.ok(&["pack", MANIFEST, code, path.to_str().unwrap()]);
        path
    }

    fn install(&self) {
        let package = self.package();
        self.ok(&["install", package.to_str().unwrap()]);
    }

    fn agent_folder(&self) -> PathBuf {
        self.data.path().join("agents/hello-agent")
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_new_user_can_pack_install_and_talk_to_an_agent() {
    let fixture = Fixture::new();
    assert_eq!(fixture.ok(&["list"]), "No agents installed.\n");
    fixture.install();
    assert_eq!(fixture.ok(&["list"]), "hello-agent 0.1.0\n");
    assert_eq!(
        fixture.ok(&["run", "hello-agent", "good", "morning"]),
        "[hello-agent] You said: good morning\n\
         [hello-agent] That's message 1 since I was installed.\n"
    );
}

#[test]
fn an_agent_keeps_its_files_between_runs_of_the_program() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.ok(&["run", "hello-agent", "one"]);
    let second = fixture.ok(&["run", "hello-agent", "two"]);
    assert!(second.contains("message 2"), "{second}");
}

#[test]
fn runs_are_listed() {
    let fixture = Fixture::new();
    fixture.install();
    assert_eq!(fixture.ok(&["runs", "hello-agent"]), "No runs yet.\n");
    fixture.ok(&["run", "hello-agent", "hello"]);
    let runs = fixture.ok(&["runs", "hello-agent"]);
    assert!(runs.contains("completed"), "{runs}");
    assert_eq!(runs.lines().count(), 1);
}

#[test]
fn remove_asks_first_and_only_a_clear_yes_deletes() {
    let fixture = Fixture::new();
    fixture.install();
    for answer in ["no\n", "y\n", ""] {
        let mut child = fixture
            .command(&["remove", "hello-agent"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(answer.as_bytes())
            .unwrap();
        assert!(
            !child.wait_with_output().unwrap().status.success(),
            "{answer:?}"
        );
        assert!(fixture.agent_folder().exists(), "{answer:?}");
    }

    let mut child = fixture
        .command(&["remove", "hello-agent"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"yes\n").unwrap();
    assert!(child.wait_with_output().unwrap().status.success());
    assert!(!fixture.agent_folder().exists());
}

#[test]
fn remove_with_yes_deletes_only_that_agents_data() {
    let fixture = Fixture::new();
    fixture.install();
    let keep = fixture.data.path().join("agents/other");
    fs::create_dir_all(&keep).unwrap();
    fixture.ok(&["remove", "hello-agent", "--yes"]);
    assert!(!fixture.agent_folder().exists());
    assert!(keep.exists());
    assert_eq!(fixture.ok(&["list"]), "No agents installed.\n");
}

#[test]
fn installing_something_that_isnt_a_package_fails_clearly() {
    let fixture = Fixture::new();
    let path = fixture.data.path().join("junk");
    fs::write(&path, "not a package").unwrap();
    let output = fixture.run(&["install", path.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("invalid package"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fixture.ok(&["list"]), "No agents installed.\n");
}

#[test]
fn installing_twice_is_refused() {
    let fixture = Fixture::new();
    let package = fixture.package();
    let path = package.to_str().unwrap();
    fixture.ok(&["install", path]);
    let output = fixture.run(&["install", path]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("already installed"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_unknown_agent_fails_clearly_and_creates_nothing() {
    let fixture = Fixture::new();
    for args in [
        &["run", "missing", "hello"][..],
        &["runs", "missing"],
        &["remove", "missing", "--yes"],
    ] {
        let output = fixture.run(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(stderr(&output).contains("not found"), "{}", stderr(&output));
    }
    assert!(!fixture.data.path().join("agents").exists());
}

#[test]
fn a_bad_command_shows_the_usage() {
    let fixture = Fixture::new();
    for args in [&[][..], &["nonsense"], &["run", "only-a-name"]] {
        let output = fixture.run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("Usage:"), "{args:?}");
    }
    assert!(fixture.ok(&["help"]).contains("Usage:"));
}
