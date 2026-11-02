//! Throwaway library that checks and measures how Wasmtime behaves inside an Android app.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{fs, mem, thread};

use jni::objects::{JClass, JString};
use jni::{EnvUnowned, errors::ThrowRuntimeExAndDefault};
use wasmtime::component::{Component, ComponentNamedList, Lift, Linker, Lower};
use wasmtime::{Config, Engine, Result, Store, StoreContextMut, bail};

macro_rules! guest_file {
    ($name:literal) => {
        include_bytes!(concat!(env!("OUT_DIR"), "/", $name))
    };
}

const BUILD_ID: &str = env!("GUEST_BUILD_ID");

const INPUT: i32 = 20;
const EXPECTED: i32 = 41;

/// Rounds of the CPU workload, and the hash they must produce in every mode.
const CPU_ROUNDS: i32 = 10_000;
const CPU_EXPECTED: i32 = 98945954;
const WAIT_COUNT: i32 = 10;
const WAIT_MS: i32 = 100;
const WARM_RUNS: usize = 101;

/// The ways to compile and load a guest component.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Cranelift compiles to machine code on the device.
    CraneliftOnDevice,
    /// Machine code compiled at build time.
    CraneliftPrecompiled,
    /// Cranelift compiles to Pulley bytecode on the device.
    PulleyOnDevice,
    /// Pulley bytecode compiled at build time.
    PulleyPrecompiled,
}

pub const MODES: [Mode; 4] = [
    Mode::CraneliftOnDevice,
    Mode::CraneliftPrecompiled,
    Mode::PulleyOnDevice,
    Mode::PulleyPrecompiled,
];

/// The test agents. Both have the same exports, but `Large` also contains unused code.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Guest {
    Hello,
    Large,
}

pub const GUESTS: [Guest; 2] = [Guest::Hello, Guest::Large];

fn guest_bytes(guest: Guest, mode: Mode) -> &'static [u8] {
    match (guest, mode) {
        (Guest::Hello, Mode::CraneliftOnDevice | Mode::PulleyOnDevice) => {
            guest_file!("hello.wasm")
        }
        (Guest::Hello, Mode::CraneliftPrecompiled) => guest_file!("hello.native.cwasm"),
        (Guest::Hello, Mode::PulleyPrecompiled) => guest_file!("hello.pulley.cwasm"),
        (Guest::Large, Mode::CraneliftOnDevice | Mode::PulleyOnDevice) => {
            guest_file!("large.wasm")
        }
        (Guest::Large, Mode::CraneliftPrecompiled) => guest_file!("large.native.cwasm"),
        (Guest::Large, Mode::PulleyPrecompiled) => guest_file!("large.pulley.cwasm"),
    }
}

fn guest_path(dir: &Path, guest: Guest, mode: Mode) -> PathBuf {
    let kind = match mode {
        Mode::CraneliftOnDevice | Mode::PulleyOnDevice => "wasm",
        Mode::CraneliftPrecompiled => "native",
        Mode::PulleyPrecompiled => "pulley",
    };
    dir.join(format!("{guest:?}-{kind}-{BUILD_ID}"))
}

/// Writes any missing guest files to the app's private storage. Guests load from files, as real
/// agents would, so memory use doesn't include the copies embedded in this library.
pub fn prepare_guests(dir: &Path) -> Result<()> {
    for guest in GUESTS {
        for mode in MODES {
            let path = guest_path(dir, guest, mode);
            if !path.exists() {
                let partial = path.with_extension("partial");
                fs::write(&partial, guest_bytes(guest, mode))?;
                fs::rename(&partial, &path)?;
            }
        }
    }
    Ok(())
}

fn load(dir: &Path, guest: Guest, mode: Mode) -> Result<(Engine, Component)> {
    let mut config = Config::new();
    if matches!(mode, Mode::PulleyOnDevice | Mode::PulleyPrecompiled) {
        config.target("pulley64")?;
    }
    let engine = Engine::new(&config)?;
    let path = guest_path(dir, guest, mode);
    let component = match mode {
        Mode::CraneliftOnDevice | Mode::PulleyOnDevice => Component::from_file(&engine, &path)?,
        // SAFETY: prepare_guests wrote this file to private storage, from bytes that build.rs
        // produced with this exact Wasmtime version. Precompiled code must never be loaded from
        // an untrusted source.
        Mode::CraneliftPrecompiled | Mode::PulleyPrecompiled => unsafe {
            Component::deserialize_file(&engine, &path)?
        },
    };
    Ok((engine, component))
}

/// The host functions. The store's data counts the calls to `double`.
fn host_linker(engine: &Engine) -> Result<Linker<u32>> {
    let mut linker = Linker::<u32>::new(engine);
    linker.root().func_wrap(
        "double",
        |mut context: StoreContextMut<'_, u32>, (x,): (i32,)| {
            *context.data_mut() += 1;
            Ok((x * 2,))
        },
    )?;
    linker
        .root()
        .func_wrap("wait", |_: StoreContextMut<'_, u32>, (ms,): (i32,)| {
            thread::sleep(Duration::from_millis(ms as u64));
            Ok(())
        })?;
    Ok(linker)
}

