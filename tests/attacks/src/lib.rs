//! A place to attack Habitat from, and a way to check what happened from outside the attacker.
//!
//! An [`Arena`] is a whole host, with the real supervisor, backend and broker, and only the network
//! and the keystore swapped for fakes. It installs a hostile agent, and plants things around it that
//! it should never reach. A test runs an attack with [`Arena::attack`], then uses the checks here.
//!
//! **Nothing here trusts what the agent says about itself.** A check looks at the host's files, the
//! audit log and everything that the agent could have seen.
//!
//! `README.md` says how to add an attack.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use habitat_backend_wasm::WasmBackend;
use habitat_broker::approval::{Answer, Approver, FilePrompt, Prompt};
use habitat_broker::audit::{self, AuditEvent};
use habitat_broker::broker::Broker;
use habitat_broker::gateway::{CheckedUrl, Incoming, Network, NetworkError};
use habitat_broker::vault::{NewSecret, SecretHeader, SecretStore, SecretValue, Vault, VaultError};
use habitat_core::backend::{
    AccessRefused, BrokerFactory, Event, FetchError, FetchRequest, FetchResponse, HostLimits,
    Lifetime, RunBroker, RunLimits, Target,
};
use habitat_core::home::AgentStorage;
use habitat_core::manifest::Manifest;
use habitat_core::package::Package;
use habitat_core::run_log::{RunEnding, RunId, RunRecord};
use habitat_core::supervisor::Supervisor;
use tempfile::TempDir;

/// The agent's name. Its folder is `agents/hostile`.
pub const AGENT: &str = "hostile";
/// A secret planted in the keystore. It must never appear anywhere the agent could see it.
pub const PLANTED_SECRET: &str = "sk-PLANTED-SECRET-c4f1d9";
/// Planted in a file outside the agent's home.
pub const OUTSIDE_CANARY: &str = "OUTSIDE-CANARY-7e21";
/// Planted in another agent's home.
pub const OTHER_AGENT_CANARY: &str = "OTHER-AGENT-CANARY-90ab";
/// The name of the other agent.
pub const OTHER_AGENT: &str = "bystander";
/// The host that the hostile agent is allowed to ask for.
pub const ALLOWED_HOST: &str = "allowed.example";
/// A second one, for checking one grant doesn't cover another.
pub const ALSO_ALLOWED_HOST: &str = "also-allowed.example";
const PROVIDER_HOST: &str = "api.anthropic.com";
const MB: usize = 1024 * 1024;

/// What a check found. A test that gets one has found a real problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breach(pub String);

impl fmt::Display for Breach {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BREACH: {}", self.0)
    }
}

impl std::error::Error for Breach {}

/// How a run went, as seen from outside.
#[derive(Debug, Clone)]
pub struct Report {
    pub record: RunRecord,
    /// Everything the agent showed the user.
    pub messages: Vec<String>,
}

impl Report {
    /// Everything that the agent saw or said in this run, which it could also have sent somewhere.
    pub fn agent_visible(&self) -> String {
        let ending = format!("{:?}", self.record.ending);
        format!("{}\n{ending}", self.messages.join("\n"))
    }
}

/// Answers prompts as a test sets it to, and remembers all it was shown.
#[derive(Clone)]
pub struct User {
    pub answer: Arc<Mutex<Answer>>,
    pub file: Arc<Mutex<Option<PathBuf>>>,
    pub prompts: Arc<Mutex<Vec<String>>>,
    pub file_prompts: Arc<Mutex<Vec<String>>>,
    pub messages: Arc<Mutex<Vec<String>>>,
}

impl Default for User {
    fn default() -> Self {
        Self {
            answer: Arc::new(Mutex::new(Answer::Deny)),
            file: Arc::default(),
            prompts: Arc::default(),
            file_prompts: Arc::default(),
            messages: Arc::default(),
        }
    }
}

impl Approver for User {
    fn ask(&self, prompt: &Prompt, _: Duration) -> Answer {
        self.prompts.lock().unwrap().push(prompt.to_string());
        *self.answer.lock().unwrap()
    }

    fn show_message(&self, _: &str, text: &str) {
        self.messages.lock().unwrap().push(text.to_string());
    }

    fn pick_file(&self, prompt: &FilePrompt, _: Duration) -> Option<PathBuf> {
        self.file_prompts.lock().unwrap().push(prompt.to_string());
        self.file.lock().unwrap().clone()
    }
}

/// One thing the fake network was asked to do.
#[derive(Debug, Clone)]
pub struct Sent {
    pub url: String,
    pub request: FetchRequest,
    /// How many secret headers the core added.
    pub secrets: usize,
}

