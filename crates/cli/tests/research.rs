//! Runs the real research helper, on the real backend and broker, with a fake network underneath, so
//! that nothing here needs an API key or the internet. These need the `wasm32-unknown-unknown` target.

#[path = "../../backend-wasm/tests/support/mod.rs"]
mod support;

use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use habitat_backend_wasm::WasmBackend;
use habitat_broker::approval::{Answer, Approver, FilePrompt, Prompt};
use habitat_broker::audit::{self, AuditEvent};
use habitat_broker::broker::Broker;
use habitat_broker::gateway::{CheckedUrl, Incoming, Network, NetworkError};
use habitat_broker::vault::{NewSecret, SecretHeader, SecretStore, SecretValue, Vault, VaultError};
use habitat_core::backend::{Event, FetchRequest, HostLimits, Lifetime, RunOutcome, Target};
use habitat_core::home::AgentStorage;
use habitat_core::package::Package;
use habitat_core::run_log::{RunEnding, RunRecord};
use habitat_core::supervisor::Supervisor;
use tempfile::TempDir;

use support::build_agent;

const NAME: &str = "research-helper";
const SECRET: &str = "sk-THE-REAL-KEY";
const WIKIPEDIA: &str = "https://en.wikipedia.org/api/rest_v1/page/summary/Honeybee";
const PROVIDER: &str = "https://api.anthropic.com/v1/messages";
const ANSWER: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":1}}}\n\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Bees \"}}\n\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"make honey.\"}}\n\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\
data: {\"type\":\"message_stop\"}\n\n";

fn code() -> &'static [u8] {
    static CODE: OnceLock<Vec<u8>> = OnceLock::new();
    CODE.get_or_init(|| build_agent("habitat-research-helper"))
}

fn manifest() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::read_to_string(root.join("examples/research-helper/agent.toml")).unwrap()
}

#[derive(Default)]
struct FakeNetwork {
    sent: Mutex<Vec<(String, FetchRequest, Vec<String>)>>,
    wikipedia_down: bool,
}

/// Lets a test keep looking at a network that the broker owns.
struct Shared(Arc<FakeNetwork>);

impl Network for Shared {
    fn send(
        &self,
        url: &CheckedUrl,
        request: &FetchRequest,
        secrets: &[SecretHeader],
        _wait: Duration,
    ) -> Result<Incoming, NetworkError> {
        let names = secrets.iter().map(|secret| format!("{secret:?}")).collect();
        self.0
            .sent
            .lock()
            .unwrap()
            .push((url.to_string(), request.clone(), names));
        let body: &str = match url.to_string().as_str() {
            WIKIPEDIA if self.0.wikipedia_down => return Err(NetworkError::Failed),
            WIKIPEDIA => "{\"extract\":\"Honeybees live in hives.\"}",
            PROVIDER => ANSWER,
            _ => return Ok(page(404, "")),
        };
        Ok(page(200, body))
    }
}

fn page(status: u16, body: &str) -> Incoming {
    Incoming {
        status,
        headers: Vec::new(),
        body: Box::new(Cursor::new(body.as_bytes().to_vec())),
    }
}

#[derive(Clone, Default)]
struct FakeStore(Arc<Mutex<HashMap<String, Vec<u8>>>>);

impl SecretStore for FakeStore {
    fn put(&self, name: &str, _value: &SecretValue) -> Result<(), VaultError> {
        self.0
            .lock()
            .unwrap()
            .insert(name.to_string(), SECRET.as_bytes().to_vec());
        Ok(())
    }

    fn get(&self, name: &str) -> Result<SecretValue, VaultError> {
        let values = self.0.lock().unwrap();
        let value = values.get(name).ok_or(VaultError::KeystoreUnavailable)?;
        Ok(SecretValue::new(value.clone()))
    }

    fn delete(&self, name: &str) -> Result<(), VaultError> {
        self.0.lock().unwrap().remove(name);
        Ok(())
    }
}

/// Says what it's told to, by what's being asked for, and remembers everything.
#[derive(Clone, Default)]
struct User {
    allow_network: bool,
    allow_model: bool,
    lifetime: Option<Lifetime>,
    file: Option<PathBuf>,
    prompts: Arc<Mutex<Vec<Prompt>>>,
    messages: Arc<Mutex<Vec<String>>>,
    file_prompts: Arc<AtomicUsize>,
}

impl User {
    fn allowing_everything() -> Self {
        Self {
            allow_network: true,
            allow_model: true,
            ..Self::default()
        }
    }
}

impl Approver for User {
    fn ask(&self, prompt: &Prompt, _wait: Duration) -> Answer {
        self.prompts.lock().unwrap().push(prompt.clone());
        let allowed = match prompt.to_string().contains("AI model") {
            true => self.allow_model,
            false => self.allow_network,
        };
        if allowed {
            Answer::Allow(self.lifetime.unwrap_or(Lifetime::ThisRun))
        } else {
            Answer::Deny
        }
    }

