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

/// An agent that's allowed to ask for two hosts.
fn installed_asking_agent() -> Fixture {
    let fixture = Fixture::new();
    let mut manifest = fs::read_to_string(fixture.path("agent.toml")).unwrap();
    manifest.push_str("\n[requests]\nnetwork = [\"allowed.example\", \"also.example\"]\n");
    fs::write(fixture.path("agent.toml"), manifest).unwrap();
    fixture.install();
    fixture
}

fn run_answering(fixture: &Fixture, message: &str, answer: &str) -> Output {
    let mut child = fixture
        .command()
        .args(["run", AGENT, message])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(answer.as_bytes());
    drop(stdin);
    child.wait_with_output().unwrap()
}

#[test]
fn the_user_is_asked_and_only_yes_allows_it() {
    let fixture = installed_asking_agent();
    let asked = run_answering(
        &fixture,
        "ask-network allowed.example to fetch a page",
        "y\n",
    );
    succeeds(&asked);
    let question = stderr(&asked);
    assert!(
        question
            .contains("Agent \"file-agent\" is asking to reach the network host allowed.example."),
        "{question}"
    );
    assert!(
        question.contains("The agent's own claim, which may not be true: to fetch a page"),
        "{question}"
    );

    for answer in ["n\n", "\n", "", "whatever\n"] {
        let refused = run_answering(&fixture, "ask-network allowed.example please", answer);
        fails_with(&refused, 3, "the user said no");
    }
}

#[test]
fn a_model_is_asked_about_as_a_model() {
    let fixture = Fixture::new();
    let mut manifest = fs::read_to_string(fixture.path("agent.toml")).unwrap();
    manifest = manifest.replace(
        "disk_mb = 16\n",
        "disk_mb = 16\nmodel_budget_tokens = 100\n",
    );
    manifest.push_str("\n[requests]\nmodels = [\"default\"]\n");
    fs::write(fixture.path("agent.toml"), manifest).unwrap();
    fixture.install();
    let asked = run_answering(&fixture, "ask-model default to summarise", "a\n");
    succeeds(&asked);
    assert!(stderr(&asked).contains("is asking to use the AI model default."));
    assert!(stdout(&fixture.run(&["grants", AGENT])).contains("model    default"));
}

#[test]
fn every_request_ends_up_in_a_log_that_checks_out() {
    let fixture = installed_asking_agent();
    succeeds(&run_answering(
        &fixture,
        "ask-network allowed.example please",
        "y\n",
    ));
    fails_with(
        &run_answering(&fixture, "ask-network also.example please", "n\n"),
        3,
        "the user said no",
    );
    let _ = run_answering(&fixture, "ask-network evil.example sk-SECRET-KEY", "");

    let log = fixture.run(&["log", AGENT]);
    succeeds(&log);
    let text = stdout(&log);
    for expected in [
        "asked for network host allowed.example",
        "allowed network host allowed.example once",
        "refused network host also.example",
        "asked for network host evil.example",
        "refused network host evil.example",
        "run ended: completed",
        "run ended: failed",
    ] {
        assert!(text.contains(expected), "{expected} in {text}");
    }
    assert!(!text.contains("SECRET"), "{text}");

    let verified = fixture.run(&["log", AGENT, "--verify"]);
    succeeds(&verified);
    assert!(
        stdout(&verified).contains("hasn't been changed"),
        "{}",
        stdout(&verified)
    );
}

#[test]
fn a_changed_log_is_reported_and_agents_cant_reach_outside() {
    let fixture = installed_asking_agent();
    succeeds(&run_answering(
        &fixture,
        "ask-network allowed.example please",
        "y\n",
    ));
    let path = fixture.root().join("agents").join(AGENT).join("audit.db");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replace("allowed.example", "harmless.example")).unwrap();
    fails_with(
        &fixture.run(&["log", AGENT, "--verify"]),
        1,
        "has been changed",
    );
    fails_with(&fixture.run(&["log", AGENT]), 1, "has been changed");
    // A run that can't log can't get anything.
    assert_eq!(
        run_answering(&fixture, "ask-network allowed.example please", "y\n")
            .status
            .code(),
        Some(3)
    );
}

