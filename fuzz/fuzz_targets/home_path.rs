//! Paths from an agent. Whatever it asks for, nothing can end up outside its home.
#![no_main]

use std::sync::OnceLock;

use habitat_core::home::AgentStorage;
use libfuzzer_sys::fuzz_target;
use tempfile::TempDir;

struct Place {
    root: TempDir,
    storage: AgentStorage,
}

fn place() -> &'static Place {
    static PLACE: OnceLock<Place> = OnceLock::new();
    PLACE.get_or_init(|| {
        let root = tempfile::tempdir().unwrap();
        let storage = AgentStorage::open(root.path(), "agent").unwrap();
        std::fs::write(root.path().join("outside.txt"), "outside").unwrap();
        std::fs::write(root.path().join("agents/agent/grants.db"), "grants").unwrap();
        Place { root, storage }
    })
}

/// A path that gets accepted has to name something inside the home, one plain name at a time.
fn check_accepted(path: &str) {
    for part in path.split('/') {
        assert!(!part.is_empty() && part != "." && part != "..", "{path:?}");
        assert!(!part.contains('\\') && !part.contains(':') && !part.contains('\0'), "{path:?}");
    }
    assert!(!path.starts_with('/'), "{path:?}");
}

fuzz_target!(|data: &[u8]| {
    let Ok(path) = std::str::from_utf8(data) else {
        return;
    };
    let place = place();
    let mut home = place.storage.home(1024 * 1024).unwrap();
    if home.write(path, b"x").is_ok() {
        check_accepted(path);
        assert_eq!(home.read(path).unwrap(), b"x", "{path:?}");
        home.remove(path).unwrap();
    }
    if home.read(path).is_ok() || home.list(path).is_ok() {
        if !path.is_empty() {
            check_accepted(path);
        }
    }
    let _ = home.remove(path);
    let _ = home.read_at(path, 0, 16);

    let root = place.root.path();
    assert_eq!(std::fs::read_to_string(root.join("outside.txt")).unwrap(), "outside");
    assert_eq!(
        std::fs::read_to_string(root.join("agents/agent/grants.db")).unwrap(),
        "grants"
    );
});
