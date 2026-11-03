//! An agent's manifest. It has to be refused or give a manifest that holds to its rules.
#![no_main]

use habitat_core::backend::HostLimits;
use habitat_core::manifest::{Manifest, is_host_name, is_model_name};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    match Manifest::parse(text) {
        Ok(manifest) => {
            assert!(manifest.requested_hosts().iter().all(|host| is_host_name(host)));
            assert!(manifest.requested_models().iter().all(|model| is_model_name(model)));
            let host = HostLimits::default();
            let limits = host.limits_for(&manifest.limits());
            assert!(limits.memory_bytes <= u64::from(host.memory_mb) * 1024 * 1024);
            assert!(limits.disk_bytes <= u64::from(host.disk_mb) * 1024 * 1024);
        }
        // What's said about it has to be safe to print, even for a hostile manifest.
        Err(error) => {
            let text = error.to_string();
            assert!(!text.chars().any(|c| c.is_control() && c != '\n'), "{text:?}");
        }
    }
});
