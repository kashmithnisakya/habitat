//! Measures what compiling hostile components costs the host, through `WasmBackend::load`.
//!
//! Run it with `cargo run --release -p habitat-backend-wasm --example compile_cost -- <max seconds>
//! <max MB> <case> <numbers...>`. Each case is built and then loaded in a process of its own, which
//! is killed if it passes either maximum, so that a compile bomb can't take the machine down with
//! it. The limits are off unless `HABITAT_COMPILE_LIMITS` is set. To see what a real core module
//! holds, use `--shape <file>`. The results are in `docs/compile-costs.md`.

#[path = "../../../tests/attacks/support/hostile_code.rs"]
mod hostile_code;

use std::env;
use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use habitat_backend_wasm::WasmBackend;
use habitat_backend_wasm::code_shape::CodeShape;
use habitat_backend_wasm::compile_limits::CompileLimits;
use habitat_core::backend::Backend;
use habitat_core::package::Package;
use hostile_code::*;

const MANIFEST: &str = "[agent]\nname = \"compile-cost\"\nversion = \"0.1.0\"\n\n[limits]\nmemory_mb = 64\nrun_seconds = 10\ndisk_mb = 1\n";
const MB: usize = 1024 * 1024;

fn build(case: &str, args: &[String]) -> Vec<u8> {
    // A real core module from disk, to compare with.
    if case == "file" {
        return component_holding(&fs::read(&args[0]).unwrap());
    }
    let args: Vec<usize> = args.iter().map(|arg| arg.parse().unwrap()).collect();
    let arg = |index: usize| args[index];
    match case {
        "tiny_functions" => tiny_functions(arg(0)),
        "arithmetic" => arithmetic(arg(0), arg(1)),
        "many_live_values" => many_live_values(arg(0), arg(1)),
        "nested_blocks" => nested_blocks(arg(0)),
        "loops_times_locals" => loops_times_locals(arg(0), arg(1) as u32),
        "blocks_times_locals" => blocks_times_locals(arg(0), arg(1) as u32),
        "blocks_times_stack" => blocks_times_stack(arg(0), arg(1)),
        "control_with_params" => control_with_params(arg(0), arg(1), arg(2)),
        "flat_loops_times_locals" => flat_loops_times_locals(arg(0), arg(1), arg(2) as u32, arg(3)),
        "functions_with_locals" => functions_with_locals(arg(0), arg(1) as u32),
        "exported_functions_with_params" => exported_functions_with_params(arg(0) as u32, arg(1)),
        "many_data_segments" => many_data_segments(arg(0) as u32),
        "tabled_functions_with_params" => tabled_functions_with_params(arg(0) as u32, arg(1)),
        "many_function_types" => many_function_types(arg(0) as u32, arg(1)),
        "huge_component_types" => huge_component_types(arg(0), arg(1) as u32),
        "long_function_names" => long_function_names(arg(0) as u32, arg(1)),
        "br_table" => br_table(arg(0), arg(1) as u32),
        "data_segment" => data_segment(arg(0)),
        "sparse_memory_images" => sparse_memory_images(arg(0), arg(1) as u32, arg(2) as u32),
        "sparse_tables" => sparse_tables(arg(0), arg(1) as u32, arg(2) as u32),
        "many_declarations" => many_declarations(arg(0) as u32),
        "custom_section" => custom_section(arg(0)),
        "many_modules" => many_modules(arg(0)),
        "many_instances" => many_instances(arg(0)),
        "instances_times_imports" => instances_times_imports(arg(0), arg(1) as u32, arg(2) as u32),
        "blocks_with_a_result" => blocks_with_a_result(arg(0)),
        "everything_at_the_limits" => everything_at_the_limits(),
        "nested_components" => nested_components(arg(0)),
        "instantiation_bomb" => instantiation_bomb(arg(0), arg(1)),
        "many_lowered_imports" => many_lowered_imports(arg(0) as u32),
        "many_adapters" => many_adapters(arg(0) as u32, arg(1) as u32),
        other => panic!("unknown case {other}"),
    }
}