    fn show_message(&self, _agent: &str, text: &str) {
        self.messages.lock().unwrap().push(text.to_string());
    }

    fn pick_file(&self, _prompt: &FilePrompt, _wait: Duration) -> Option<PathBuf> {
        self.file_prompts.fetch_add(1, Ordering::SeqCst);
        self.file.clone()
    }
}

struct Setup {
    root: TempDir,
    network: Arc<FakeNetwork>,
    user: User,
    store: FakeStore,
    supervisor: Supervisor,
}

fn setup_with(user: User, network: FakeNetwork, with_key: bool) -> Setup {
    let root = TempDir::new().unwrap();
    let storage = AgentStorage::open(root.path(), NAME).unwrap();
    Package::new(&manifest(), code().to_vec())
        .unwrap()
        .install(&storage)
        .unwrap();
    let store = FakeStore::default();
    let vault = Vault::new(root.path(), store.clone());
    if with_key {
        vault
            .add(NewSecret {
                name: "anthropic",
                hosts: &["api.anthropic.com".to_string()],
                header: "x-api-key",
                prefix: "",
                value: SecretValue::new(SECRET.as_bytes().to_vec()),
            })
            .unwrap();
    }
    let network = Arc::new(network);
    let supervisor = Supervisor::new(
        root.path(),
        WasmBackend::new().unwrap(),
        HostLimits::default(),
    )
    .with_broker(
        Broker::with_network(user.clone(), Shared(Arc::clone(&network))).with_vault(vault),
    );
    Setup {
        root,
        network,
        user,
        store,
        supervisor,
    }
}

fn setup(user: User) -> Setup {
    setup_with(user, FakeNetwork::default(), true)
}

impl Setup {
    fn run(&self, message: &str) -> RunRecord {
        self.supervisor
            .run(NAME, &Event::UserMessage(message.to_string()))
            .unwrap()
    }

    fn storage(&self) -> AgentStorage {
        AgentStorage::open_existing(self.root.path(), NAME).unwrap()
    }

    fn messages(&self) -> Vec<String> {
        self.user.messages.lock().unwrap().clone()
    }

    fn events(&self) -> Vec<AuditEvent> {
        audit::verify(&self.storage())
            .unwrap()
            .into_iter()
            .map(|entry| entry.event)
            .collect()
    }
}

fn completed(record: &RunRecord) {
    assert_eq!(
        record.ending,
        Some(RunEnding::Ran(RunOutcome::Completed)),
        "{record:?}"
    );
}

fn failed_with(record: &RunRecord, text: &str) {
    let Some(RunEnding::Ran(RunOutcome::Failed(message))) = &record.ending else {
        panic!("{record:?}");
    };
    assert!(message.contains(text), "{message}");
}

#[test]
fn a_whole_run_goes_through_approvals_the_network_and_the_model() {
    let setup = setup(User::allowing_everything());
    completed(&setup.run("research Honeybee"));
    assert_eq!(setup.messages(), ["Bees make honey."]);

    let prompts = setup.user.prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[0].to_string().contains("en.wikipedia.org"));
    assert!(prompts[1].to_string().contains("AI model default"));
    assert!(prompts[1].agents_claim.contains("to answer"));
    drop(prompts);

    let sent = setup.network.sent.lock().unwrap();
    assert_eq!(sent[0].0, WIKIPEDIA);
    assert_eq!(sent[1].0, PROVIDER);
    // What it looked up went to the model, and the key went only there.
    let body = String::from_utf8(sent[1].1.body.clone().unwrap()).unwrap();
    assert!(body.contains("Honeybees live in hives."), "{body}");
    assert!(sent[0].2.is_empty());
    assert_eq!(sent[1].2.len(), 1);
    drop(sent);

    let events = setup.events();
    assert!(events.contains(&AuditEvent::ModelUsed {
        model: "default".to_string(),
        input_tokens: 12,
        output_tokens: 5,
    }));
    assert!(events.contains(&AuditEvent::MessageSent));
    assert_eq!(
        events.last(),
        Some(&AuditEvent::RunEnded {
            how: "completed".to_string()
        })
    );
}

#[test]
fn the_key_never_goes_anywhere_the_agent_or_a_file_could_see() {
    let setup = setup(User::allowing_everything());
    completed(&setup.run("research Honeybee"));
    assert_eq!(setup.store.0.lock().unwrap().len(), 1);
    let mut pending = vec![setup.root.path().to_path_buf()];
    while let Some(folder) = pending.pop() {
        for entry in fs::read_dir(folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let text = fs::read(&path).unwrap();
                assert!(!String::from_utf8_lossy(&text).contains(SECRET), "{path:?}");
            }
        }
    }
}

