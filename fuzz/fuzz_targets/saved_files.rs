//! The files that the core saves for itself. They aren't the agent's to write, but they can be
//! corrupted, and anything in them has to be refused or read safely. A bad one must never allow
//! anything, or stop the agent's next run from being reported properly.
#![no_main]

use std::fs;
use std::sync::OnceLock;

use habitat_broker::audit;
use habitat_broker::grants::list_saved;
use habitat_broker::policy::{Grants, Target};
use habitat_core::home::AgentStorage;
use libfuzzer_sys::fuzz_target;
use tempfile::TempDir;

fn place() -> &'static (TempDir, AgentStorage) {
    static PLACE: OnceLock<(TempDir, AgentStorage)> = OnceLock::new();
    PLACE.get_or_init(|| {
        let root = tempfile::tempdir().unwrap();
        let storage = AgentStorage::open(root.path(), "agent").unwrap();
        (root, storage)
    })
}

fuzz_target!(|data: &[u8]| {
    let (_, storage) = place();
    fs::write(storage.grants_path(), data).unwrap();
    if let Ok(saved) = list_saved(storage) {
        for target in saved {
            match target {
                Target::NetworkHost(host) => {
                    assert!(habitat_core::manifest::is_host_name(&host), "{host:?}")
                }
                Target::Model(model) => {
                    assert!(habitat_core::manifest::is_model_name(&model), "{model:?}")
                }
            }
        }
    }
    let grants = habitat_broker::grants::RunGrants::new(storage.clone());
    let _ = grants.covers(&Target::NetworkHost("a.example".to_string()));

    // A log with no record of where it got to has to check out by itself, or be refused.
    fs::write(storage.audit_path(), data).unwrap();
    if let Ok(entries) = audit::verify(storage) {
        for (index, entry) in entries.iter().enumerate() {
            assert_eq!(entry.number, index as u64 + 1);
            let shown = audit::describe(entry);
            assert!(!shown.chars().any(char::is_control), "{shown:?}");
        }
    }
});
