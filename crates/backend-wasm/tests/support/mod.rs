//! Shared by the tests that run real agents.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The sample agent in `examples/file-agent`, built once for each test program.
#[allow(dead_code, reason = "not every test program uses each agent")]
pub fn file_agent_code() -> &'static [u8] {
    static CODE: OnceLock<Vec<u8>> = OnceLock::new();
    CODE.get_or_init(|| {
        // For a test run on a device, where the agent was built beforehand.
        if let Ok(path) = env::var("HABITAT_TEST_AGENT") {
            return fs::read(path).unwrap();
        }
        build_agent("habitat-file-agent")
    })
}

/// Builds an agent from this workspace into a component. This takes the name of its package.
pub fn build_agent(package: &str) -> Vec<u8> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    // Its own folder, so that this can't wait on the build that's running these tests.
    let target_dir: PathBuf = root.join("target/test-agents");
    let output = Command::new(env::var("CARGO").unwrap_or("cargo".to_string()))
        .current_dir(&root)
        .args([
            "build",
            "-p",
            package,
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "--target-dir",
        ])
        .arg(&target_dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "couldn't build {package}. If the target is missing, run \
         `rustup target add wasm32-unknown-unknown`.\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let library = format!(
        "wasm32-unknown-unknown/release/{}.wasm",
        package.replace('-', "_")
    );
    let module = fs::read(target_dir.join(library)).unwrap();
    wit_component::ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap()
}
