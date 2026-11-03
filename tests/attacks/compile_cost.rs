//! Hostile packages that try to make compiling them cost the host too much. Each one has to be
//! refused, with the host still working afterwards.
//!
//! Every load runs under a time limit of its own here, so that a broken check fails a test rather
//! than leaving it hanging. See `docs/compile-costs.md` for what each of these costs when it isn't
//! refused. Their sizes are kept near each limit, so that even then they can't do much harm.

#[path = "support/hostile_code.rs"]
mod hostile_code;

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use habitat_backend_wasm::WasmBackend;
use habitat_backend_wasm::compile_limits::CompileLimits;
use habitat_core::backend::{Backend, Event, LoadError, RunLimits, RunOutcome, StopSignal};
use habitat_core::home::AgentStorage;
use habitat_core::package::Package;
use hostile_code::*;
use tempfile::TempDir;

const MANIFEST: &str = "[agent]\nname = \"hostile\"\nversion = \"0.1.0\"\n\n[limits]\nmemory_mb = 64\nrun_seconds = 10\ndisk_mb = 1\n";
const MB: usize = 1024 * 1024;

/// Longer than any of these should take, even in a slow debug build on a busy machine.
const TEST_TIME_LIMIT: Duration = Duration::from_secs(300);

/// Refusing a package means reading it, not compiling it, which should be over quickly. This allows
/// for a slow debug build on a busy machine, as compiling the largest of these would take far longer.
const REFUSAL_TIME_LIMIT: Duration = Duration::from_secs(60);

/// Runs something on another thread and fails the test if that takes too long, because a call that
/// never returned wouldn't fail on its own. The thread is left behind, like a hung compile.
fn within<T: Send + 'static>(limit: Duration, work: impl FnOnce() -> T + Send + 'static) -> T {
    let (result, finished) = mpsc::channel();
    thread::spawn(move || {
        let _ = result.send(work());
    });
    match finished.recv_timeout(limit) {
        Ok(value) => value,
        Err(error) => panic!("no result within {limit:?}: {error}"),
    }
}

fn package(code: Vec<u8>) -> Package {
    Package::new(MANIFEST, code).unwrap()
}

/// An ordinary agent that does nothing, to show that the host still works.
fn harmless_agent() -> Package {
    let code = wat::parse_str(
        r#"(component
            (core module $main
                (memory (export "memory") 1)
                (func (export "handle-event") (param i32 i32 i32) (result i32) i32.const 0)
                (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) i32.const 1024))
            (core instance $i (instantiate $main))
            (alias core export $i "memory" (core memory $memory))
            (type $event (variant (case "user-message" string) (case "scheduled" string)))
            (export $exported "event" (type $event))
            (func (export "handle-event") (param "event" $exported) (result (result (error string)))
                (canon lift (core func $i "handle-event")
                    (memory $memory)
                    (realloc (core func $i "cabi_realloc"))
                    string-encoding=utf8)))"#,
    )
    .unwrap();
    package(code)
}

fn assert_runs(backend: &WasmBackend) {
    let agent = backend.load(&harmless_agent()).unwrap();
    let root = TempDir::new().unwrap();
    let home = AgentStorage::open(root.path(), "hostile")
        .unwrap()
        .home(1024 * 1024)
        .unwrap();
    let limits = RunLimits {
        memory_bytes: 64 * MB as u64,
        run_time: Duration::from_secs(60),
        disk_bytes: 1024 * 1024,
    };
    let outcome = agent.run(
        &Event::UserMessage("hello".to_string()),
        home,
        &limits,
        &StopSignal::default(),
    );
    assert_eq!(outcome, RunOutcome::Completed);
}

/// Checks that a component is refused for the expected reason without being compiled, then that
/// the same backend can load and run an agent straight afterwards.
fn assert_refused(code: Vec<u8>, reason: &'static str) {
    let package = package(code);
    within(TEST_TIME_LIMIT, move || {
        let backend = WasmBackend::new().unwrap();
        let start = Instant::now();
        let error = match backend.load(&package) {
            Ok(_) => panic!("the component loaded"),
            Err(error) => error,
        };
        let LoadError::TooCostly(message) = &error else {
            panic!("the wrong error: {error:?}");
        };
        assert!(message.contains(reason), "{message}");
        assert!(
            start.elapsed() < REFUSAL_TIME_LIMIT,
            "that took {:?}, so it was probably compiled",
            start.elapsed()
        );

        let printed = error.to_string();
        assert!(printed.is_ascii(), "{printed:?}");
        assert!(!printed.chars().any(char::is_control), "{printed:?}");

        assert_runs(&backend);
    });
}

#[test]
fn a_tiny_instantiation_bomb_is_refused() {
    // Two more levels of this would take gigabytes, and a few more would have no end, but this
    // has to stay safe to run if the check is ever broken.
    let bomb = instantiation_bomb(6, 8);
    assert!(bomb.len() < 4096, "{}", bomb.len());
    assert_refused(bomb, "nested components");
}

#[test]
fn a_single_nested_component_is_refused() {
    assert_refused(nested_components(1), "nested components");
    assert_refused(instantiation_bomb(1, 1), "nested components");
}

