//! Runs real components through the backend. These need the `wasm32-unknown-unknown` target,
//! because they build the sample agent in `examples/file-agent`.

mod support;

use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use habitat_backend_wasm::WasmBackend;
use habitat_backend_wasm::code_shape::CodeShape;
use habitat_backend_wasm::compile_limits::CompileLimits;
use habitat_core::backend::{
    Backend, DenyEverything, Event, HostLimits, LoadError, LoadedAgent, RunLimits, RunOutcome,
    StopSignal,
};
use habitat_core::home::{AgentStorage, Home};
use habitat_core::package::Package;
use tempfile::TempDir;

use support::file_agent_code;

const MANIFEST: &str = "[agent]\nname = \"file-agent\"\nversion = \"0.1.0\"\n\n[limits]\nmemory_mb = 64\nrun_seconds = 10\ndisk_mb = 1\n";
const MB: u64 = 1024 * 1024;
const DISK_LIMIT: u64 = 16 * MB;

/// Plenty of room, for tests that aren't about limits.
fn roomy_limits() -> RunLimits {
    RunLimits {
        memory_bytes: 256 * MB,
        run_time: Duration::from_secs(60),
        disk_bytes: DISK_LIMIT,
    }
}

// Turns the core module `$main` into an agent, by exporting its handle-event.
const MAIN_INSTANCE_AND_EXPORT: &str = r#"
    (core instance $i (instantiate $main))
    (alias core export $i "memory" (core memory $memory))
    (type $event (variant (case "user-message" string) (case "scheduled" string)))
    (export $exported "event" (type $event))
    (func (export "handle-event") (param "event" $exported) (result (result (error string)))
        (canon lift (core func $i "handle-event")
            (memory $memory)
            (realloc (core func $i "cabi_realloc"))
            string-encoding=utf8))
"#;

const REALLOC: &str =
    r#"(func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) i32.const 1024)"#;

fn component_from_main(main: &str) -> Vec<u8> {
    wat::parse_str(format!(
        "(component (core module $main {main}) {MAIN_INSTANCE_AND_EXPORT})"
    ))
    .unwrap()
}

// A component with no imports. `body` is what handle-event does, and `extra` is anything else in
// its core module.
fn component_exporting(body: &str, extra: &str) -> Vec<u8> {
    component_from_main(&format!(
        r#"(memory (export "memory") 1)
           (func (export "handle-event") (param i32 i32 i32) (result i32) {body})
           {REALLOC}
           {extra}"#
    ))
}

fn component_with_realloc(realloc_body: &str) -> Vec<u8> {
    component_from_main(&format!(
        r#"(memory (export "memory") 1)
           (func (export "handle-event") (param i32 i32 i32) (result i32) i32.const 0)
           (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) {realloc_body})"#
    ))
}

fn component_with_memory_pages(pages: u32) -> Vec<u8> {
    component_from_main(&format!(
        r#"(memory (export "memory") {pages})
           (func (export "handle-event") (param i32 i32 i32) (result i32) i32.const 0)
           {REALLOC}"#
    ))
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

    /// For testing that the limits on a run hold even for a component that gets past the limits
    /// on compiling, or that a host has chosen to allow.
    fn past_the_compile_limits(code: Vec<u8>) -> Self {
        let root = TempDir::new().unwrap();
        let storage = AgentStorage::open(root.path(), "file-agent").unwrap();
        let mut backend = WasmBackend::new().unwrap();
        backend.set_compile_limits(CompileLimits {
            memories: u64::MAX,
            table_entries: u64::MAX,
            ..CompileLimits::default()
        });
        let agent = backend
            .load(&Package::new(MANIFEST, code).unwrap())
            .unwrap();
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
        self.run_limited(message, &roomy_limits())
    }

    fn run_limited(&self, message: &str, limits: &RunLimits) -> RunOutcome {
        self.agent.run(
            &Event::UserMessage(message.to_string()),
            self.home(),
            limits,
            &StopSignal::default(),
            Box::new(DenyEverything),
        )
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
fn the_sample_agent_is_far_within_the_compile_limits() {
    let limits = CompileLimits::default();
    let shape = CodeShape::of(file_agent_code()).unwrap();
    assert_eq!(limits.broken_by(&shape), None);

    // None of its limits should be close, or a real agent would soon run into them.
    let far_tighter = CompileLimits {
        time: limits.time,
        nested_components: 0,
        modules: limits.modules / 4,
        functions: limits.functions / 4,
        code_bytes: limits.code_bytes / 4,
        function_bytes: limits.function_bytes / 4,
        work: limits.work / 4,
        nested_loops: limits.nested_loops / 4,
        function_loops: limits.function_loops / 4,
        function_branch_points: limits.function_branch_points / 4,
        function_block_values: limits.function_block_values / 4,
        instance_imports: limits.instance_imports / 4,
        memories: limits.memories / 4,
        table_entries: limits.table_entries / 4,
        canonical_functions: limits.canonical_functions / 4,
        declarations: limits.declarations / 4,
        declaration_bytes: limits.declaration_bytes / 4,
        signature_values: limits.signature_values / 4,
    };
    assert_eq!(far_tighter.broken_by(&shape), None, "{shape:?}");
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
    let run = |event: Event| {
        fixture.agent.run(
            &event,
            fixture.home(),
            &roomy_limits(),
            &StopSignal::default(),
            Box::new(DenyEverything),
        )
    };
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
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, fixture.home_path().join("link")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&outside, fixture.home_path().join("link")).unwrap();

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
        &roomy_limits(),
        &StopSignal::default(),
        Box::new(DenyEverything),
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

/// Fails a test instead of hanging it, if a limit doesn't work.
fn within_a_minute<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || sender.send(work()));
    receiver
        .recv_timeout(Duration::from_secs(60))
        .expect("the agent wasn't stopped")
}

