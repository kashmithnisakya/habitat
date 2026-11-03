//! Wasmtime backend for running Habitat agents.
//!
//! So far this only holds the bindings generated from `wit/agent-home.wit`. See
//! `docs/architecture.md`.

/// The agent API, as generated from `wit/agent-home.wit`.
pub mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "agent",
    });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use wasmtime::component::{HasSelf, Linker, Resource};
    use wasmtime::{Engine, Result};
    use wit_parser::{Resolve, WorldItem, WorldKey};

    use super::bindings::Agent;
    use super::bindings::habitat::host::agent_home;
    use super::bindings::habitat::host::types::{
        self, AccessTarget, ByteStream, DirEntry, Grant, HomeError, HostByteStream,
        HostModelStream, HostUserFile, HttpRequest, HttpResponse, LogLevel, ModelRequest,
        ModelStream, TokenUsage, UserFile,
    };

    /// Implements the whole API, which only compiles if the bindings are complete.
    struct DenyEverything;

    fn denied<T>() -> Result<T, HomeError> {
        Err(HomeError::Denied("not implemented".to_string()))
    }

    impl HostByteStream for DenyEverything {
        fn read(
            &mut self,
            _stream: Resource<ByteStream>,
            _max_bytes: u32,
        ) -> Result<Option<Vec<u8>>, HomeError> {
            denied()
        }

        fn drop(&mut self, _stream: Resource<ByteStream>) -> Result<()> {
            Ok(())
        }
    }

    impl HostModelStream for DenyEverything {
        fn next(&mut self, _stream: Resource<ModelStream>) -> Result<Option<String>, HomeError> {
            denied()
        }

        fn usage(&mut self, _stream: Resource<ModelStream>) -> Option<TokenUsage> {
            None
        }

        fn drop(&mut self, _stream: Resource<ModelStream>) -> Result<()> {
            Ok(())
        }
    }

    impl HostUserFile for DenyEverything {
        fn name(&mut self, _file: Resource<UserFile>) -> String {
            String::new()
        }

        fn size(&mut self, _file: Resource<UserFile>) -> u64 {
            0
        }

        fn read(
            &mut self,
            _file: Resource<UserFile>,
            _offset: u64,
            _max_bytes: u32,
        ) -> Result<Vec<u8>, HomeError> {
            denied()
        }

        fn drop(&mut self, _file: Resource<UserFile>) -> Result<()> {
            Ok(())
        }
    }

    impl types::Host for DenyEverything {}

    impl agent_home::Host for DenyEverything {
        fn read_file(
            &mut self,
            _path: String,
            _offset: u64,
            _max_bytes: u32,
        ) -> Result<Vec<u8>, HomeError> {
            denied()
        }

        fn write_file(&mut self, _path: String, _data: Vec<u8>) -> Result<(), HomeError> {
            denied()
        }

        fn remove_file(&mut self, _path: String) -> Result<(), HomeError> {
            denied()
        }

        fn list_dir(&mut self, _path: String) -> Result<Vec<DirEntry>, HomeError> {
            denied()
        }

        fn now_ms(&mut self) -> u64 {
            0
        }

        fn random_bytes(&mut self, _count: u32) -> Result<Vec<u8>, HomeError> {
            denied()
        }

        fn send_message(&mut self, _text: String) -> Result<(), HomeError> {
            denied()
        }

        fn log(&mut self, _level: LogLevel, _message: String) {}

        fn model_complete(
            &mut self,
            _request: ModelRequest,
        ) -> Result<Resource<ModelStream>, HomeError> {
            denied()
        }

        fn http_fetch(&mut self, _request: HttpRequest) -> Result<HttpResponse, HomeError> {
            denied()
        }

        fn request_access(
            &mut self,
            _target: AccessTarget,
            _reason: String,
        ) -> Result<Grant, HomeError> {
            denied()
        }

        fn pick_user_file(&mut self, _reason: String) -> Result<Resource<UserFile>, HomeError> {
            denied()
        }
    }

    #[test]
    fn the_host_bindings_link() {
        let engine = Engine::default();
        let mut linker = Linker::<DenyEverything>::new(&engine);
        Agent::add_to_linker::<_, HasSelf<DenyEverything>>(&mut linker, |state| state).unwrap();
    }

    /// Everything an agent can reach. Adding to this list gives every agent a new way to affect
    /// the outside world, so check it against `docs/threat-model.md` first.
    #[test]
    fn agents_can_only_import_the_reviewed_api() {
        let mut resolve = Resolve::default();
        let wit = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../wit");
        let (package, _) = resolve.push_dir(wit).unwrap();

        // No WASI or any other package, which is how raw sockets or host files would get in.
        let packages: Vec<_> = resolve
            .packages
            .iter()
            .map(|(_, package)| package.name.to_string())
            .collect();
        assert_eq!(packages, ["habitat:host@0.1.0"]);

        let world = resolve.select_world(&[package], Some("agent")).unwrap();
        let world = &resolve.worlds[world];

        let mut imports = BTreeSet::new();
        for (key, item) in &world.imports {
            match (key, item) {
                (WorldKey::Interface(id), WorldItem::Interface { .. }) => {
                    imports.insert(resolve.interfaces[*id].name.clone().unwrap());
                }
                // Types that the world's export uses.
                (_, WorldItem::Type { .. }) => {}
                other => panic!("unexpected import: {other:?}"),
            }
        }
        assert_eq!(
            imports,
            BTreeSet::from(["agent-home".to_string(), "types".to_string()])
        );

        let exports: Vec<_> = world
            .exports
            .keys()
            .map(|key| resolve.name_world_key(key))
            .collect();
        assert_eq!(exports, ["handle-event"]);

        let functions = |name: &str| -> BTreeSet<String> {
            let (_, interface) = resolve
                .interfaces
                .iter()
                .find(|(_, interface)| interface.name.as_deref() == Some(name))
                .unwrap();
            interface.functions.keys().cloned().collect()
        };
        assert_eq!(
            functions("agent-home"),
            BTreeSet::from(
                [
                    "http-fetch",
                    "list-dir",
                    "log",
                    "model-complete",
                    "now-ms",
                    "pick-user-file",
                    "random-bytes",
                    "read-file",
                    "remove-file",
                    "request-access",
                    "send-message",
                    "write-file",
                ]
                .map(String::from)
            )
        );
        assert_eq!(
            functions("types"),
            BTreeSet::from(
                [
                    "[method]byte-stream.read",
                    "[method]model-stream.next",
                    "[method]model-stream.usage",
                    "[method]user-file.name",
                    "[method]user-file.read",
                    "[method]user-file.size",
                ]
                .map(String::from)
            )
        );
    }
}