#[test]
fn too_much_code_is_refused() {
    assert_refused(arithmetic(17 * MB, 64 * 1024), "bytes of code");
}

#[test]
fn a_package_of_the_largest_size_is_refused_if_it_is_all_code() {
    // This much takes minutes to compile, even as simple as it is.
    assert_refused(arithmetic(127 * MB, MB / 8), "bytes of code");
}

#[test]
fn a_very_large_function_is_refused() {
    assert_refused(arithmetic(MB, MB), "bytes in the largest function");
    assert_refused(nested_blocks(350_000), "bytes in the largest function");
}

#[test]
fn too_many_functions_are_refused() {
    assert_refused(tiny_functions(100_001), "functions");
}

#[test]
fn too_many_modules_are_refused() {
    assert_refused(many_modules(257), "modules");
}

#[test]
fn many_loops_in_a_function_are_refused() {
    assert_refused(control_with_params(0, 1025, 0), "loops in one function");
}

#[test]
fn deeply_nested_loops_are_refused() {
    assert_refused(loops_times_locals(17, 1), "nested loops");
}

#[test]
fn many_branches_in_a_function_are_refused() {
    assert_refused(control_with_params(1, 64_001, 0), "branch points");
    assert_refused(br_table(64_001, 1), "branch points");
}

#[test]
fn values_tracked_through_many_branches_are_refused() {
    assert_refused(blocks_times_locals(1000, 1000), "units of work");
    assert_refused(blocks_times_stack(2000, 1000), "units of work");
    assert_refused(flat_loops_times_locals(1, 800, 800, 16), "units of work");
}

#[test]
fn values_passed_through_many_blocks_are_refused() {
    for kind in 0..3 {
        assert_refused(control_with_params(kind, 2, 1000), "passed through blocks");
    }
    assert_refused(control_with_params(0, 1000, 2), "passed through blocks");
    assert_refused(blocks_with_a_result(1025), "passed through blocks");
}

#[test]
fn work_adds_up_across_functions() {
    assert_refused(functions_with_locals(500, 50_000), "units of work");
    assert_refused(flat_loops_times_locals(20, 350, 350, 16), "units of work");
}

#[test]
fn many_sparse_memory_images_are_refused() {
    assert_refused(sparse_memory_images(1, 17, 15 * MB as u32), "memories");
}

#[test]
fn many_large_tables_are_refused() {
    assert_refused(sparse_tables(1, 2, 1_000_000), "table entries");
}

#[test]
fn many_instances_of_a_module_with_many_imports_are_refused() {
    assert_refused(instances_times_imports(257, 16_000, 0), "imports across");
}

#[test]
fn many_adapters_are_refused() {
    assert_refused(many_adapters(1025, 0), "canonical functions");
}

#[test]
fn many_declarations_are_refused() {
    assert_refused(many_declarations(40_000), "declarations");
}

#[test]
fn a_few_very_large_declarations_are_refused() {
    assert_refused(huge_component_types(10, 100_000), "bytes of declarations");
}

#[test]
fn glue_code_for_many_values_is_refused() {
    let reason = "in signatures";
    assert_refused(tabled_functions_with_params(600, 1000), reason);
    assert_refused(many_function_types(1001, 1000), reason);
}

#[test]
fn a_cost_is_refused_before_the_code_behind_it_is_read() {
    // Everything after the count of functions may as well be rubbish.
    let mut code = tiny_functions(100_001);
    let start = code.len() - 16;
    code[start..].fill(0xff);
    assert_refused(code, "functions");
}

#[test]
fn a_compile_that_takes_too_long_is_abandoned_and_the_host_keeps_working() {
    // Within every limit but time. It has to take long enough to compile that it's still going
    // during the second load below, which this much code does even in a release build.
    let slow = package(arithmetic(MB / 4, 64 * 1024));
    within(TEST_TIME_LIMIT, move || {
        let mut backend = WasmBackend::new().unwrap();

        // No time at all, so that the result can't depend on how fast this machine is.
        backend.set_compile_limits(CompileLimits {
            time: Duration::ZERO,
            ..CompileLimits::default()
        });
        let error = backend.load(&slow).err().unwrap();
        assert!(matches!(error, LoadError::TooCostly(_)), "{error:?}");
        assert!(error.to_string().contains("limit of 0 ms"), "{error}");

        // Nothing else gets to compile until it's over, which limits what it can cost.
        let error = backend.load(&harmless_agent()).err().unwrap();
        assert_eq!(error, LoadError::Busy);

        // Given long enough, the backend waits for it and then works as normal.
        backend.set_compile_limits(CompileLimits {
            time: TEST_TIME_LIMIT,
            ..CompileLimits::default()
        });
        assert_runs(&backend);
    });
}

#[test]
fn loading_many_times_at_once_compiles_one_at_a_time_and_all_finish() {
    within(TEST_TIME_LIMIT, || {
        let backend = Arc::new(WasmBackend::new().unwrap());
        let loads: Vec<_> = (0..8)
            .map(|_| {
                let backend = Arc::clone(&backend);
                thread::spawn(move || backend.load(&harmless_agent()).is_ok())
            })
            .collect();
        for load in loads {
            assert!(load.join().unwrap());
        }
    });
}
