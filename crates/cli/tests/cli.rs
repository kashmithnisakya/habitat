//! Runs the real `habitat` program, as someone would from a terminal. These need the
//! `wasm32-unknown-unknown` target, because they build the sample agent in `examples/file-agent`.

#[path = "../../backend-wasm/tests/support/mod.rs"]
mod support;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use support::file_agent_code;

const AGENT: &str = "file-agent";
const WAIT_LIMIT: Duration = Duration::from_secs(120);

/// A folder of agents, and a package ready to install into it.
struct Fixture {
    folder: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            folder: TempDir::new().unwrap(),
        };
        fs::write(fixture.path("agent.wasm"), file_agent_code()).unwrap();
        fixture.write_manifest("agent.toml", AGENT, 10);
        fixture
    }

    fn path(&self, name: &str) -> PathBuf {
        self.folder.path().join(name)
    }

    fn root(&self) -> PathBuf {
        self.path("root")
    }

    fn write_manifest(&self, file: &str, name: &str, run_seconds: u32) {
        fs::write(
            self.path(file),
            format!(
                "[agent]\nname = \"{name}\"\nversion = \"1.2.3\"\n\n[limits]\nmemory_mb = 64\nrun_seconds = {run_seconds}\ndisk_mb = 16\n"
            ),
        )
        .unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_habitat"));
        command
            .current_dir(self.folder.path())
            .env("HABITAT_HOME", self.root())
            // So that even a bug can't reach the real folder of whoever runs these tests.
            .env("HOME", self.path("no-such-home"))
            .env("LOCALAPPDATA", self.path("no-such-home"))
            .env_remove("XDG_DATA_HOME")
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn pack_as(&self, manifest: &str, output: &str) {
        succeeds(&self.run(&["pack", manifest, "agent.wasm", "-o", output]));
    }

    fn install(&self) {
        self.pack_as("agent.toml", "agent.habitat");
        succeeds(&self.run(&["install", "agent.habitat"]));
    }

    fn home_file(&self, agent: &str, name: &str) -> PathBuf {
        self.root()
            .join("agents")
            .join(agent)
            .join("home")
            .join(name)
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

#[track_caller]
fn succeeds(output: &Output) {
    assert_eq!(output.status.code(), Some(0), "{}", stderr(output));
}

#[track_caller]
fn fails_with(output: &Output, code: i32, message: &str) {
    assert_eq!(output.status.code(), Some(code), "{}", stderr(output));
    assert!(stderr(output).contains(message), "{}", stderr(output));
}

/// Stops the program if a test ends early, so that it can't be left running.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for(path: &Path) {
    let start = Instant::now();
    while !path.exists() {
        assert!(start.elapsed() < WAIT_LIMIT, "gave up waiting for a file");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Waits for the program to end, and fails rather than hangs if it doesn't.
#[cfg(unix)]
fn wait_for_exit(running: &mut Running) -> Option<i32> {
    let start = Instant::now();
    loop {
        if let Some(status) = running.0.try_wait().unwrap() {
            return status.code();
        }
        assert!(
            start.elapsed() < WAIT_LIMIT,
            "gave up waiting for it to end"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(unix)]
fn send_signal(running: &Running, signal: &str) {
    let status = Command::new("kill")
        .args([signal, &running.0.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn an_agent_goes_from_package_to_running_to_removed() {
    let fixture = Fixture::new();
    fixture.pack_as("agent.toml", "agent.habitat");
    let installed = fixture.run(&["install", "agent.habitat"]);
    succeeds(&installed);
    assert!(stdout(&installed).contains("Installed file-agent 1.2.3"));
    assert!(stdout(&installed).contains("Package hash: "));

    let listed = fixture.run(&["list"]);
    succeeds(&listed);
    assert!(stdout(&listed).contains("file-agent"));
    assert!(stdout(&listed).contains("1.2.3"));
    assert!(stdout(&listed).contains("never"));

    let ran = fixture.run(&["run", AGENT, "write notes.txt hello"]);
    succeeds(&ran);
    assert!(
        stdout(&ran).starts_with("completed (run "),
        "{}",
        stdout(&ran)
    );

    // The state is still there for a whole new process.
    succeeds(&fixture.run(&["run", AGENT, "expect notes.txt hello"]));
    assert!(stdout(&fixture.run(&["list"])).contains("completed"));

    succeeds(&fixture.run(&["remove", AGENT, "--yes"]));
    assert!(stdout(&fixture.run(&["list"])).contains("No agents are installed."));
    fails_with(&fixture.run(&["run", AGENT, "x"]), 1, "isn't installed");
}

#[test]
fn each_way_a_run_can_end_has_its_own_exit_code() {
    let fixture = Fixture::new();
    fixture.install();

    let failed = fixture.run(&["run", AGENT, "fail - it went wrong"]);
    fails_with(&failed, 3, "failed, and the agent said: it went wrong");

    fails_with(&fixture.run(&["run", AGENT, "panic"]), 3, "crashed");
    fails_with(
        &fixture.run(&["run", AGENT, "allocate-forever"]),
        3,
        "memory limit",
    );

    let quick = Fixture::new();
    quick.write_manifest("agent.toml", AGENT, 1);
    quick.install();
    fails_with(&quick.run(&["run", AGENT, "loop-forever"]), 3, "time limit");
    // A failed run doesn't get in the way of the next.
    succeeds(&quick.run(&["run", AGENT, "write a.txt b"]));
}

#[test]
fn an_agent_whose_code_is_refused_exits_with_its_own_code() {
    let fixture = Fixture::new();
    fs::write(fixture.path("bad.wasm"), b"\0asm\x0d\0\x01\0 not really").unwrap();
    succeeds(&fixture.run(&["pack", "agent.toml", "bad.wasm", "-o", "bad.habitat"]));
    succeeds(&fixture.run(&["install", "bad.habitat"]));
    fails_with(
        &fixture.run(&["run", AGENT, "x"]),
        4,
        "didn't start: invalid agent code",
    );
    assert!(stdout(&fixture.run(&["list"])).contains("didn't start"));
}

#[test]
fn text_from_a_hostile_agent_is_safe_to_print() {
    let fixture = Fixture::new();
    fixture.install();
    let failed = fixture.run(&["run", AGENT, "fail - \x1b[31mfake\u{202e}\nerror: fake"]);
    assert_eq!(failed.status.code(), Some(3));
    let text = stderr(&failed);
    assert!(text.is_ascii(), "{text:?}");
    assert!(!text.trim_end().contains(['\x1b', '\n']), "{text:?}");
    assert!(!stdout(&fixture.run(&["list"])).contains('\x1b'));
}

#[cfg(unix)]
#[test]
fn stopping_the_program_stops_the_agent_and_keeps_what_it_saved() {
    for signal in ["-INT", "-TERM"] {
        let fixture = Fixture::new();
        fixture.write_manifest("agent.toml", AGENT, 60);
        fixture.install();
        let mut running = Running(
            fixture
                .command()
                .args(["run", AGENT, "write-then-loop saved.txt state"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        wait_for(&fixture.home_file(AGENT, "saved.txt"));

        send_signal(&running, signal);
        assert_eq!(wait_for_exit(&mut running), Some(130), "{signal}");

        assert!(
            stdout(&fixture.run(&["list"])).contains("stopped"),
            "{signal}"
        );
        succeeds(&fixture.run(&["run", AGENT, "expect saved.txt state"]));
    }
}

#[cfg(unix)]
/// A package that's within the limits on compiling, and takes far longer than the wait to stop.
/// Arithmetic with no loops, as `docs/compile-costs.md` measures, at about 40 s or more on a Mac.
fn slow_to_compile() -> Vec<u8> {
    let body = "local.get 0 local.get 1 i32.add local.set 0 ".repeat(70_000);
    let function = format!("(func (local i32 i32) {body})");
    let functions = function.repeat(30);
    wat::parse_str(format!("(component (core module {functions}))")).unwrap()
}

#[cfg(unix)]
#[test]
fn stopping_the_program_during_a_compile_doesnt_wait_for_it_to_end() {
    let fixture = Fixture::new();
    fs::write(fixture.path("slow.wasm"), slow_to_compile()).unwrap();
    succeeds(&fixture.run(&["pack", "agent.toml", "slow.wasm", "-o", "slow.habitat"]));
    succeeds(&fixture.run(&["install", "slow.habitat"]));

    let mut running = Running(
        fixture
            .command()
            .args(["run", AGENT, "x"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    // The run's record is saved just before the compile starts.
    let runs = fixture.root().join("agents").join(AGENT).join("runs");
    let start = Instant::now();
    while fs::read_dir(&runs).unwrap().next().is_none() {
        assert!(start.elapsed() < WAIT_LIMIT, "gave up waiting for the run");
        thread::sleep(Duration::from_millis(5));
    }

    let start = Instant::now();
    send_signal(&running, "-TERM");
    assert_eq!(wait_for_exit(&mut running), Some(130));
    // It gets 5 seconds, and it shouldn't take many more than that, however slow the compile is.
    assert!(
        start.elapsed() < Duration::from_secs(20),
        "{:?}",
        start.elapsed()
    );
    assert!(stdout(&fixture.run(&["list"])).contains("interrupted"));
}

#[test]
fn killing_the_program_leaves_an_agent_that_can_run_again() {
    let fixture = Fixture::new();
    fixture.write_manifest("agent.toml", AGENT, 60);
    fixture.install();
    let mut running = Running(
        fixture
            .command()
            .args(["run", AGENT, "write-then-loop saved.txt state"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for(&fixture.home_file(AGENT, "saved.txt"));

    // While it's running, it can't be run again or removed.
    fails_with(&fixture.run(&["run", AGENT, "x"]), 5, "already running");
    fails_with(
        &fixture.run(&["remove", AGENT, "--yes"]),
        5,
        "already running",
    );
    assert!(stdout(&fixture.run(&["list"])).contains("running"));

    running.0.kill().unwrap();
    running.0.wait().unwrap();

    assert!(stdout(&fixture.run(&["list"])).contains("interrupted"));
    succeeds(&fixture.run(&["run", AGENT, "expect saved.txt state"]));
}

fn answer_removal(fixture: &Fixture, answer: Option<&str>) -> Output {
    let mut command = fixture.command();
    let mut child = command
        .args(["remove", AGENT])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    if let Some(answer) = answer {
        stdin.write_all(answer.as_bytes()).unwrap();
    }
    drop(stdin);
    child.wait_with_output().unwrap()
}

#[test]
fn removing_only_goes_ahead_on_a_clear_yes() {
    let fixture = Fixture::new();
    fixture.install();
    succeeds(&fixture.run(&["run", AGENT, "write notes.txt hello"]));
    let notes = fixture.home_file(AGENT, "notes.txt");

    for answer in [
        None,
        Some(""),
        Some("\n"),
        Some("n\n"),
        Some("no\n"),
        Some("yep\n"),
        Some("yes please\n"),
    ] {
        let output = answer_removal(&fixture, answer);
        fails_with(&output, 1, "nothing was removed");
        assert!(stderr(&output).contains("Continue?"));
        assert!(notes.exists(), "{answer:?}");
    }
    // With no way to answer at all, the answer is no.
    fails_with(&fixture.run(&["remove", AGENT]), 1, "nothing was removed");
    assert!(notes.exists());

    for answer in ["y\n", "YES\n"] {
        succeeds(&answer_removal(&fixture, Some(answer)));
        assert!(!notes.exists());
        succeeds(&fixture.run(&["install", "agent.habitat"]));
        assert!(!notes.exists());
    }
}

#[test]
fn removing_leaves_other_agents_and_files_alone() {
    let fixture = Fixture::new();
    fixture.install();
    fixture.write_manifest("other.toml", "other-agent", 10);
    fixture.pack_as("other.toml", "other.habitat");
    succeeds(&fixture.run(&["install", "other.habitat"]));
    succeeds(&fixture.run(&["run", AGENT, "write notes.txt mine"]));
    succeeds(&fixture.run(&["run", "other-agent", "write notes.txt theirs"]));

    succeeds(&fixture.run(&["remove", AGENT, "--yes"]));

    assert!(fixture.path("agent.habitat").exists());
    assert!(!fixture.root().join("agents").join(AGENT).exists());
    succeeds(&fixture.run(&["run", "other-agent", "expect notes.txt theirs"]));
}

#[test]
fn removing_an_unknown_agent_doesnt_ask() {
    let fixture = Fixture::new();
    let output = fixture.run(&["remove", "nobody"]);
    fails_with(&output, 1, "isn't installed");
    assert!(!stderr(&output).contains("Continue?"));
    for name in ["..", "../..", "/tmp", "a/b"] {
        fails_with(
            &fixture.run(&["remove", name, "--yes"]),
            1,
            "isn't a valid agent name",
        );
    }
    assert!(fixture.path("agent.wasm").exists());
}

#[test]
fn a_bad_package_is_refused_and_installs_nothing() {
    let fixture = Fixture::new();
    fs::write(fixture.path("junk.habitat"), "not a package").unwrap();
    fails_with(
        &fixture.run(&["install", "junk.habitat"]),
        1,
        "invalid package",
    );
    fails_with(
        &fixture.run(&["install", "missing.habitat"]),
        1,
        "missing.habitat",
    );
    assert!(stdout(&fixture.run(&["list"])).contains("No agents are installed."));
    assert!(!fixture.root().join("agents").exists());
}

#[test]
fn installing_twice_is_refused_and_keeps_the_agents_files() {
    let fixture = Fixture::new();
    fixture.install();
    succeeds(&fixture.run(&["run", AGENT, "write notes.txt hello"]));
    fails_with(
        &fixture.run(&["install", "agent.habitat"]),
        1,
        "already installed",
    );
    succeeds(&fixture.run(&["run", AGENT, "expect notes.txt hello"]));
}

#[test]
fn packing_checks_what_it_packs_and_doesnt_overwrite() {
    let fixture = Fixture::new();
    fs::write(fixture.path("bad.toml"), "not a manifest").unwrap();
    fs::write(fixture.path("core.wasm"), b"\0asm\x01\0\0\0").unwrap();
    let cases: [(&[&str], &str); 3] = [
        (&["pack", "bad.toml", "agent.wasm", "-o", "out"], "manifest"),
        (
            &["pack", "agent.toml", "core.wasm", "-o", "out"],
            "component",
        ),
        (
            &["pack", "missing.toml", "agent.wasm", "-o", "out"],
            "missing.toml",
        ),
    ];
    for (args, message) in cases {
        fails_with(&fixture.run(args), 1, message);
        assert!(!fixture.path("out").exists(), "{args:?}");
    }

    fs::write(fixture.path("taken"), "keep").unwrap();
    fails_with(
        &fixture.run(&["pack", "agent.toml", "agent.wasm", "-o", "taken"]),
        1,
        "taken",
    );
    assert_eq!(fs::read_to_string(fixture.path("taken")).unwrap(), "keep");
}

#[test]
fn a_package_is_the_same_each_time_and_the_hash_says_so() {
    let fixture = Fixture::new();
    let first = fixture.run(&["pack", "agent.toml", "agent.wasm", "-o", "first"]);
    let second = fixture.run(&["pack", "agent.toml", "agent.wasm", "-o", "second"]);
    let hash = |output: &Output| stdout(output).lines().last().unwrap().to_string();
    assert_eq!(hash(&first), hash(&second));
    assert_eq!(
        fs::read(fixture.path("first")).unwrap(),
        fs::read(fixture.path("second")).unwrap()
    );
    let installed = {
        fs::rename(fixture.path("first"), fixture.path("agent.habitat")).unwrap();
        fixture.run(&["install", "agent.habitat"])
    };
    assert_eq!(hash(&installed), hash(&first));
}

#[test]
fn the_command_line_says_how_to_use_it() {
    let fixture = Fixture::new();
    let help = fixture.run(&["--help"]);
    succeeds(&help);
    for word in [
        "pack",
        "install",
        "run",
        "list",
        "remove",
        "Exit codes:",
        "HABITAT_HOME",
    ] {
        assert!(stdout(&help).contains(word), "{word}");
    }
    for command in ["pack", "install", "run", "list", "remove"] {
        succeeds(&fixture.run(&[command, "--help"]));
    }
    assert_eq!(fixture.run(&[]).status.code(), Some(2));
    assert_eq!(fixture.run(&["nonsense"]).status.code(), Some(2));
    assert_eq!(fixture.run(&["run"]).status.code(), Some(2));
}

#[test]
fn the_folder_can_come_from_the_command_line_or_the_environment() {
    let fixture = Fixture::new();
    fixture.pack_as("agent.toml", "agent.habitat");
    let other = fixture.path("other-root");
    let flag = other.to_str().unwrap();
    succeeds(&fixture.run(&["--root", flag, "install", "agent.habitat"]));
    assert!(other.join("agents").join(AGENT).exists());
    assert!(!fixture.root().exists());
    assert!(stdout(&fixture.run(&["list", "--root", flag])).contains(AGENT));
    assert!(stdout(&fixture.run(&["list"])).contains("No agents"));
}

#[test]
fn without_a_home_folder_it_says_what_to_do() {
    let fixture = Fixture::new();
    let output = fixture
        .command()
        .env_remove("HABITAT_HOME")
        .env_remove("HOME")
        .env_remove("LOCALAPPDATA")
        .arg("list")
        .output()
        .unwrap();
    fails_with(&output, 1, "--root");
}

#[test]
fn an_empty_home_folder_counts_as_none() {
    let fixture = Fixture::new();
    let output = fixture
        .command()
        .env_remove("HABITAT_HOME")
        .env("HOME", "")
        .env("LOCALAPPDATA", "")
        .arg("list")
        .output()
        .unwrap();
    fails_with(&output, 1, "--root");
}

#[cfg(not(any(target_os = "macos", windows)))]
#[test]
fn the_default_folder_follows_xdg_data_home_if_its_absolute() {
    let fixture = Fixture::new();
    fixture.pack_as("agent.toml", "agent.habitat");
    let home = fixture.path("user");
    let data = fixture.path("data");
    let install = |xdg: &std::ffi::OsStr| {
        succeeds(
            &fixture
                .command()
                .env_remove("HABITAT_HOME")
                .env("HOME", &home)
                .env("XDG_DATA_HOME", xdg)
                .args(["install", "agent.habitat"])
                .output()
                .unwrap(),
        );
    };
    install(data.as_os_str());
    assert!(data.join("habitat/agents/file-agent").is_dir());
    // The standard says to ignore a relative path.
    install(std::ffi::OsStr::new("relative"));
    assert!(home.join(".local/share/habitat/agents/file-agent").is_dir());
}

#[test]
fn the_default_folder_is_under_the_users_home() {
    let fixture = Fixture::new();
    fixture.pack_as("agent.toml", "agent.habitat");
    let home = fixture.path("user");
    fs::create_dir(&home).unwrap();
    let output = fixture
        .command()
        .env_remove("HABITAT_HOME")
        .env_remove("XDG_DATA_HOME")
        .env("HOME", &home)
        .env("LOCALAPPDATA", &home)
        .args(["install", "agent.habitat"])
        .output()
        .unwrap();
    succeeds(&output);
    let expected = if cfg!(windows) {
        "habitat/agents/file-agent"
    } else if cfg!(target_os = "macos") {
        "Library/Application Support/habitat/agents/file-agent"
    } else {
        ".local/share/habitat/agents/file-agent"
    };
    assert!(home.join(expected).is_dir());
}

#[test]
fn printing_to_a_closed_pipe_isnt_a_crash() {
    let fixture = Fixture::new();
    fixture.install();
    let mut child = fixture
        .command()
        .arg("list")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn no_host_paths_appear_in_agent_errors() {
    let fixture = Fixture::new();
    fixture.install();
    let root = fixture.root();
    let root = root.to_str().unwrap();
    for args in [
        vec!["run", AGENT, "fail - x"],
        vec!["run", "nobody", "x"],
        vec!["run", AGENT, "panic"],
    ] {
        let output = fixture.run(&args);
        assert!(!stderr(&output).contains(root), "{}", stderr(&output));
        assert!(!stdout(&output).contains(root), "{}", stdout(&output));
    }
}