/// Creates a new instance and calls one export. Returns the result and the `double` call count.
fn call<Params, Results>(
    linker: &Linker<u32>,
    component: &Component,
    name: &str,
    params: Params,
) -> Result<(Results, u32)>
where
    Params: ComponentNamedList + Lower,
    Results: ComponentNamedList + Lift,
{
    let mut store = Store::new(linker.engine(), 0);
    let instance = linker.instantiate(&mut store, component)?;
    let func = instance.get_typed_func::<Params, Results>(&mut store, name)?;
    let results = func.call(&mut store, params)?;
    Ok((results, *store.data()))
}

fn call_run(linker: &Linker<u32>, component: &Component) -> Result<Duration> {
    let start = Instant::now();
    let ((result,), host_calls) = call::<(i32,), (i32,)>(linker, component, "run", (INPUT,))?;
    let elapsed = start.elapsed();
    if result != EXPECTED || host_calls != 1 {
        bail!("run({INPUT}) returned {result} with {host_calls} host calls");
    }
    Ok(elapsed)
}

/// Confirms that an out-of-bounds read traps and comes back as an error.
fn expect_trap(linker: &Linker<u32>, component: &Component) -> Result<()> {
    match call::<(), (i32,)>(linker, component, "read-out-of-bounds", ()) {
        Ok(((value,), _)) => bail!("out-of-bounds read returned {value} instead of trapping"),
        Err(_) => Ok(()),
    }
}

/// Runs every check for one mode and returns a description of the result.
pub fn check_mode(dir: &Path, mode: Mode) -> Result<String> {
    prepare_guests(dir)?;
    let start = Instant::now();
    let (engine, component) = load(dir, Guest::Hello, mode)?;
    let loaded = start.elapsed();

    let linker = host_linker(&engine)?;
    let ran = call_run(&linker, &component)?;

    expect_trap(&linker, &component)?;

    // The engine must keep working after a trap.
    call_run(&linker, &component)?;

    Ok(format!(
        "run({INPUT})={EXPECTED}, host called, trap caught, load {} us, run {} us",
        loaded.as_micros(),
        ran.as_micros()
    ))
}

