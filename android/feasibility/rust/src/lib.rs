//! Throwaway library that checks how Wasmtime behaves inside an Android app.

use std::time::Instant;

use jni::objects::{JClass, JString};
use jni::{EnvUnowned, errors::ThrowRuntimeExAndDefault};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Result, Store, StoreContextMut, bail};

const GUEST_WAT: &str = include_str!("../guest.wat");
const GUEST_NATIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/guest.native.cwasm"));
const GUEST_PULLEY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/guest.pulley.cwasm"));

const INPUT: i32 = 20;
const EXPECTED: i32 = 41;

/// The ways to compile and load the guest component.
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

fn load(mode: Mode) -> Result<(Engine, Component)> {
    let mut config = Config::new();
    if matches!(mode, Mode::PulleyOnDevice | Mode::PulleyPrecompiled) {
        config.target("pulley64")?;
    }
    let engine = Engine::new(&config)?;
    let component = match mode {
        Mode::CraneliftOnDevice | Mode::PulleyOnDevice => Component::new(&engine, GUEST_WAT)?,
        // SAFETY: these bytes were produced by build.rs with this exact Wasmtime version.
        // Precompiled code must never be loaded from an untrusted source.
        Mode::CraneliftPrecompiled => unsafe { Component::deserialize(&engine, GUEST_NATIVE)? },
        Mode::PulleyPrecompiled => unsafe { Component::deserialize(&engine, GUEST_PULLEY)? },
    };
    Ok((engine, component))
}

/// Calls the guest, which calls back into the host. Returns the result and the host call count.
fn call_run(engine: &Engine, component: &Component, input: i32) -> Result<(i32, u32)> {
    let mut linker = Linker::<u32>::new(engine);
    linker.root().func_wrap(
        "double",
        |mut context: StoreContextMut<'_, u32>, (x,): (i32,)| {
            *context.data_mut() += 1;
            Ok((x * 2,))
        },
    )?;
    let mut store = Store::new(engine, 0);
    let instance = linker.instantiate(&mut store, component)?;
    let run = instance.get_typed_func::<(i32,), (i32,)>(&mut store, "run")?;
    let (result,) = run.call(&mut store, (input,))?;
    Ok((result, *store.data()))
}

/// Confirms that an out-of-bounds read traps and comes back as an error.
fn expect_trap(engine: &Engine, component: &Component) -> Result<()> {
    let mut linker = Linker::<u32>::new(engine);
    linker
        .root()
        .func_wrap("double", |_: StoreContextMut<'_, u32>, (x,): (i32,)| {
            Ok((x * 2,))
        })?;
    let mut store = Store::new(engine, 0);
    let instance = linker.instantiate(&mut store, component)?;
    let read = instance.get_typed_func::<(), (i32,)>(&mut store, "read-out-of-bounds")?;
    match read.call(&mut store, ()) {
        Ok((value,)) => bail!("out-of-bounds read returned {value} instead of trapping"),
        Err(_) => Ok(()),
    }
}

/// Runs every check for one mode and returns a description of the result.
pub fn check_mode(mode: Mode) -> Result<String> {
    let start = Instant::now();
    let (engine, component) = load(mode)?;
    let loaded = start.elapsed();

    let start = Instant::now();
    let (result, host_calls) = call_run(&engine, &component, INPUT)?;
    let ran = start.elapsed();
    if result != EXPECTED || host_calls != 1 {
        bail!("run({INPUT}) returned {result} with {host_calls} host calls");
    }

    expect_trap(&engine, &component)?;

    // The engine must keep working after a trap.
    let (result, _) = call_run(&engine, &component, INPUT)?;
    if result != EXPECTED {
        bail!("run({INPUT}) returned {result} after a trap");
    }

    Ok(format!(
        "run({INPUT})={result}, host called, trap caught, load {} us, run {} us",
        loaded.as_micros(),
        ran.as_micros()
    ))
}

/// Runs every mode, with one line per mode. Lines start with PASS or FAIL.
pub fn run_checks() -> String {
    MODES
        .iter()
        .map(|&mode| {
            let outcome = std::panic::catch_unwind(|| check_mode(mode));
            match outcome {
                Ok(Ok(details)) => format!("PASS {mode:?}: {details}"),
                Ok(Err(error)) => format!("FAIL {mode:?}: {error:#}"),
                Err(_) => format!("FAIL {mode:?}: panicked"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Called from `io.github.kashmithnisakya.habitat.feasibility.NativeBridge.runChecks`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_kashmithnisakya_habitat_feasibility_NativeBridge_runChecks<
    'caller,
>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    env.with_env(|env| JString::from_str(env, run_checks()))
        .resolve::<ThrowRuntimeExAndDefault>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_passes() {
        for mode in MODES {
            check_mode(mode).unwrap_or_else(|error| panic!("{mode:?}: {error:#}"));
        }
    }

    #[test]
    fn report_has_one_passing_line_per_mode() {
        let report = run_checks();
        assert_eq!(report.lines().count(), MODES.len());
        assert!(
            report.lines().all(|line| line.starts_with("PASS ")),
            "{report}"
        );
    }

    #[test]
    fn precompiled_code_for_the_wrong_engine_is_rejected() {
        let engine = Engine::default();
        let result = unsafe { Component::deserialize(&engine, GUEST_PULLEY) };
        assert!(result.is_err());
    }
}