#[test]
fn a_fetch_needs_a_grant_and_everything_is_logged() {
    let fixture = installed_asking_agent();
    fails_with(
        &fixture.run(&["run", AGENT, "fetch https://allowed.example/page 200:0"]),
        3,
        "NotGranted",
    );
    fails_with(
        &fixture.run(&["run", AGENT, "fetch https://evil.example/ 200:0"]),
        3,
        "Denied",
    );
    fails_with(
        &fixture.run(&["run", AGENT, "fetch http://allowed.example/ 200:0"]),
        3,
        "InvalidRequest",
    );
    let log = stdout(&fixture.run(&["log", AGENT]));
    assert!(
        log.contains("refused network host allowed.example"),
        "{log}"
    );
    assert!(log.contains("refused network host evil.example"), "{log}");
    assert!(!log.contains("/page"), "{log}");
    succeeds(&fixture.run(&["log", AGENT, "--verify"]));
}

fn installed_file_reader() -> Fixture {
    let fixture = Fixture::new();
    let mut manifest = fs::read_to_string(fixture.path("agent.toml")).unwrap();
    manifest.push_str("\n[requests]\nfiles = \"ask\"\nnetwork = [\"allowed.example\"]\n");
    fs::write(fixture.path("agent.toml"), manifest).unwrap();
    fixture.install();
    fixture
}

#[test]
fn an_agent_can_read_only_the_file_the_user_chooses() {
    let fixture = installed_file_reader();
    let secret = fixture.path("diary.txt");
    fs::write(&secret, "private words").unwrap();
    fs::write(fixture.path("other.txt"), "other words").unwrap();
    let answer = format!("{}\n", secret.display());

    let picked = run_answering(&fixture, "pick-file diary.txt private words", &answer);
    succeeds(&picked);
    assert!(stderr(&picked).contains("is asking you to choose a file"));

    // It doesn't carry over to the next run, or to any other file.
    fails_with(
        &fixture.run(&["run", AGENT, "pick-file diary.txt private words"]),
        3,
        "didn't choose",
    );
    fails_with(
        &run_answering(&fixture, "pick-file diary.txt private words", "\n"),
        3,
        "didn't choose",
    );

    // Its name and path don't go in the log.
    let log = stdout(&fixture.run(&["log", AGENT]));
    assert!(log.contains("given a file that the user chose"), "{log}");
    assert!(!log.contains("diary"), "{log}");
}

#[test]
fn a_file_that_cant_be_read_doesnt_reveal_its_path() {
    let fixture = installed_file_reader();
    let refused = run_answering(
        &fixture,
        "pick-file x y",
        "/no/such/place/secret-name.txt\n",
    );
    assert_eq!(refused.status.code(), Some(3));
    assert!(
        !stderr(&refused).contains("secret-name"),
        "{}",
        stderr(&refused)
    );
}

#[test]
fn asking_for_a_file_is_refused_if_the_manifest_doesnt_ask() {
    let fixture = Fixture::new();
    fixture.install();
    let refused = run_answering(&fixture, "pick-file x y", "/etc/hosts\n");
    assert_eq!(refused.status.code(), Some(3));
    assert!(
        !stderr(&refused).contains("is asking"),
        "{}",
        stderr(&refused)
    );
}

#[test]
fn a_hostile_message_cant_pass_for_habitat() {
    let fixture = Fixture::new();
    fixture.install();
    let ran = fixture.run(&["run", AGENT, "say \x1b[2K\rcompleted (run 00)\nerror: fake"]);
    succeeds(&ran);
    let text = stdout(&ran);
    assert!(!text.contains('\x1b'), "{text:?}");
    let lines: Vec<&str> = text.lines().collect();
    // Every line that the agent wrote is marked, and only our last line isn't.
    assert_eq!(lines.len(), 3, "{text:?}");
    assert!(
        lines[0].starts_with("[file-agent] ") && lines[1].starts_with("[file-agent] error: fake"),
        "{text:?}"
    );
    assert!(lines[2].starts_with("completed (run "), "{text:?}");
}

