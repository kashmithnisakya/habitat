//! Wasmtime backend for running Habitat agents.
//!
//! Agents are untrusted, and they can only reach what's linked here, which is the agent API. Each
//! run is limited in how much time and memory it can use.

mod host;

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use habitat_core::backend::{
    Backend, Event, LoadError, LoadedAgent, RunLimits, RunOutcome, RunOutput,
};
use habitat_core::home::Home;
use habitat_core::package::Package;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store, Trap, UpdateDeadline};

use crate::bindings::{Agent, AgentPre};
use crate::host::HostState;

/// The agent API, as generated from `wit/agent-home.wit`.
pub mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "agent",
    });
}

// The API's interfaces. Agents can't import anything else, whatever version it claims to be.
const ALLOWED_IMPORTS: [&str; 2] = ["habitat:host/agent-home@", "habitat:host/types@"];

/// How often a running agent stops to check the time, and so how late it can be stopped.
const TICK: Duration = Duration::from_millis(50);

/// Runs agents as WebAssembly components with Wasmtime.
pub struct WasmBackend {
    engine: Engine,
    linker: Linker<HostState>,
}

impl WasmBackend {
    pub fn new() -> Result<Self, LoadError> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(internal)?;
        let mut linker = Linker::new(&engine);
        Agent::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |state| state)
            .map_err(internal)?;
        Ok(Self { engine, linker })
    }
}

fn internal(error: wasmtime::Error) -> LoadError {
    LoadError::Internal(format!("{error:#}"))
}

impl Backend for WasmBackend {
    fn load(&self, package: &Package) -> Result<Box<dyn LoadedAgent>, LoadError> {
        let component = Component::new(&self.engine, package.component())
            .map_err(|error| LoadError::invalid_code(&format!("{error:#}")))?;

        // The linker would refuse these too, but this gives a clearer reason.
        let component_type = component.component_type();
        for (name, _) in component_type.imports(&self.engine) {
            if !ALLOWED_IMPORTS
                .iter()
                .any(|allowed| name.starts_with(allowed))
            {
                return Err(LoadError::forbidden_import(name));
            }
        }

        let incompatible = |error: wasmtime::Error| LoadError::incompatible(&format!("{error:#}"));
        let instance = self
            .linker
            .instantiate_pre(&component)
            .map_err(incompatible)?;
        let agent = AgentPre::new(instance).map_err(incompatible)?;
        Ok(Box::new(WasmAgent {
            engine: self.engine.clone(),
            agent,
        }))
    }
}

struct WasmAgent {
    engine: Engine,
    agent: AgentPre<HostState>,
}

/// Makes running agents check the time, for as long as this exists. It only runs during a run, so
/// that an idle host, such as a phone, doesn't keep waking up.
struct Ticker {
    _stop: Sender<()>,
}

impl Ticker {
    fn start(engine: Engine) -> std::io::Result<Self> {
        let (stop, stopped) = mpsc::channel::<()>();
        thread::Builder::new()
            .name("habitat-ticker".to_string())
            .spawn(move || {
                while stopped.recv_timeout(TICK) == Err(RecvTimeoutError::Timeout) {
                    engine.increment_epoch();
                }
            })?;
        Ok(Self { _stop: stop })
    }
}

impl LoadedAgent for WasmAgent {
    fn run(&self, event: &Event, home: Home, limits: &RunLimits) -> RunOutput {
        // A new store each time, so that nothing in memory carries over between runs, even after
        // a crash.
        let mut store = Store::new(&self.engine, HostState::new(home, limits.memory_bytes));
        store.limiter(HostState::limiter);

        // Other runs can make the same engine tick, so each tick only means it's time to check.
        let deadline = Instant::now() + limits.run_time;
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(move |_| {
            if Instant::now() >= deadline {
                Err(Trap::Interrupt.into())
            } else {
                Ok(UpdateDeadline::Continue(1))
            }
        });
        let Ok(_ticker) = Ticker::start(self.engine.clone()) else {
            return RunOutput {
                outcome: RunOutcome::Internal("the run couldn't be timed".to_string()),
                messages: Vec::new(),
            };
        };

        let result = self
            .agent
            .instantiate(&mut store)
            .and_then(|agent| agent.call_handle_event(&mut store, &to_api_event(event)));
        let outcome = match result {
            Ok(Ok(())) => RunOutcome::Completed,
            Ok(Err(message)) => RunOutcome::failed(&message),
            Err(error) => {
                let trap = error.downcast_ref::<Trap>();
                if trap == Some(&Trap::Interrupt) {
                    RunOutcome::TimeLimit
                } else if store.data().hit_memory_limit() {
                    RunOutcome::MemoryLimit
                } else {
                    // Only the kind of trap is reported. A full error can include names from the
                    // agent. Anything else that fails here is also down to the agent, such as
                    // creating too many instances or returning a value that makes no sense.
                    RunOutcome::Crashed(trap.map_or_else(
                        || "the agent didn't follow the rules for components".to_string(),
                        Trap::to_string,
                    ))
                }
            }
        };
        RunOutput {
            outcome,
            messages: store.into_data().into_messages(),
        }
    }
}

fn to_api_event(event: &Event) -> bindings::habitat::host::types::Event {
    use bindings::habitat::host::types::Event as ApiEvent;
    match event {
        Event::UserMessage(text) => ApiEvent::UserMessage(text.clone()),
        Event::Scheduled(name) => ApiEvent::Scheduled(name.clone()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use wit_parser::{Resolve, WorldItem, WorldKey};

    /// Everything an agent can reach. Adding to this list gives every agent a new way to affect
    /// the outside world, so check it against `docs/threat-model.md` first.
    #[test]
    #[cfg_attr(
        target_os = "android",
        ignore = "This reads the source files, which aren't on a device"
    )]
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