/// Runs in the child: loads a component from a file, and reports how long that took.
fn load(path: &str) {
    let component = fs::read(path).unwrap();
    let package = Package::new(MANIFEST, component).unwrap();
    let mut backend = WasmBackend::new().unwrap();
    // Without the limits unless asked for, to show what they're there to prevent.
    if env::var_os("HABITAT_COMPILE_LIMITS").is_none() {
        backend.set_compile_limits(CompileLimits {
            time: Duration::from_secs(3600),
            nested_components: u64::MAX,
            modules: u64::MAX,
            functions: u64::MAX,
            code_bytes: u64::MAX,
            function_bytes: u64::MAX,
            work: u64::MAX,
            nested_loops: u64::MAX,
            function_loops: u64::MAX,
            function_branch_points: u64::MAX,
            function_block_values: u64::MAX,
            instance_imports: u64::MAX,
            memories: u64::MAX,
            table_entries: u64::MAX,
            canonical_functions: u64::MAX,
            declarations: u64::MAX,
            declaration_bytes: u64::MAX,
            signature_values: u64::MAX,
        });
    }
    let start = Instant::now();
    let result = backend.load(&package);
    let elapsed = start.elapsed();
    let outcome = match result {
        Ok(_) => "loaded".to_string(),
        Err(error) => error.to_string(),
    };
    let outcome: String = outcome.chars().take(100).collect();
    println!("{:.2} {outcome}", elapsed.as_secs_f64());
}

fn rss_kb(pid: u32) -> Option<usize> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn measure(case: &str, args: &[String], max_seconds: u64, max_mb: usize) {
    let path = env::temp_dir().join(format!("habitat-compile-cost-{}.wasm", std::process::id()));
    let component = build(case, args);
    let bytes = component.len();
    let shape = CodeShape::of(&component).ok();
    fs::write(&path, &component).unwrap();
    drop(component);

    let mut child = Command::new(env::current_exe().unwrap())
        .arg("--load")
        .arg(&path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let mut peak_kb = 0;
    let mut killed = None;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if let Some(rss) = rss_kb(child.id()) {
            peak_kb = peak_kb.max(rss);
        }
        if start.elapsed() > Duration::from_secs(max_seconds) {
            killed = Some(format!("KILLED at the {max_seconds} s guard"));
        } else if peak_kb / 1024 > max_mb {
            killed = Some(format!("KILLED at the {max_mb} MB guard"));
        }
        if killed.is_some() {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let _ = fs::remove_file(&path);

    let report = match status {
        Some(status) if status.success() => {
            let output = child.wait_with_output().unwrap();
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        Some(status) => {
            let output = child.wait_with_output().unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            let first: String = stderr.chars().take(300).collect();
            format!("CRASHED ({status}): {first}")
        }
        None => killed.unwrap(),
    };
    let verdict = shape
        .and_then(|shape| CompileLimits::default().broken_by(&shape))
        .unwrap_or_else(|| "within the limits".to_string());
    let args: Vec<&str> = args
        .iter()
        .map(|arg| arg.rsplit('/').next().unwrap_or(arg))
        .collect();
    println!(
        "{case}({args}) file {bytes:.1} MB, peak {peak:.0} MB: {report} [{verdict}]",
        args = args.join(", "),
        bytes = bytes as f64 / MB as f64,
        peak = peak_kb as f64 / 1024.0,
    );
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--load") {
        return load(&args[1]);
    }
    if args.first().map(String::as_str) == Some("--shape") {
        let component = component_holding(&fs::read(&args[1]).unwrap());
        let start = Instant::now();
        let shape = CodeShape::of(&component);
        println!("{:.2} s to read: {shape:?}", start.elapsed().as_secs_f64());
        return;
    }
    let max_seconds = args[0].parse().unwrap();
    let max_mb = args[1].parse().unwrap();
    measure(&args[2], &args[3..], max_seconds, max_mb);
}