/// What the fake network does for a URL.
#[derive(Clone)]
pub enum Reply {
    Page {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// Sends back everything it was sent, headers and secrets included, as a server could.
    Echo,
    Fail(NetworkError),
}

impl Reply {
    pub fn page(status: u16, body: &str) -> Self {
        Self::Page {
            status,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    pub fn redirect(location: &str) -> Self {
        Self::Page {
            status: 302,
            headers: vec![("location".to_string(), location.to_string())],
            body: Vec::new(),
        }
    }
}

/// A network that does as it's told, and remembers everything. Asking it for a URL it hasn't been
/// told about gets a 404.
#[derive(Clone, Default)]
pub struct FakeNetwork {
    pub replies: Arc<Mutex<HashMap<String, Reply>>>,
    pub sent: Arc<Mutex<Vec<Sent>>>,
}

impl FakeNetwork {
    pub fn reply(&self, url: &str, reply: Reply) {
        self.replies.lock().unwrap().insert(url.to_string(), reply);
    }

    pub fn urls_sent(&self) -> Vec<String> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .map(|sent| sent.url.clone())
            .collect()
    }
}

impl Network for FakeNetwork {
    fn send(
        &self,
        url: &CheckedUrl,
        request: &FetchRequest,
        secrets: &[SecretHeader],
        _: Duration,
    ) -> Result<Incoming, NetworkError> {
        let text = url.to_string();
        self.sent.lock().unwrap().push(Sent {
            url: text.clone(),
            request: request.clone(),
            secrets: secrets.len(),
        });
        let reply = self
            .replies
            .lock()
            .unwrap()
            .get(&text)
            .cloned()
            .unwrap_or_else(|| Reply::page(404, ""));
        match reply {
            Reply::Page {
                status,
                headers,
                body,
            } => Ok(Incoming {
                status,
                headers,
                body: Box::new(Cursor::new(body)),
            }),
            Reply::Echo => {
                // Everything it can see, the way a hostile server would send it back. A secret
                // header can't be printed, which is the point, so that comes back as its text.
                let body = format!("{request:?} {secrets:?}");
                Ok(Incoming {
                    status: 200,
                    headers: request.headers.clone(),
                    body: Box::new(Cursor::new(body.into_bytes())),
                })
            }
            Reply::Fail(error) => Err(error),
        }
    }
}

/// A keystore in memory, holding the planted secret.
#[derive(Clone, Default)]
pub struct FakeStore(Arc<Mutex<HashMap<String, Vec<u8>>>>);

impl SecretStore for FakeStore {
    fn put(&self, name: &str, _: &SecretValue) -> Result<(), VaultError> {
        self.0
            .lock()
            .unwrap()
            .insert(name.to_string(), PLANTED_SECRET.as_bytes().to_vec());
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

/// What to give the hostile agent.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub memory_mb: u32,
    pub run_seconds: u32,
    pub disk_mb: u32,
    pub tokens: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_mb: 64,
            run_seconds: 5,
            disk_mb: 1,
            tokens: 1000,
        }
    }
}

/// Builds the hostile agent into a component, once for each test program.
pub fn hostile_code() -> &'static [u8] {
    static CODE: OnceLock<Vec<u8>> = OnceLock::new();
    CODE.get_or_init(|| build_agent("habitat-hostile-agent"))
}

fn build_agent(package: &str) -> Vec<u8> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    // Its own folder, so that this can't wait on the build that's running these tests.
    let target_dir = root.join("target/test-agents");
    let output = Command::new(std::env::var("CARGO").unwrap_or("cargo".to_string()))
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

fn manifest_for(name: &str, limits: Limits) -> String {
    format!(
        "[agent]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n[limits]\nmemory_mb = {}\nrun_seconds = {}\ndisk_mb = {}\nmodel_budget_tokens = {}\n\n[requests]\nnetwork = [\"{ALLOWED_HOST}\", \"{ALSO_ALLOWED_HOST}\"]\nmodels = [\"default\"]\nfiles = \"ask\"\n",
        limits.memory_mb, limits.run_seconds, limits.disk_mb, limits.tokens
    )
}

/// A whole host for an attack to happen in.
pub struct Arena {
    root: TempDir,
    outside: TempDir,
    pub supervisor: Supervisor,
    pub user: User,
    pub network: FakeNetwork,
    pub store: FakeStore,
}

impl Arena {
    /// A host with everything planted, and the hostile agent installed.
    pub fn new() -> Self {
        Self::with_limits(Limits::default())
    }

