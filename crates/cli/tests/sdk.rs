//! Builds real agents with the SDK, and runs them through the real `habitat` program. These need
//! the `wasm32-unknown-unknown` target.

#[path = "../../backend-wasm/tests/support/mod.rs"]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

use tempfile::TempDir;

use support::build_agent;

fn hello_code() -> &'static [u8] {
    static CODE: OnceLock<Vec<u8>> = OnceLock::new();
    CODE.get_or_init(|| build_agent("habitat-hello"))
}

fn check_code() -> &'static [u8] {
    static CODE: OnceLock<Vec<u8>> = OnceLock::new();
    CODE.get_or_init(|| build_agent("habitat-sdk-check"))
}

fn habitat(folder: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_habitat"));
    command
        .current_dir(folder)
        .env("HABITAT_HOME", folder.join("root"))
        // So that even a bug can't reach the real folder of whoever runs these tests.
        .env("HOME", folder.join("no-such-home"))
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::null());
    command
}

fn run(folder: &Path, args: &[&str]) -> Output {
    habitat(folder).args(args).output().unwrap()
}

#[track_caller]
fn succeeds(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Makes a folder, and installs an agent into it, with its manifest from `manifest`.
fn installed(manifest: &str, code: &[u8]) -> TempDir {
    let folder = TempDir::new().unwrap();
    fs::write(folder.path().join("agent.toml"), manifest).unwrap();
    fs::write(folder.path().join("agent.wasm"), code).unwrap();
    succeeds(&run(
        folder.path(),
        &["pack", "agent.toml", "agent.wasm", "-o", "agent.habitat"],
    ));
    succeeds(&run(folder.path(), &["install", "agent.habitat"]));
    folder
}

fn example_manifest() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::read_to_string(root.join("examples/hello/agent.toml")).unwrap()
}

fn saved_visits(folder: &Path) -> PathBuf {
    folder.join("root/agents/hello/home/state/visits.json")
}

const CHECK_MANIFEST: &str = "[agent]\nname = \"check\"\nversion = \"0.1.0\"\n\n[limits]\nmemory_mb = 256\nrun_seconds = 60\ndisk_mb = 16\n";

fn check(folder: &Path, message: &str) {
    succeeds(&run(folder, &["run", "check", message]));
}

/// Runs a command that has to fail, and returns what it said.
fn check_fails(folder: &Path, message: &str) -> String {
    let output = run(folder, &["run", "check", message]);
    assert_eq!(output.status.code(), Some(3), "{message}");
    String::from_utf8(output.stderr).unwrap()
}

#[test]
fn the_hello_agent_remembers_between_runs_and_restarts() {
    let folder = installed(&example_manifest(), hello_code());
    let folder = folder.path();

    let first = run(folder, &["run", "hello", "Ada"]);
    succeeds(&first);
    assert!(String::from_utf8_lossy(&first.stdout).starts_with("completed"));
    assert_eq!(
        fs::read_to_string(saved_visits(folder)).unwrap(),
        r#"{"count":1,"last_visitor":"Ada"}"#
    );

    succeeds(&run(folder, &["run", "hello", "Grace"]));
    assert_eq!(
        fs::read_to_string(saved_visits(folder)).unwrap(),
        r#"{"count":2,"last_visitor":"Grace"}"#
    );
}

#[test]
fn the_tutorials_failing_run_goes_as_it_says_and_loses_nothing() {
    let folder = installed(&example_manifest(), hello_code());
    let folder = folder.path();
    succeeds(&run(folder, &["run", "hello", "Ada"]));

    let failed = run(folder, &["run", "hello", " "]);
    assert_eq!(failed.status.code(), Some(3));
    let said = String::from_utf8(failed.stderr).unwrap();
    assert!(
        said.starts_with("error: failed, and the agent said: tell me who's visiting"),
        "{said}"
    );
    assert_eq!(
        fs::read_to_string(saved_visits(folder)).unwrap(),
        r#"{"count":1,"last_visitor":"Ada"}"#
    );
}

#[test]
fn the_tutorials_code_is_the_examples_code() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let tutorial = fs::read_to_string(root.join("docs/writing-an-agent.md")).unwrap();
    let code = fs::read_to_string(root.join("examples/hello/src/lib.rs")).unwrap();
    let start = code.find("use habitat_sdk").unwrap();
    assert!(
        tutorial.contains(&code[start..]),
        "the tutorial's code has changed from examples/hello/src/lib.rs"
    );
    let manifest = fs::read_to_string(root.join("examples/hello/agent.toml")).unwrap();
    assert!(
        tutorial.contains(&manifest),
        "the tutorial's manifest has changed from examples/hello/agent.toml"
    );
}