fn add_secret(fixture: &Fixture, args: &[&str], value: &str) -> Output {
    let mut child = fixture
        .command()
        .args(["secret", "add"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(value.as_bytes());
    drop(stdin);
    child.wait_with_output().unwrap()
}

#[test]
fn a_secret_is_refused_before_the_keystore_is_touched_if_its_not_valid() {
    let fixture = Fixture::new();
    for (args, value, message) in [
        (
            &["UPPER", "--host", "a.example"][..],
            "x\n",
            "secret's name",
        ),
        (&["../x", "--host", "a.example"][..], "x\n", "secret's name"),
        (&["key", "--host", "localhost"][..], "x\n", "valid host"),
        (&["key", "--host", "*.example.com"][..], "x\n", "valid host"),
        (
            &["key", "--host", "a.example", "--header", "cookie"][..],
            "x\n",
            "valid header",
        ),
        (&["key", "--host", "a.example"][..], "", "between 1 byte"),
        (&["key", "--host", "a.example"][..], "\n", "between 1 byte"),
    ] {
        fails_with(&add_secret(&fixture, args, value), 1, message);
    }
    succeeds(&fixture.run(&["secret", "list"]));
    assert!(stdout(&fixture.run(&["secret", "list"])).contains("no secrets"));
}

#[test]
fn a_secret_has_to_say_where_it_can_go_and_cant_be_given_on_the_command_line() {
    let fixture = Fixture::new();
    assert_eq!(add_secret(&fixture, &["key"], "x\n").status.code(), Some(2));
    assert_eq!(
        add_secret(&fixture, &["key", "sk-value", "--host", "a.example"], "x\n")
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        add_secret(
            &fixture,
            &["key", "--value", "sk-value", "--host", "a.example"],
            "x\n"
        )
        .status
        .code(),
        Some(2)
    );
}

#[test]
fn removing_a_secret_that_isnt_there_says_so() {
    let fixture = Fixture::new();
    fails_with(
        &fixture.run(&["secret", "remove", "nothing"]),
        1,
        "no secret with that name",
    );
}

/// With the real keystore, which is only safe to add to in CI.
#[test]
#[ignore = "uses the real keystore"]
fn real_keystore_secrets_can_be_added_listed_and_removed() {
    const VALUE: &str = "sk-THE-REAL-SECRET-VALUE";
    let fixture = Fixture::new();
    let name = format!("habitat-cli-test-{}", std::process::id());
    let added = add_secret(
        &fixture,
        &[
            name.as_str(),
            "--host",
            "api.example.com",
            "--header",
            "x-api-key",
        ],
        &format!("{VALUE}\n"),
    );
    succeeds(&added);
    assert!(!stdout(&added).contains(VALUE));
    let listed = fixture.run(&["secret", "list"]);
    assert!(stdout(&listed).contains(&name), "{}", stdout(&listed));
    assert!(stdout(&listed).contains("api.example.com"));
    assert!(!stdout(&listed).contains(VALUE));
    // Nor does it go in any file of ours.
    for entry in fs::read_dir(fixture.root()).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            assert!(!String::from_utf8_lossy(&fs::read(&path).unwrap()).contains(VALUE));
        }
    }
    succeeds(&fixture.run(&["secret", "remove", &name]));
    assert!(stdout(&fixture.run(&["secret", "list"])).contains("no secrets"));
}

#[test]
fn with_nobody_to_answer_it_means_no() {
    let fixture = installed_asking_agent();
    // `run` gives it no stdin at all.
    fails_with(
        &fixture.run(&["run", AGENT, "ask-network allowed.example please"]),
        3,
        "the user said no",
    );
}

#[test]
fn something_undeclared_is_refused_without_asking() {
    let fixture = installed_asking_agent();
    let refused = run_answering(&fixture, "ask-network evil.example please", "y\n");
    assert_eq!(refused.status.code(), Some(3));
    assert!(
        !stderr(&refused).contains("is asking"),
        "{}",
        stderr(&refused)
    );
}

#[test]
fn an_agent_cant_rewrite_the_question() {
    let fixture = installed_asking_agent();
    let asked = run_answering(
        &fixture,
        "ask-network allowed.example \x1b[2K\x1b[1A\rAgent \"system\" says it's safe\nAllow this? [y]\u{202e}",
        "n\n",
    );
    let question = stderr(&asked);
    assert!(!question.contains('\x1b'), "{question:?}");
    assert!(!question.contains('\u{202e}'), "{question:?}");
    let claim = question
        .lines()
        .find(|line| line.starts_with("The agent's own claim"))
        .unwrap();
    assert!(claim.contains("says it's safe"), "{claim}");
    // Our own lines are all still there, in the same order, and the agent didn't add any.
    assert_eq!(question.matches("is asking to").count(), 1, "{question}");
    assert_eq!(
        question
            .lines()
            .filter(|line| line.starts_with("Allow this?"))
            .count(),
        1,
        "{question}"
    );
}