    pub fn with_limits(limits: Limits) -> Self {
        let user = User::default();
        let network = FakeNetwork::default();
        let store = FakeStore::default();
        let root = TempDir::new().unwrap();
        let vault = Vault::new(root.path(), store.clone());
        let broker = Broker::with_network(user.clone(), network.clone()).with_vault(vault);
        Self::build(root, limits, broker, user, network, store)
    }

    /// The same, with a broker of a test's own. The harness's own tests use this to prove that the
    /// checks can fail.
    pub fn with_broker(broker: impl BrokerFactory + 'static) -> Self {
        Self::build(
            TempDir::new().unwrap(),
            Limits::default(),
            broker,
            User::default(),
            FakeNetwork::default(),
            FakeStore::default(),
        )
    }

    fn build(
        root: TempDir,
        limits: Limits,
        broker: impl BrokerFactory + 'static,
        user: User,
        network: FakeNetwork,
        store: FakeStore,
    ) -> Self {
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret.txt"), OUTSIDE_CANARY).unwrap();

        let storage = AgentStorage::open(root.path(), AGENT).unwrap();
        Package::new(&manifest_for(AGENT, limits), hostile_code().to_vec())
            .unwrap()
            .install(&storage)
            .unwrap();
        let other = AgentStorage::open(root.path(), OTHER_AGENT).unwrap();
        Package::new(&manifest_for(OTHER_AGENT, limits), hostile_code().to_vec())
            .unwrap()
            .install(&other)
            .unwrap();
        fs::write(
            root.path()
                .join("agents")
                .join(OTHER_AGENT)
                .join("home/secret.txt"),
            OTHER_AGENT_CANARY,
        )
        .unwrap();

        Vault::new(root.path(), store.clone())
            .add(NewSecret {
                name: "anthropic",
                hosts: &[PROVIDER_HOST.to_string()],
                header: "x-api-key",
                prefix: "",
                value: SecretValue::new(PLANTED_SECRET.as_bytes().to_vec()),
            })
            .unwrap();

        let supervisor = Supervisor::new(
            root.path(),
            WasmBackend::new().unwrap(),
            HostLimits::default(),
        )
        .with_broker(broker);
        Self {
            root,
            outside,
            supervisor,
            user,
            network,
            store,
        }
    }

    /// Runs an attack, and says what happened. The hostile agent is told what to try by the message.
    pub fn attack(&self, message: &str) -> Report {
        let before = self.user.messages.lock().unwrap().len();
        let record = self
            .supervisor
            .run(AGENT, &Event::UserMessage(message.to_string()))
            .expect("the run should at least have been recorded");
        let messages = self.user.messages.lock().unwrap()[before..].to_vec();
        Report { record, messages }
    }

    pub fn storage(&self) -> AgentStorage {
        AgentStorage::open_existing(self.root.path(), AGENT).unwrap()
    }

    pub fn agent_folder(&self) -> PathBuf {
        self.root.path().join("agents").join(AGENT)
    }