#[test]
fn saved_values_come_back_and_can_be_cleared() {
    let folder = installed(CHECK_MANIFEST, check_code());
    let folder = folder.path();
    check(folder, "expect-nothing note");
    check(folder, "save note hello there");
    check(folder, "expect note hello there");
    check(folder, "save note replaced");
    check(folder, "expect note replaced");
    check(folder, "save other separate");
    check(folder, "clear note");
    check(folder, "expect-nothing note");
    check(folder, "expect other separate");
    // Clearing something that isn't there is fine.
    check(folder, "clear note");
}

#[test]
fn a_saved_value_that_cant_be_read_is_an_error_and_isnt_lost() {
    let folder = installed(CHECK_MANIFEST, check_code());
    let folder = folder.path();
    let file = folder.join("root/agents/check/home/state/note.json");

    check(folder, "corrupt note");
    let said = check_fails(folder, "expect note anything");
    assert!(said.contains("invalid saved state"), "{said}");
    assert_eq!(fs::read(&file).unwrap(), b"{not json");

    // The same goes for a value of a different type.
    check(folder, "save note text");
    let said = check_fails(folder, "load-as-number note");
    assert!(said.contains("invalid saved state"), "{said}");
    assert!(file.exists());
}

#[test]
fn a_host_error_while_loading_isnt_mistaken_for_nothing_saved() {
    let folder = installed(CHECK_MANIFEST, check_code());
    let folder = folder.path();
    // A folder where the value's file should be, so that reading it fails with something else.
    check(folder, "write-file state/note.json/inside x");
    let said = check_fails(folder, "expect-nothing note");
    assert!(said.contains("the agent said:"), "{said}");
    assert!(!said.contains("holds"), "{said}");
}

#[test]
fn state_names_cant_lead_out_of_the_state_folder() {
    let folder = installed(CHECK_MANIFEST, check_code());
    let folder = folder.path();
    let home = folder.join("root/agents/check/home");
    for name in ["../outside", "a/b", "..", "a.b", "é"] {
        for command in ["save", "expect-nothing", "clear"] {
            let said = check_fails(folder, &format!("{command} {name} x"));
            assert!(said.contains("a state name can only use"), "{said}");
        }
    }
    assert_eq!(fs::read_dir(home).unwrap().count(), 0);
}

#[test]
fn whole_files_come_back_even_when_theyre_larger_than_one_read() {
    let folder = installed(CHECK_MANIFEST, check_code());
    let folder = folder.path();
    // On each side of the most that one read returns, and more than one read's worth.
    for size in [
        0,
        1,
        1024 * 1024 - 1,
        1024 * 1024,
        1024 * 1024 + 1,
        3 * 1024 * 1024,
    ] {
        check(folder, &format!("big-file {size}"));
    }
}

#[test]
fn the_other_helpers_work() {
    let folder = installed(CHECK_MANIFEST, check_code());
    let folder = folder.path();
    check(folder, "file-text notes/a.txt some text");
    check(folder, "not-text binary");
    check(folder, "missing-file nothing-here");
    check(folder, "list");
    check(folder, "clock-and-random");
}

#[test]
fn an_agents_error_reaches_the_user_as_its_own_claim() {
    let folder = installed(CHECK_MANIFEST, check_code());
    let said = check_fails(folder.path(), "fail - it went wrong");
    assert!(said.contains("the agent said: it went wrong"), "{said}");
}

#[test]
fn a_host_error_reaches_the_agent_and_holds_no_host_paths() {
    let folder = installed(
        "[agent]\nname = \"check\"\nversion = \"0.1.0\"\n\n[limits]\nmemory_mb = 64\nrun_seconds = 60\ndisk_mb = 1\n",
        check_code(),
    );
    let folder = folder.path();
    let said = check_fails(folder, "save-big big 2097152");
    assert!(said.contains("the agent said:"), "{said}");
    assert!(!said.contains(folder.to_str().unwrap()), "{said}");
    // The disk limit stopped it, and the agent can carry on.
    check(folder, "expect-nothing big");
}