/// Runs every mode, with one line per mode. Lines start with PASS or FAIL.
pub fn run_checks(dir: &Path) -> String {
    MODES
        .iter()
        .map(|&mode| {
            let outcome = std::panic::catch_unwind(|| check_mode(dir, mode));
            match outcome {
                Ok(Ok(details)) => format!("PASS {mode:?}: {details}"),
                Ok(Err(error)) => format!("FAIL {mode:?}: {error:#}"),
                Err(_) => format!("FAIL {mode:?}: panicked"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The highest resident memory of this process so far, in KB.
fn peak_memory_kb() -> Result<u64> {
    // SAFETY: getrusage only writes to the zeroed struct it's given.
    let usage = unsafe {
        let mut usage: libc::rusage = mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            bail!("getrusage failed");
        }
        usage
    };
    let peak = usage.ru_maxrss as u64;
    Ok(if cfg!(target_os = "macos") {
        peak / 1024
    } else {
        peak
    })
}

/// The CPU time this thread has used so far.
fn thread_cpu_time() -> Result<Duration> {
    // SAFETY: clock_gettime only writes to the zeroed struct it's given.
    let time = unsafe {
        let mut time: libc::timespec = mem::zeroed();
        if libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) != 0 {
            bail!("clock_gettime failed");
        }
        time
    };
    Ok(Duration::new(time.tv_sec as u64, time.tv_nsec as u32))
}

/// Returns the elapsed time and thread CPU time of `work`, and its result.
fn timed<T>(work: impl FnOnce() -> Result<T>) -> Result<(Duration, Duration, T)> {
    let cpu_start = thread_cpu_time()?;
    let start = Instant::now();
    let result = work()?;
    let elapsed = start.elapsed();
    Ok((elapsed, thread_cpu_time()? - cpu_start, result))
}

/// Measures one guest in one mode. Each measurement needs a new process, because the peak memory
/// figure can't be reset. Returns one comma-separated row: see the header in `measure.sh`.
pub fn measure(dir: &Path, mode: Mode, guest: Guest, app_ms: i64) -> Result<String> {
    prepare_guests(dir)?;
    let base_kb = peak_memory_kb()?;

    let start = Instant::now();
    let (engine, component) = load(dir, guest, mode)?;
    let linker = host_linker(&engine)?;
    call_run(&linker, &component)?;
    let cold = start.elapsed();

    let mut warm = (0..WARM_RUNS)
        .map(|_| call_run(&linker, &component))
        .collect::<Result<Vec<_>>>()?;
    warm.sort();
    let warm = warm[WARM_RUNS / 2];

    let (cpu_elapsed, cpu_time, ((hash,), _)) =
        timed(|| call::<(i32,), (i32,)>(&linker, &component, "cpu", (CPU_ROUNDS,)))?;
    if hash != CPU_EXPECTED {
        bail!("cpu({CPU_ROUNDS}) returned {hash}, expected {CPU_EXPECTED}");
    }

    let (wait_elapsed, wait_time, _) = timed(|| {
        call::<(i32, i32), (i32,)>(&linker, &component, "wait-loop", (WAIT_COUNT, WAIT_MS))
    })?;

    let peak_kb = peak_memory_kb()?;
    let file_kb = fs::metadata(guest_path(dir, guest, mode))?
        .len()
        .div_ceil(1024);
    Ok(format!(
        "{mode:?},{guest:?},{file_kb},{app_ms},{},{},{base_kb},{peak_kb},{},{},{},{}",
        cold.as_micros(),
        warm.as_micros(),
        cpu_elapsed.as_micros(),
        cpu_time.as_micros(),
        wait_elapsed.as_micros(),
        wait_time.as_micros(),
    ))
}

fn parse_mode(name: &str) -> Option<Mode> {
    MODES.into_iter().find(|mode| format!("{mode:?}") == name)
}

fn parse_guest(name: &str) -> Option<Guest> {
    GUESTS
        .into_iter()
        .find(|guest| format!("{guest:?}") == name)
}

/// Runs one measurement. The line starts with MEASURE or FAIL.
pub fn measure_report(dir: &Path, mode: &str, guest: &str, app_ms: i64) -> String {
    let (Some(mode), Some(guest)) = (parse_mode(mode), parse_guest(guest)) else {
        return format!("FAIL unknown mode or guest: {mode} {guest}");
    };
    match std::panic::catch_unwind(|| measure(dir, mode, guest, app_ms)) {
        Ok(Ok(row)) => format!("MEASURE,{row}"),
        Ok(Err(error)) => format!("FAIL {mode:?} {guest:?}: {error:#}"),
        Err(_) => format!("FAIL {mode:?} {guest:?}: panicked"),
    }
}

/// Called from `io.github.kashmithnisakya.habitat.feasibility.NativeBridge.runChecks`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_kashmithnisakya_habitat_feasibility_NativeBridge_runChecks<
    'caller,
>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    dir: JString<'caller>,
) -> JString<'caller> {
    env.with_env(|env| {
        let dir = dir.try_to_string(env)?;
        JString::from_str(env, run_checks(Path::new(&dir)))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

/// Called from `io.github.kashmithnisakya.habitat.feasibility.NativeBridge.measure`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_kashmithnisakya_habitat_feasibility_NativeBridge_measure<
    'caller,
>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    dir: JString<'caller>,
    mode: JString<'caller>,
    guest: JString<'caller>,
    app_ms: i64,
) -> JString<'caller> {
    env.with_env(|env| {
        let dir = dir.try_to_string(env)?;
        let mode = mode.try_to_string(env)?;
        let guest = guest.try_to_string(env)?;
        JString::from_str(env, measure_report(Path::new(&dir), &mode, &guest, app_ms))
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("habitat-feasibility-{name}-{BUILD_ID}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn every_mode_passes() {
        let dir = test_dir("modes");
        for mode in MODES {
            check_mode(&dir, mode).unwrap_or_else(|error| panic!("{mode:?}: {error:#}"));
        }
    }

    #[test]
    fn report_has_one_passing_line_per_mode() {
        let report = run_checks(&test_dir("report"));
        assert_eq!(report.lines().count(), MODES.len());
        assert!(
            report.lines().all(|line| line.starts_with("PASS ")),
            "{report}"
        );
    }

    #[test]
    fn precompiled_code_for_the_wrong_engine_is_rejected() {
        let engine = Engine::default();
        let result = unsafe {
            Component::deserialize(&engine, guest_bytes(Guest::Hello, Mode::PulleyPrecompiled))
        };
        assert!(result.is_err());
    }

    #[test]
    fn every_guest_and_mode_can_be_measured() {
        let dir = test_dir("measure");
        for guest in GUESTS {
            for mode in MODES {
                let row = measure_report(&dir, &format!("{mode:?}"), &format!("{guest:?}"), 1);
                assert!(row.starts_with("MEASURE,"), "{row}");
                assert_eq!(row.split(',').count(), 13, "{row}");
            }
        }
    }

    #[test]
    fn unknown_names_are_rejected() {
        let report = measure_report(&test_dir("names"), "Nope", "Hello", 1);
        assert!(report.starts_with("FAIL "), "{report}");
    }

    #[test]
    fn large_guest_is_a_few_mb() {
        let size = guest_bytes(Guest::Large, Mode::CraneliftOnDevice).len();
        assert!((1_500_000..3_000_000).contains(&size), "{size} bytes");
    }
}
