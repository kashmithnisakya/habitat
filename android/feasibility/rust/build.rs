//! Builds the guest components, and precompiles them for the target CPU and for Pulley.

use std::fmt::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs};

use wasmtime::{Config, Engine, Result};

const FILLER_MARKER: &str = ";; @filler";
/// Gives the larger guest about 2.7 MB of WebAssembly.
const FILLER_FUNCTIONS: usize = 8000;

/// Generates unused functions of varied length, so the larger guest has more code to compile.
fn filler() -> String {
    let mut text = String::new();
    for function in 0..FILLER_FUNCTIONS {
        text.push_str("(func (param i32) (result i32) (local i32) local.get 0 local.set 1\n");
        for step in 0..(4 + function % 9) {
            let add = function * 31 + step;
            let multiply = function * 2 + 1;
            writeln!(
                text,
                "local.get 1 i32.const {add} i32.add local.tee 1 i32.const 65532 i32.and i32.load \
                 local.get 1 i32.xor i32.const {multiply} i32.mul local.set 1 \
                 local.get 1 i32.const 0 i32.lt_s \
                 (if (then local.get 1 i32.const 1 i32.shr_u local.set 1))"
            )
            .unwrap();
        }
        text.push_str("local.get 1)\n");
    }
    text
}

fn precompile(target: &str, guest: &[u8], output: &Path) -> Result<()> {
    let mut config = Config::new();
    config.target(target)?;
    let compiled = Engine::new(&config)?.precompile_component(guest)?;
    fs::write(output, compiled)?;
    Ok(())
}

fn build_guest(name: &str, text: &str, out_dir: &Path, target: &str) -> Result<()> {
    let guest = wat::parse_str(text)?;
    fs::write(out_dir.join(format!("{name}.wasm")), &guest)?;
    precompile(
        target,
        &guest,
        &out_dir.join(format!("{name}.native.cwasm")),
    )?;
    precompile(
        "pulley64",
        &guest,
        &out_dir.join(format!("{name}.pulley.cwasm")),
    )?;
    Ok(())
}

fn main() -> Result<()> {
    println!("cargo:rerun-if-changed=guest.wat");
    println!("cargo:rerun-if-changed=build.rs");

    let hello = fs::read_to_string("guest.wat")?;
    assert!(hello.contains(FILLER_MARKER));
    let large = hello.replace(FILLER_MARKER, &filler());

    let out_dir = env::var("OUT_DIR")?;
    let out_dir = Path::new(&out_dir);
    let target = env::var("TARGET")?;
    build_guest("hello", &hello, out_dir, &target)?;
    build_guest("large", &large, out_dir, &target)?;

    // Names the files the app extracts, so a new build never loads an old file.
    let build_id = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    println!("cargo:rustc-env=GUEST_BUILD_ID={build_id}");
    Ok(())
}