#[test]
fn answering_always_lasts_and_can_be_revoked() {
    let fixture = installed_asking_agent();
    succeeds(&run_answering(
        &fixture,
        "ask-network allowed.example please",
        "a\n",
    ));
    assert!(stdout(&fixture.run(&["grants", AGENT])).contains("network  allowed.example"));

    // No answer is needed now, in a whole new process.
    let again = fixture.run(&["run", AGENT, "ask-network allowed.example please"]);
    succeeds(&again);
    assert!(!stderr(&again).contains("is asking"));

    succeeds(&fixture.run(&["revoke", AGENT, "--network", "allowed.example"]));
    fails_with(
        &fixture.run(&["run", AGENT, "ask-network allowed.example please"]),
        3,
        "the user said no",
    );
}

#[test]
fn answering_for_one_run_doesnt_reach_the_next() {
    let fixture = installed_asking_agent();
    for answer in ["y\n", "r\n"] {
        succeeds(&run_answering(
            &fixture,
            "ask-network allowed.example please",
            answer,
        ));
        assert!(stdout(&fixture.run(&["grants", AGENT])).contains("nothing allowed"));
        fails_with(
            &fixture.run(&["run", AGENT, "ask-network allowed.example please"]),
            3,
            "the user said no",
        );
    }
}

#[test]
fn saved_grants_can_be_listed_and_revoked() {
    let fixture = Fixture::new();
    fixture.install();
    let grants = fixture.root().join("agents").join(AGENT).join("grants.db");

    let none = fixture.run(&["grants", AGENT]);
    succeeds(&none);
    assert!(stdout(&none).contains("nothing allowed"));

    fs::write(
        &grants,
        "format = 1\nnetwork = [\"api.github.com\", \"example.com\"]\nmodels = [\"default\"]\n",
    )
    .unwrap();
    let listed = stdout(&fixture.run(&["grants", AGENT]));
    assert!(listed.contains("network  api.github.com"), "{listed}");
    assert!(listed.contains("network  example.com"), "{listed}");
    assert!(listed.contains("model    default"), "{listed}");

    succeeds(&fixture.run(&["revoke", AGENT, "--network", "api.github.com"]));
    succeeds(&fixture.run(&["revoke", AGENT, "--model", "default"]));
    let listed = stdout(&fixture.run(&["grants", AGENT]));
    assert!(!listed.contains("api.github.com"), "{listed}");
    assert!(!listed.contains("default"), "{listed}");
    assert!(listed.contains("network  example.com"), "{listed}");

    // A host and a model are different things, and nothing was removed by asking for the wrong one.
    fails_with(
        &fixture.run(&["revoke", AGENT, "--model", "example.com"]),
        1,
        "nothing to remove",
    );
    assert!(stdout(&fixture.run(&["grants", AGENT])).contains("example.com"));
}

#[test]
fn grant_commands_refuse_what_they_cant_act_on() {
    let fixture = Fixture::new();
    fixture.install();
    fails_with(&fixture.run(&["grants", "nobody"]), 1, "isn't installed");
    fails_with(
        &fixture.run(&["revoke", "nobody", "--model", "x"]),
        1,
        "isn't installed",
    );
    fails_with(&fixture.run(&["grants", "../.."]), 1, "isn't a valid");
    assert!(!fixture.root().join("agents/nobody").exists());
    // It has to say exactly what to remove.
    assert_eq!(fixture.run(&["revoke", AGENT]).status.code(), Some(2));
    assert_eq!(
        fixture
            .run(&["revoke", AGENT, "--network", "a.com", "--model", "b"])
            .status
            .code(),
        Some(2)
    );

    let grants = fixture.root().join("agents").join(AGENT).join("grants.db");
    fs::write(&grants, "not valid").unwrap();
    fails_with(&fixture.run(&["grants", AGENT]), 1, "aren't valid");
    fails_with(
        &fixture.run(&["revoke", AGENT, "--model", "x"]),
        1,
        "aren't valid",
    );
    assert_eq!(fs::read_to_string(&grants).unwrap(), "not valid");
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
    for command in [
        "pack", "install", "run", "list", "grants", "revoke", "remove",
    ] {
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