    pub fn home(&self) -> PathBuf {
        self.agent_folder().join("home")
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// A folder outside everything, holding `secret.txt`.
    pub fn outside(&self) -> &Path {
        self.outside.path()
    }

    /// Everything in the audit log, after checking that it's all still in one piece.
    pub fn log(&self) -> Result<Vec<AuditEvent>, Breach> {
        audit::verify(&self.storage())
            .map(|entries| entries.into_iter().map(|entry| entry.event).collect())
            .map_err(|error| Breach(format!("the audit log doesn't check out: {error}")))
    }

    /// Takes down everything outside the agent's own home, to compare with later. The records of
    /// its own runs are left out, as they change with each run, and so are locks.
    pub fn snapshot(&self) -> Snapshot {
        let mut files = BTreeMap::new();
        for (base, label) in [(self.root.path(), "root"), (self.outside.path(), "outside")] {
            walk(base, base, label, &self.home(), &mut files);
        }
        Snapshot(files)
    }

    /// Checks that nothing outside the agent's home has changed since a snapshot.
    pub fn check_unchanged(&self, before: &Snapshot) -> Result<(), Breach> {
        let now = self.snapshot();
        for (path, contents) in &before.0 {
            match now.0.get(path) {
                Some(found) if found == contents => {}
                Some(_) => return Err(Breach(format!("{path} was changed"))),
                None => return Err(Breach(format!("{path} was removed"))),
            }
        }
        for path in now.0.keys() {
            if !before.0.contains_key(path) {
                return Err(Breach(format!("{path} was created")));
            }
        }
        Ok(())
    }

    /// Checks that nothing planted appears anywhere the agent could have seen it, including what's
    /// in its own home now.
    pub fn check_no_leak(&self, report: &Report) -> Result<(), Breach> {
        let mut seen = report.agent_visible();
        let mut home_files = BTreeMap::new();
        let home = self.home();
        walk(
            &home,
            &home,
            "home",
            Path::new("/nothing/skipped"),
            &mut home_files,
        );
        for contents in home_files.values() {
            seen.push_str(&String::from_utf8_lossy(contents));
        }
        for planted in [PLANTED_SECRET, OUTSIDE_CANARY, OTHER_AGENT_CANARY] {
            if seen.contains(planted) {
                return Err(Breach(format!("the agent got hold of {planted}")));
            }
        }
        Ok(())
    }

    /// Checks that a run ended, and that another can start after it, and do its job.
    pub fn check_host_still_works(&self) -> Result<(), Breach> {
        let report = self.attack("write still-works.txt yes");
        if report.record.ending
            != Some(RunEnding::Ran(habitat_core::backend::RunOutcome::Completed))
        {
            return Err(Breach(format!(
                "the next run didn't work: {:?}",
                report.record.ending
            )));
        }
        if fs::read_to_string(self.home().join("still-works.txt"))
            .ok()
            .as_deref()
            != Some("yes")
        {
            return Err(Breach("the next run couldn't save anything".to_string()));
        }
        let _ = fs::remove_file(self.home().join("still-works.txt"));
        Ok(())
    }

    /// Checks that the planted secret isn't anywhere in the host's files, as it should only ever be
    /// in the keystore.
    pub fn check_secret_only_in_keystore(&self) -> Result<(), Breach> {
        let mut files = BTreeMap::new();
        walk(
            self.root.path(),
            self.root.path(),
            "root",
            Path::new("/nothing/skipped"),
            &mut files,
        );
        for (path, contents) in files {
            if String::from_utf8_lossy(&contents).contains(PLANTED_SECRET) {
                return Err(Breach(format!("the secret is in {path}")));
            }
        }
        Ok(())
    }
}

impl Default for Arena {
    fn default() -> Self {
        Self::new()
    }
}

/// The contents of every file outside the agent's home at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot(BTreeMap<String, Vec<u8>>);

impl Snapshot {
    pub fn holds(&self, path_part: &str) -> bool {
        self.0.keys().any(|path| path.contains(path_part))
    }
}

/// Things that change on every run, and that a run is supposed to change.
fn changes_every_run(path: &Path) -> bool {
    let text = path.to_string_lossy().replace('\\', "/");
    text.contains("/runs/")
        || text.contains("/locks/")
        || text.contains("/tmp/")
        || text.ends_with("audit.db")
        || text.ends_with("audit.head")
}

fn walk(base: &Path, dir: &Path, label: &str, skip: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == skip || changes_every_run(&path) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            walk(base, &path, label, skip, files);
        } else if metadata.is_file() {
            let relative = path.strip_prefix(base).unwrap_or(&path).display();
            files.insert(
                format!("{label}/{relative}"),
                fs::read(&path).unwrap_or_default(),
            );
        }
    }
}

/// A host that does everything wrong, for proving that the checks can fail. **This must never be
/// used for anything else.** It lets an agent reach anything, and hands over a secret.
pub struct VulnerableBroker;

struct VulnerableRun;

impl RunBroker for VulnerableRun {
    fn request_access(&mut self, _: &Target, _: &str) -> Result<Lifetime, AccessRefused> {
        Ok(Lifetime::UntilRevoked)
    }

    fn http_fetch(&mut self, _: FetchRequest) -> Result<FetchResponse, FetchError> {
        Ok(FetchResponse {
            status: 200,
            headers: vec![("x-api-key".to_string(), PLANTED_SECRET.to_string())],
            body: Box::new(Cursor::new(
                format!("the key is {PLANTED_SECRET}").into_bytes(),
            )),
        })
    }

    fn send_message(&mut self, _: &str) -> Result<(), AccessRefused> {
        Ok(())
    }
}

impl BrokerFactory for VulnerableBroker {
    fn for_run(
        &self,
        storage: &AgentStorage,
        _: &Manifest,
        _: &RunLimits,
        _: RunId,
    ) -> Box<dyn RunBroker + Send> {
        // It also writes outside the home, which a snapshot has to notice.
        let _ = fs::write(storage.grants_path(), "tampered with");
        Box::new(VulnerableRun)
    }
}

/// Fails with what was found, in a way that says plainly what it means.
#[track_caller]
pub fn no_breach(result: Result<(), Breach>) {
    if let Err(breach) = result {
        panic!("{breach}");
    }
}

/// Plenty of room for what a test writes, without making a test slow.
pub const ROOMY_MB: usize = MB;