fn short_time() -> RunLimits {
    RunLimits {
        run_time: Duration::from_millis(300),
        ..roomy_limits()
    }
}

/// Runs an agent that should run out of time, and checks that it's stopped at about the right time.
fn expect_stopped_on_time(code: Vec<u8>, message: &'static str) {
    within_a_minute(move || {
        let fixture = Fixture::new(code);
        let start = Instant::now();
        assert_eq!(
            fixture.run_limited(message, &short_time()),
            RunOutcome::TimeLimit
        );
        let elapsed = start.elapsed();
        assert!(elapsed >= short_time().run_time, "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    });
}

#[test]
fn an_infinite_loop_is_stopped_on_time() {
    expect_stopped_on_time(file_agent_code().to_vec(), "loop-forever");
}

#[test]
fn an_infinite_loop_calling_the_host_is_stopped_on_time() {
    expect_stopped_on_time(file_agent_code().to_vec(), "call-host-forever");
}

#[test]
fn a_loop_that_never_calls_anything_is_stopped_on_time() {
    expect_stopped_on_time(component_exporting("(loop (br 0)) i32.const 0", ""), "x");
}

#[test]
fn a_loop_while_starting_is_stopped_on_time() {
    let code = component_exporting("i32.const 0", "(func $start (loop (br 0))) (start $start)");
    expect_stopped_on_time(code, "x");
}

#[test]
fn a_loop_while_receiving_the_event_is_stopped_on_time() {
    // The host calls cabi_realloc to pass the event in, which runs before handle-event does.
    let code = component_with_realloc("(loop (br 0)) i32.const 0");
    expect_stopped_on_time(code, "x");
}

#[test]
fn an_agent_that_finishes_in_time_is_not_stopped() {
    let fixture = Fixture::file_agent();
    // Long enough for several ticks, so that a tick alone doesn't stop a run.
    let limits = RunLimits {
        run_time: Duration::from_secs(30),
        ..roomy_limits()
    };
    assert_eq!(
        fixture.run_limited("allocate 1", &limits),
        RunOutcome::Completed
    );
}

#[test]
fn each_run_gets_its_full_time() {
    // The deadline starts when a run does, not when the agent was loaded.
    let fixture = Fixture::file_agent();
    thread::sleep(short_time().run_time * 2);
    assert_eq!(
        fixture.run_limited("expect-user-message", &short_time()),
        RunOutcome::Completed
    );
}

#[test]
fn runs_at_the_same_time_keep_their_own_deadlines() {
    within_a_minute(|| {
        let long_run = thread::spawn(|| {
            let fixture = Fixture::file_agent();
            let limits = RunLimits {
                run_time: Duration::from_secs(2),
                ..roomy_limits()
            };
            let start = Instant::now();
            assert_eq!(
                fixture.run_limited("loop-forever", &limits),
                RunOutcome::TimeLimit
            );
            start.elapsed()
        });
        // Another run ticks at the same time, and mustn't cut the long run short.
        for _ in 0..3 {
            expect_stopped_on_time(file_agent_code().to_vec(), "loop-forever");
        }
        let elapsed = long_run.join().unwrap();
        assert!(elapsed >= Duration::from_secs(2), "{elapsed:?}");
    });
}

fn tiny_memory() -> RunLimits {
    RunLimits {
        memory_bytes: 32 * MB,
        ..roomy_limits()
    }
}

#[test]
fn the_next_run_works_after_hitting_a_limit() {
    within_a_minute(|| {
        let fixture = Fixture::file_agent();
        fixture.ok("write state.txt saved");
        assert_eq!(
            fixture.run_limited("loop-forever", &short_time()),
            RunOutcome::TimeLimit
        );
        fixture.ok("expect state.txt saved");
        assert_eq!(
            fixture.run_limited("allocate-forever", &tiny_memory()),
            RunOutcome::MemoryLimit
        );
        fixture.ok("expect state.txt saved");
        fixture.ok("allocate 32");
    });
}

#[test]
fn an_allocation_loop_fails_without_harming_the_host() {
    within_a_minute(|| {
        let fixture = Fixture::file_agent();
        assert_eq!(
            fixture.run_limited("allocate-forever", &tiny_memory()),
            RunOutcome::MemoryLimit
        );
    });
}

#[test]
fn memory_within_the_limit_can_be_used() {
    let fixture = Fixture::file_agent();
    assert_eq!(
        fixture.run_limited("allocate 16", &tiny_memory()),
        RunOutcome::Completed
    );
}

#[test]
fn allocating_more_than_the_limit_fails() {
    let fixture = Fixture::file_agent();
    assert_eq!(
        fixture.run_limited("allocate 64", &tiny_memory()),
        RunOutcome::MemoryLimit
    );
}

#[test]
fn an_agent_can_notice_that_its_out_of_memory_and_carry_on() {
    let fixture = Fixture::file_agent();
    assert_eq!(
        fixture.run_limited("try-allocate-too-much", &tiny_memory()),
        RunOutcome::Completed
    );
}

fn one_mb_of_memory() -> RunLimits {
    RunLimits {
        memory_bytes: MB,
        ..roomy_limits()
    }
}

#[test]
fn growing_memory_directly_stops_at_the_limit() {
    // 16 pages make 1 MB. The module starts with one page, so growing by 15 fits exactly.
    let body = "(if (i32.ne (memory.grow (i32.const 15)) (i32.const 1)) (then unreachable)) \
                (if (i32.ne (memory.grow (i32.const 1)) (i32.const -1)) (then unreachable)) \
                (if (i32.ne (memory.size) (i32.const 16)) (then unreachable)) \
                i32.const 0";
    let fixture = Fixture::new(component_exporting(body, ""));
    assert_eq!(
        fixture.run_limited("x", &one_mb_of_memory()),
        RunOutcome::Completed
    );
}

#[test]
fn a_memory_too_large_to_start_with_is_reported() {
    let fixture = Fixture::new(component_with_memory_pages(17));
    assert_eq!(
        fixture.run_limited("x", &one_mb_of_memory()),
        RunOutcome::MemoryLimit
    );

    let fixture = Fixture::new(component_with_memory_pages(16));
    assert_eq!(
        fixture.run_limited("x", &one_mb_of_memory()),
        RunOutcome::Completed
    );
}

#[test]
fn several_memories_share_one_limit() {
    // Each of these fits alone, but not together.
    let fixture = Fixture::new(component_exporting("i32.const 0", "(memory 16)"));
    assert_eq!(
        fixture.run_limited("x", &one_mb_of_memory()),
        RunOutcome::MemoryLimit
    );
}

#[test]
fn large_tables_count_towards_the_limit() {
    // 5 million elements take 40 MB, because each counts for 8 bytes.
    let table = "(table 5000000 funcref)";
    let fixture = Fixture::past_the_compile_limits(component_exporting("i32.const 0", table));
    assert_eq!(
        fixture.run_limited("x", &tiny_memory()),
        RunOutcome::MemoryLimit
    );
}

#[test]
fn growing_a_table_counts_towards_the_limit() {
    let must_grow = "(if (i32.eq (table.grow $t (ref.null func) (i32.const 5000000)) \
                     (i32.const -1)) (then unreachable)) i32.const 0";
    let fixture = Fixture::new(component_exporting(must_grow, "(table $t 1 funcref)"));
    assert_eq!(
        fixture.run_limited("x", &tiny_memory()),
        RunOutcome::MemoryLimit
    );
}

/// An agent that also creates `count` instances of a module holding `contents`.
fn component_with_instances(contents: &str, count: usize) -> Vec<u8> {
    let instances = "(core instance (instantiate $extra))\n".repeat(count);
    wat::parse_str(format!(
        r#"(component
            (core module $extra {contents})
            {instances}
            (core module $main
                (memory (export "memory") 1)
                (func (export "handle-event") (param i32 i32 i32) (result i32) i32.const 0)
                {REALLOC}
            )
            {MAIN_INSTANCE_AND_EXPORT}
        )"#
    ))
    .unwrap()
}

/// Checks that a component runs when it's under a cap, and is refused when it's over.
fn expect_cap(under: Vec<u8>, over: Vec<u8>) {
    assert_eq!(
        Fixture::past_the_compile_limits(under).run("x"),
        RunOutcome::Completed
    );
    let outcome = Fixture::past_the_compile_limits(over).run("x");
    assert!(matches!(outcome, RunOutcome::Crashed(_)), "{outcome:?}");
}

#[test]
fn too_many_instances_are_refused() {
    expect_cap(
        component_with_instances("", 50),
        component_with_instances("", 200),
    );
}

#[test]
fn too_many_memories_are_refused() {
    // One module can only hold so many, so this takes several instances.
    let memories = "(memory 0)".repeat(40);
    expect_cap(
        component_with_instances(&memories, 2),
        component_with_instances(&memories, 3),
    );
}

#[test]
fn too_many_tables_are_refused() {
    let tables = "(table 0 funcref)".repeat(40);
    expect_cap(
        component_with_instances(&tables, 2),
        component_with_instances(&tables, 3),
    );
}

#[test]
fn endless_recursion_is_reported_without_harming_the_host() {
    let code = component_exporting(
        "(call $forever) i32.const 0",
        "(func $forever (call $forever))",
    );
    let fixture = Fixture::new(code);
    let RunOutcome::Crashed(reason) = fixture.run("x") else {
        panic!("the crash wasn't reported");
    };
    assert!(reason.contains("call stack"), "{reason}");
}

#[test]
fn a_nonsense_return_value_is_reported_as_a_crash() {
    // The result is a pointer into memory, which is far past the end here.
    let fixture = Fixture::new(component_exporting("i32.const 2000000", ""));
    let outcome = fixture.run("x");
    assert!(matches!(outcome, RunOutcome::Crashed(_)), "{outcome:?}");
}

#[test]
fn a_manifest_cant_raise_its_limits_above_the_hosts() {
    const GREEDY: &str = "[agent]\nname = \"file-agent\"\nversion = \"0.1.0\"\n\n[limits]\nmemory_mb = 4096\nrun_seconds = 3600\ndisk_mb = 10240\n";
    let host = HostLimits {
        memory_mb: 32,
        run_seconds: 1,
        disk_mb: 1,
    };
    let package = Package::new(GREEDY, file_agent_code().to_vec()).unwrap();
    let limits = host.limits_for(&package.manifest().limits());

    within_a_minute(move || {
        let fixture = Fixture::file_agent();
        let start = Instant::now();
        assert_eq!(
            fixture.run_limited("loop-forever", &limits),
            RunOutcome::TimeLimit
        );
        assert!(start.elapsed() < Duration::from_secs(10));
        assert_eq!(
            fixture.run_limited("allocate-forever", &limits),
            RunOutcome::MemoryLimit
        );

        let small_home = fixture.storage.home(limits.disk_bytes).unwrap();
        let too_big = format!("expect-big big.bin {}", 2 * MB);
        let outcome = fixture.agent.run(
            &Event::UserMessage(too_big),
            small_home,
            &limits,
            &StopSignal::default(),
            Box::new(DenyEverything),
        );
        let RunOutcome::Failed(message) = outcome else {
            panic!("the disk limit wasn't applied: {outcome:?}");
        };
        assert!(message.contains("disk limit"), "{message}");
    });
}
