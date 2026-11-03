//! A package file. It has to be refused, or round trip to the same thing, which has to install.
#![no_main]

use std::io::Cursor;

use habitat_core::home::AgentStorage;
use habitat_core::package::Package;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match Package::read(Cursor::new(data)) {
        Ok(package) => {
            let mut written = Vec::new();
            package.write(&mut written).unwrap();
            let again = Package::read(Cursor::new(&written)).unwrap();
            assert_eq!(package.hash().to_string(), again.hash().to_string());

            let root = tempfile::tempdir().unwrap();
            let storage = AgentStorage::open(root.path(), package.manifest().name()).unwrap();
            package.install(&storage).unwrap();
            let loaded = Package::load_installed(&storage).unwrap();
            assert_eq!(package.hash().to_string(), loaded.hash().to_string());
        }
        Err(error) => {
            let text = error.to_string();
            assert!(!text.chars().any(char::is_control), "{text:?}");
        }
    }
});
