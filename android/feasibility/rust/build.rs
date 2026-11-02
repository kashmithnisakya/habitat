//! Precompiles the guest component for the target CPU and for the Pulley interpreter.

use std::path::Path;
use std::{env, fs};

use wasmtime::{Config, Engine, Result};

fn precompile(target: &str, guest: &[u8], output: &Path) -> Result<()> {
    let mut config = Config::new();
    config.target(target)?;
    let compiled = Engine::new(&config)?.precompile_component(guest)?;
    fs::write(output, compiled)?;
    Ok(())
}

fn main() -> Result<()> {
    println!("cargo:rerun-if-changed=guest.wat");
    println!("cargo:rerun-if-changed=build.rs");

    let guest = wat::parse_file("guest.wat")?;
    let out_dir = env::var("OUT_DIR")?;
    let target = env::var("TARGET")?;
    precompile(
        &target,
        &guest,
        &Path::new(&out_dir).join("guest.native.cwasm"),
    )?;
    precompile(
        "pulley64",
        &guest,
        &Path::new(&out_dir).join("guest.pulley.cwasm"),
    )?;
    Ok(())
}