#[test]
fn notes_are_kept_between_runs() {
    let setup = setup(User::allowing_everything());
    completed(&setup.run("notes"));
    assert_eq!(setup.messages(), ["I haven't kept any notes yet."]);
    completed(&setup.run("research Honeybee"));
    completed(&setup.run("notes"));
    let messages = setup.messages();
    let last = messages.last().unwrap();
    assert!(last.contains("Honeybee: Bees make honey."), "{last}");
    assert!(last.contains("en.wikipedia.org"), "{last}");
}

#[test]
fn being_refused_the_network_is_a_clear_message_and_it_carries_on() {
    let setup = setup(User {
        allow_model: true,
        ..User::default()
    });
    completed(&setup.run("research Honeybee"));
    let messages = setup.messages();
    assert!(messages[0].contains("refused"), "{messages:?}");
    assert_eq!(messages[1], "Bees make honey.");
    assert_eq!(setup.network.sent.lock().unwrap().len(), 1);
}

#[test]
fn being_refused_the_model_is_a_clear_result_not_a_crash() {
    let setup = setup(User {
        allow_network: true,
        ..User::default()
    });
    failed_with(
        &setup.run("research Honeybee"),
        "Using the model was refused",
    );
}

#[test]
fn a_site_being_down_is_a_clear_message_and_it_carries_on() {
    let setup = setup_with(
        User::allowing_everything(),
        FakeNetwork {
            wikipedia_down: true,
            ..FakeNetwork::default()
        },
        true,
    );
    completed(&setup.run("research Honeybee"));
    let messages = setup.messages();
    assert!(messages[0].contains("isn't available"), "{messages:?}");
    assert_eq!(messages[1], "Bees make honey.");
}

#[test]
fn no_api_key_is_a_clear_result() {
    let setup = setup_with(User::allowing_everything(), FakeNetwork::default(), false);
    failed_with(&setup.run("research Honeybee"), "API key");
}

#[test]
fn reading_a_file_makes_the_model_need_a_fresh_approval_even_with_a_saved_grant() {
    let folder = TempDir::new().unwrap();
    let file = folder.path().join("diary.txt");
    fs::write(&file, "my private words").unwrap();
    let user = User {
        file: Some(file),
        lifetime: Some(Lifetime::UntilRevoked),
        ..User::allowing_everything()
    };
    let setup = setup(user);
    // This saves approval for both, for good.
    completed(&setup.run("research Honeybee"));
    let asked = setup.user.prompts.lock().unwrap().len();
    completed(&setup.run("research Honeybee"));
    assert_eq!(
        setup.user.prompts.lock().unwrap().len(),
        asked,
        "it's saved"
    );

    completed(&setup.run("research-file Honeybee"));
    assert_eq!(setup.user.file_prompts.load(Ordering::SeqCst), 1);
    let prompts = setup.user.prompts.lock().unwrap();
    let last = prompts.last().unwrap();
    assert!(last.after_private_read, "{last:?}");
    assert!(last.to_string().contains("AI model default"));
    drop(prompts);

    // The file went to the model, and its name is in the notes, but neither is in the log.
    let sent = setup.network.sent.lock().unwrap();
    let body = String::from_utf8(sent.last().unwrap().1.body.clone().unwrap()).unwrap();
    assert!(body.contains("my private words"), "{body}");
    drop(sent);
    let log = fs::read_to_string(setup.storage().audit_path()).unwrap();
    assert!(
        !log.contains("diary") && !log.contains("private words"),
        "{log}"
    );
}

#[test]
fn not_choosing_a_file_is_a_clear_result() {
    let setup = setup(User::allowing_everything());
    failed_with(
        &setup.run("research-file Honeybee"),
        "Reading your file was refused",
    );
}

#[test]
fn the_tutorials_manifest_is_the_examples() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let tutorial = fs::read_to_string(root.join("docs/research-helper.md")).unwrap();
    let manifest = manifest();
    let requests = &manifest[manifest.find("[requests]").unwrap()..];
    assert!(
        tutorial.contains(requests.trim()),
        "the tutorial's manifest has changed"
    );
}

#[test]
fn anything_else_gets_a_clear_answer() {
    let setup = setup(User::default());
    failed_with(&setup.run("nonsense"), "try `research");
    failed_with(&setup.run("research"), "tell me what");
}

#[test]
fn a_saved_grant_for_the_model_is_recorded() {
    let setup = setup(User {
        lifetime: Some(Lifetime::UntilRevoked),
        ..User::allowing_everything()
    });
    completed(&setup.run("research Honeybee"));
    let saved = habitat_broker::grants::list_saved(&setup.storage()).unwrap();
    assert!(
        saved.contains(&Target::Model("default".to_string())),
        "{saved:?}"
    );
}
