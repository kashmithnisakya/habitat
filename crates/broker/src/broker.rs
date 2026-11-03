//! Puts the policy, the grants and the user together, for each request that one run makes.

use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use habitat_core::backend::{
    AccessRefused, BrokerFactory, FetchError, FetchMethod, FetchRequest, FetchResponse, Lifetime,
    ModelAnswer, ModelRequest, PickedFile, RunBroker, RunLimits, Target,
};
use habitat_core::home::AgentStorage;
use habitat_core::manifest::{Manifest, printable};
use habitat_core::platform;

use habitat_core::run_log::RunId;

use crate::approval::{ANSWER_TIME, Answer, Approver, FilePrompt, Prompt, PromptLimit};
use crate::audit::{AuditError, AuditEvent, AuditLog};
use crate::gateway::{
    CheckedUrl, Incoming, LimitedBody, MAX_HOP_TIME, MAX_REDIRECTS, MAX_RESPONSE_BYTES, Network,
    NetworkError, RealNetwork, check_request, check_url, clean_response_headers, follow,
    redirected,
};
use crate::grants::RunGrants;
use crate::model::{self, RunShared};
use crate::policy::{Action, Context, Decision, DenyReason, LeakCheck, decide};
use crate::vault::Vault;

/// Makes a broker for each run, which asks the user through the given shell.
pub struct Broker {
    approver: Arc<dyn Approver>,
    network: Arc<dyn Network>,
    vault: Option<Vault>,
}

impl Broker {
    pub fn new(approver: impl Approver + 'static) -> Self {
        Self::with_network(approver, RealNetwork::new())
    }

    /// The same, using this in place of the real network.
    pub fn with_network(
        approver: impl Approver + 'static,
        network: impl Network + 'static,
    ) -> Self {
        Self {
            approver: Arc::new(approver),
            network: Arc::new(network),
            vault: None,
        }
    }

    /// Lets this use the secrets in a vault for what the core itself sends.
    pub fn with_vault(mut self, vault: Vault) -> Self {
        self.vault = Some(vault);
        self
    }
}

/// How many messages one run can show. An agent can send them as fast as it likes.
pub const MAX_MESSAGES_PER_RUN: u32 = 100;
/// How much of one message is shown, in characters.
const MAX_MESSAGE_CHARS: usize = 4096;

/// Makes a message safe to show. Line breaks and tabs stay, because a long message needs them, and
/// each line is cleaned by itself, with anything else that a terminal would act on replaced. A
/// shell has to mark each line as coming from the agent, so a line break can't fake the host.
fn printable_message(text: &str) -> String {
    let mut shown: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
    let cut = shown.len() < text.len();
    shown = shown
        .replace("\r\n", "\n")
        .split('\n')
        .map(|line| printable(line).replace("\\u{9}", "\t"))
        .collect::<Vec<_>>()
        .join("\n");
    if cut {
        shown.push_str("...");
    }
    shown
}

/// Opens the one file that the user chose. This holds on to it, so an agent can only ever read that
/// file, and it refuses a link, as what it leads to may not be what the user meant.
fn open_picked(path: &Path) -> io::Result<PickedFile> {
    let file = platform::open_no_follow(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let name = path
        .file_name()
        .map(|name| printable(&name.to_string_lossy()))
        .ok_or(io::ErrorKind::InvalidInput)?;
    Ok(PickedFile {
        name,
        size: metadata.len(),
        file: Box::new(file),
    })
}

impl BrokerFactory for Broker {
    fn run_ended(&self, storage: &AgentStorage, run_id: RunId, how: &str) {
        // The run's record has this too, so there's nothing more to do if it can't be logged.
        if let Ok(mut audit) = AuditLog::open(storage, run_id) {
            let _ = audit.append(&AuditEvent::RunEnded {
                how: how.to_string(),
            });
        }
    }

    fn for_run(
        &self,
        storage: &AgentStorage,
        manifest: &Manifest,
        limits: &RunLimits,
        run_id: RunId,
    ) -> Box<dyn RunBroker + Send> {
        Box::new(ForRun {
            shared: RunShared {
                audit: Arc::new(Mutex::new(AuditLog::open(storage, run_id))),
                tokens_left: Arc::new(AtomicU64::new(manifest.limits().model_budget_tokens)),
            },
            vault: self.vault.clone(),
            manifest: manifest.clone(),
            grants: RunGrants::new(storage.clone()),
            approver: Arc::clone(&self.approver),
            network: Arc::clone(&self.network),
            private_read: false,
            reached: HashSet::new(),
            messages_sent: 0,
            limit: PromptLimit::default(),
            deadline: Instant::now() + limits.run_time,
        })
    }
}

/// The broker for one run. Nothing here outlives the run, except a grant until revoked.
struct ForRun {
    shared: RunShared,
    vault: Option<Vault>,
    manifest: Manifest,
    grants: RunGrants,
    approver: Arc<dyn Approver>,
    network: Arc<dyn Network>,
    /// Whether this run has read the user's private data, which only ever goes from no to yes.
    private_read: bool,
    /// What the user has approved since this run read private data.
    reached: HashSet<Target>,
    messages_sent: u32,
    limit: PromptLimit,
    /// When the run's time is up. A call can't be interrupted, so it has to stop by itself.
    deadline: Instant,
}

fn refused(reason: DenyReason) -> AccessRefused {
    match reason {
        DenyReason::OverBudget => AccessRefused::LimitExceeded(reason.to_string()),
        other => AccessRefused::Denied(other.to_string()),
    }
}

impl ForRun {
    /// Records something. If it can't be recorded, nothing else gets to happen.
    fn log(&mut self, event: AuditEvent) -> Result<(), AccessRefused> {
        let mut audit = self
            .shared
            .audit
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let audit = audit.as_mut().map_err(|_| AccessRefused::Internal)?;
        match audit.append(&event) {
            Ok(()) => Ok(()),
            Err(AuditError::TooManyEntries) => Err(AccessRefused::LimitExceeded(
                "this run has made too many requests".to_string(),
            )),
            Err(_) => Err(AccessRefused::Internal),
        }
    }

    fn logged(
        &mut self,
        target: &Target,
        result: Result<Lifetime, AccessRefused>,
    ) -> Result<Lifetime, AccessRefused> {
        let event = match &result {
            Ok(lifetime) => AuditEvent::Allowed {
                target: target.clone(),
                lifetime: *lifetime,
            },
            Err(_) => AuditEvent::Refused {
                target: target.clone(),
            },
        };
        // It's already been decided, so if a yes can't be recorded, it turns into a no.
        self.log(event)?;
        result
    }
}

impl RunBroker for ForRun {
    fn request_access(&mut self, target: &Target, reason: &str) -> Result<Lifetime, AccessRefused> {
        self.log(AuditEvent::Requested {
            target: target.clone(),
        })?;
        let result = self.decide_request(target, reason);
        self.logged(target, result)
    }

    fn send_message(&mut self, text: &str) -> Result<(), AccessRefused> {
        // What's printed is cut short, so this limits what a run can show however long each is.
        if self.messages_sent >= MAX_MESSAGES_PER_RUN {
            return Err(AccessRefused::LimitExceeded(
                "this run has sent too many messages".to_string(),
            ));
        }
        self.log(AuditEvent::MessageSent)?;
        self.messages_sent += 1;
        self.approver
            .show_message(self.manifest.name(), &printable_message(text));
        Ok(())
    }

    fn pick_user_file(&mut self, reason: &str) -> Result<PickedFile, AccessRefused> {
        let result = self.pick_file(reason);
        self.log(if result.is_ok() {
            AuditEvent::UserFilePicked
        } else {
            AuditEvent::UserFileRefused
        })?;
        result
    }

    fn model_complete(
        &mut self,
        request: ModelRequest,
    ) -> Result<Box<dyn ModelAnswer + Send>, FetchError> {
        let target = Target::Model(request.model.clone());
        self.log_for_fetch(AuditEvent::Requested {
            target: target.clone(),
        })?;
        let refused = match self.decide(&Action::Reach(target.clone())) {
            Decision::Allow => None,
            Decision::Ask => Some(FetchError::NotGranted),
            Decision::Deny(DenyReason::OverBudget) => Some(FetchError::LimitExceeded(
                DenyReason::OverBudget.to_string(),
            )),
            Decision::Deny(reason) => Some(FetchError::Denied(reason.to_string())),
        };
        if let Some(refused) = refused {
            self.log_for_fetch(AuditEvent::Refused { target })?;
            return Err(refused);
        }
        self.grants.used(&target);
        let wait = self
            .deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default()
            .min(MAX_HOP_TIME);
        if wait.is_zero() {
            return Err(FetchError::TimedOut);
        }
        model::start(
            self.network.as_ref(),
            self.vault.as_ref(),
            &request,
            self.shared.clone(),
            wait,
        )
    }

    fn http_fetch(&mut self, request: FetchRequest) -> Result<FetchResponse, FetchError> {
        let mut url = check_url(&request.url).map_err(invalid_request)?;
        check_request(&request).map_err(invalid_request)?;
        let mut request = request;
        for _ in 0..=MAX_REDIRECTS {
            let incoming = self.one_hop(&url, &request)?;
            if let Some(location) = redirect_location(&incoming) {
                let next = follow(&url, location).map_err(|_| {
                    FetchError::Denied(
                        "the site redirected somewhere that isn't allowed".to_string(),
                    )
                })?;
                request = redirected(request, incoming.status, next.host == url.host);
                url = next;
                continue;
            }
            return Ok(FetchResponse {
                status: incoming.status,
                headers: clean_response_headers(incoming.headers),
                body: Box::new(LimitedBody {
                    inner: incoming.body,
                    left: MAX_RESPONSE_BYTES,
                }),
            });
        }
        Err(FetchError::LimitExceeded("too many redirects".to_string()))
    }
}

fn invalid_request(reason: &str) -> FetchError {
    FetchError::InvalidRequest(reason.to_string())
}

/// Where a response redirects to, if it does.
fn redirect_location(incoming: &Incoming) -> Option<&str> {
    if !matches!(incoming.status, 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    incoming
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("location"))
        .map(|(_, value)| value.as_str())
}

fn method_name(method: FetchMethod) -> &'static str {
    match method {
        FetchMethod::Get => "GET",
        FetchMethod::Head => "HEAD",
        FetchMethod::Post => "POST",
        FetchMethod::Put => "PUT",
        FetchMethod::Patch => "PATCH",
        FetchMethod::Delete => "DELETE",
    }
}

impl ForRun {
    fn log_for_fetch(&mut self, event: AuditEvent) -> Result<(), FetchError> {
        self.log(event).map_err(|refused| match refused {
            AccessRefused::LimitExceeded(why) => FetchError::LimitExceeded(why),
            _ => FetchError::Internal,
        })
    }

    /// Makes one request to one host, after checking the policy for it, as every hop has to.
    fn one_hop(
        &mut self,
        url: &CheckedUrl,
        request: &FetchRequest,
    ) -> Result<Incoming, FetchError> {
        let target = Target::NetworkHost(url.host.clone());
        self.log_for_fetch(AuditEvent::Requested {
            target: target.clone(),
        })?;
        let refused = match self.decide(&Action::Reach(target.clone())) {
            Decision::Allow => None,
            Decision::Ask => Some(FetchError::NotGranted),
            Decision::Deny(reason) => Some(FetchError::Denied(reason.to_string())),
        };
        if let Some(refused) = refused {
            self.log_for_fetch(AuditEvent::Refused { target })?;
            return Err(refused);
        }
        // A grant for one request is used up from here, whatever happens next.
        self.grants.used(&target);

        let wait = self
            .deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default()
            .min(MAX_HOP_TIME);
        if wait.is_zero() {
            return Err(FetchError::TimedOut);
        }
        let method = method_name(request.method).to_string();
        let result = self.network.send(url, request, &[], wait);
        self.log_for_fetch(AuditEvent::Fetched {
            target,
            method,
            status: result.as_ref().map_or(0, |incoming| incoming.status),
        })?;
        result.map_err(|error| match error {
            NetworkError::TimedOut => FetchError::TimedOut,
            NetworkError::Blocked => {
                FetchError::Denied("that site's address isn't on the public internet".to_string())
            }
            NetworkError::Failed => FetchError::Unavailable("the request failed".to_string()),
        })
    }
}

impl ForRun {
    fn decide(&self, action: &Action) -> Decision {
        let context = Context {
            manifest: &self.manifest,
            grants: &self.grants,
            tokens_left: self.shared.tokens_left.load(Ordering::SeqCst),
            leak_check: if self.private_read {
                LeakCheck::PrivateRead {
                    reached: &self.reached,
                }
            } else {
                LeakCheck::NothingPrivateRead
            },
        };
        decide(&context, action)
    }

    fn pick_file(&mut self, reason: &str) -> Result<PickedFile, AccessRefused> {
        if let Decision::Deny(reason) = self.decide(&Action::ReadUserFile) {
            return Err(refused(reason));
        }
        if !self.limit.may_ask_for_file() {
            return Err(AccessRefused::LimitExceeded(
                "this run can't ask the user about that any more".to_string(),
            ));
        }
        let prompt = FilePrompt::new(self.manifest.name(), reason);
        let Some(path) = self.approver.pick_file(&prompt, ANSWER_TIME) else {
            self.limit.file_refused();
            return Err(AccessRefused::Denied(
                "the user didn't choose a file".to_string(),
            ));
        };
        let picked = open_picked(&path).map_err(|_| {
            AccessRefused::Denied("the file that the user chose couldn't be read".to_string())
        })?;
        // From here on, it's what the data-leak rule is for. This counts even if the agent never
        // reads the file, since that can't be told from here in time.
        self.private_read = true;
        self.reached.clear();
        Ok(picked)
    }

    fn decide_request(&mut self, target: &Target, reason: &str) -> Result<Lifetime, AccessRefused> {
        match self.decide(&Action::Reach(target.clone())) {
            // It already has access, and saying so can't reveal anything that using it wouldn't.
            Decision::Allow => self
                .grants
                .lifetime_of(target)
                .ok_or(AccessRefused::Internal),
            Decision::Deny(reason) => Err(refused(reason)),
            Decision::Ask => {
                if !self.limit.may_ask(target) {
                    return Err(AccessRefused::LimitExceeded(
                        "this run can't ask the user about that any more".to_string(),
                    ));
                }
                let prompt = Prompt::new(self.manifest.name(), target, reason, self.private_read);
                match self.approver.ask(&prompt, ANSWER_TIME) {
                    Answer::Allow(lifetime) => {
                        self.grants
                            .grant(target.clone(), lifetime)
                            .map_err(|_| AccessRefused::Internal)?;
                        // This is the fresh approval that the data-leak rule asks for.
                        self.reached.insert(target.clone());
                        Ok(lifetime)
                    }
                    Answer::Deny => {
                        self.limit.refused(target);
                        Err(AccessRefused::Denied("the user said no".to_string()))
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::io::Read;
    use std::path::PathBuf;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::approval::{MAX_PROMPTS_PER_RUN, NobodyThere};
    use crate::audit::{MAX_ENTRIES_PER_RUN, verify};
    use crate::model::SECRET_NAME;
    use crate::vault::tests::{FakeStore, SECRET};
    use crate::vault::{NewSecret, SecretHeader, SecretValue};
    use habitat_core::backend::{ModelMessage, ModelRole, TokenUsage};

    const MANIFEST: &str = r#"
        [agent]
        name = "tester"
        version = "1.0.0"

        [limits]
        memory_mb = 8
        run_seconds = 1
        disk_mb = 1
        model_budget_tokens = 100

        [requests]
        network = ["a.example", "b.example", "c.example", "d.example", "e.example", "f.example", "g.example"]
        models = ["default"]
        files = "ask"
    "#;

    /// A status, headers and body.
    type FakePage = (u16, Vec<(String, String)>, Vec<u8>);

    /// Answers each host with what it's told to, and remembers what it was asked for.
    #[derive(Clone, Default)]
    struct FakeNetwork {
        pages: Arc<Mutex<HashMap<String, FakePage>>>,
        /// Answers with a body of this many bytes, without holding it all in memory.
        huge_body: Option<u64>,
        sent: Arc<Mutex<Vec<(String, FetchRequest)>>>,
        secrets_sent: Arc<Mutex<Vec<(String, String, String)>>>,
        fails_with: Option<NetworkError>,
    }

    impl FakeNetwork {
        fn page(self, url: &str, status: u16, headers: &[(&str, &str)], body: &[u8]) -> Self {
            let headers = headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect();
            self.pages
                .lock()
                .unwrap()
                .insert(url.to_string(), (status, headers, body.to_vec()));
            self
        }

        fn urls_sent(&self) -> Vec<String> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .map(|(url, _)| url.clone())
                .collect()
        }
    }

    impl Network for FakeNetwork {
        fn send(
            &self,
            url: &CheckedUrl,
            request: &FetchRequest,
            secrets: &[SecretHeader],
            _wait: Duration,
        ) -> Result<Incoming, NetworkError> {
            self.sent
                .lock()
                .unwrap()
                .push((url.to_string(), request.clone()));
            for secret in secrets {
                self.secrets_sent.lock().unwrap().push((
                    url.to_string(),
                    secret.name_for_tests().to_string(),
                    String::from_utf8(secret.value_for_tests().to_vec()).unwrap(),
                ));
            }
            if let Some(error) = self.fails_with {
                return Err(error);
            }
            if let Some(size) = self.huge_body {
                return Ok(Incoming {
                    status: 200,
                    headers: Vec::new(),
                    body: Box::new(std::io::repeat(7).take(size)),
                });
            }
            let pages = self.pages.lock().unwrap();
            let (status, headers, body) =
                pages
                    .get(&url.to_string())
                    .cloned()
                    .unwrap_or((404, vec![], vec![]));
            Ok(Incoming {
                status,
                headers,
                body: Box::new(std::io::Cursor::new(body)),
            })
        }
    }

    /// Gives the same answer each time, and remembers what it was asked.
    #[derive(Clone)]
    struct Scripted {
        answer: Answer,
        asked: Arc<Mutex<Vec<(Prompt, Duration)>>>,
        file: Option<PathBuf>,
        file_prompts: Arc<Mutex<Vec<FilePrompt>>>,
        shown: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl Scripted {
        fn new(answer: Answer) -> Self {
            Self {
                answer,
                asked: Arc::default(),
                file: None,
                file_prompts: Arc::default(),
                shown: Arc::default(),
            }
        }

        fn choosing(answer: Answer, file: &Path) -> Self {
            Self {
                file: Some(file.to_path_buf()),
                ..Self::new(answer)
            }
        }

        fn times_asked(&self) -> usize {
            self.asked.lock().unwrap().len()
        }
    }

    impl Approver for Scripted {
        fn ask(&self, prompt: &Prompt, wait: Duration) -> Answer {
            self.asked.lock().unwrap().push((prompt.clone(), wait));
            self.answer
        }

        fn show_message(&self, agent: &str, text: &str) {
            self.shown
                .lock()
                .unwrap()
                .push((agent.to_string(), text.to_string()));
        }

        fn pick_file(&self, prompt: &FilePrompt, _wait: Duration) -> Option<PathBuf> {
            self.file_prompts.lock().unwrap().push(prompt.clone());
            self.file.clone()
        }
    }

    struct Fixture {
        _root: TempDir,
        storage: AgentStorage,
    }

    impl Fixture {
        fn new() -> Self {
            let root = TempDir::new().unwrap();
            let storage = AgentStorage::open(root.path(), "tester").unwrap();
            Self {
                _root: root,
                storage,
            }
        }

        fn run(&self, approver: impl Approver + 'static) -> Box<dyn RunBroker + Send> {
            let manifest = Manifest::parse(MANIFEST).unwrap();
            let limits = RunLimits {
                memory_bytes: 1,
                run_time: Duration::from_secs(1),
                disk_bytes: 1,
            };
            let run_id = RunId::parse(&"1".repeat(32)).unwrap();
            Broker::with_network(approver, FakeNetwork::default()).for_run(
                &self.storage,
                &manifest,
                &limits,
                run_id,
            )
        }
    }

    fn host(name: &str) -> Target {
        Target::NetworkHost(name.to_string())
    }

    #[test]
    fn something_undeclared_is_denied_without_asking() {
        let fixture = Fixture::new();
        let approver = Scripted::new(Answer::Allow(Lifetime::UntilRevoked));
        let mut broker = fixture.run(approver.clone());
        assert!(matches!(
            broker.request_access(&host("evil.example"), "please"),
            Err(AccessRefused::Denied(_))
        ));
        assert_eq!(approver.times_asked(), 0);
    }

    #[test]
    fn something_malformed_is_denied_without_asking() {
        let fixture = Fixture::new();
        let approver = Scripted::new(Answer::Allow(Lifetime::Once));
        let mut broker = fixture.run(approver.clone());
        assert!(matches!(
            broker.request_access(&host("\x1b[31ma.example"), "please"),
            Err(AccessRefused::Denied(_))
        ));
        assert_eq!(approver.times_asked(), 0);
    }

    #[test]
    fn the_user_is_asked_and_their_answer_counts() {
        for lifetime in [Lifetime::Once, Lifetime::ThisRun, Lifetime::UntilRevoked] {
            let fixture = Fixture::new();
            let approver = Scripted::new(Answer::Allow(lifetime));
            let mut broker = fixture.run(approver.clone());
            assert_eq!(
                broker.request_access(&host("a.example"), "because"),
                Ok(lifetime)
            );
            let asked = approver.asked.lock().unwrap();
            assert_eq!(asked.len(), 1);
            assert_eq!(asked[0].1, ANSWER_TIME);
            assert_eq!(asked[0].0.agent, "tester");
            assert_eq!(asked[0].0.agents_claim, "because");
        }
    }

    #[test]
    fn no_shell_means_no() {
        let fixture = Fixture::new();
        let mut broker = fixture.run(NobodyThere);
        assert!(matches!(
            broker.request_access(&host("a.example"), "please"),
            Err(AccessRefused::Denied(_))
        ));
    }

    #[test]
    fn saying_no_grants_nothing_and_isnt_asked_again() {
        let fixture = Fixture::new();
        let approver = Scripted::new(Answer::Deny);
        let mut broker = fixture.run(approver.clone());
        assert!(matches!(
            broker.request_access(&host("a.example"), "please"),
            Err(AccessRefused::Denied(_))
        ));
        for _ in 0..3 {
            assert!(matches!(
                broker.request_access(&host("a.example"), "please please"),
                Err(AccessRefused::LimitExceeded(_))
            ));
        }
        assert_eq!(approver.times_asked(), 1);
        assert!(!fixture.storage.grants_path().exists());
    }

    #[test]
    fn repeated_requests_stop_prompting_after_the_limit() {
        let fixture = Fixture::new();
        let approver = Scripted::new(Answer::Deny);
        let mut broker = fixture.run(approver.clone());
        for name in ["a", "b", "c", "d", "e", "f", "g"] {
            let _ = broker.request_access(&host(&format!("{name}.example")), "please");
        }
        assert_eq!(approver.times_asked(), MAX_PROMPTS_PER_RUN as usize);
    }

    #[test]
    fn a_new_run_gets_a_new_limit_but_a_saved_grant_needs_no_asking() {
        let fixture = Fixture::new();
        let approver = Scripted::new(Answer::Allow(Lifetime::UntilRevoked));
        let mut first = fixture.run(approver.clone());
        first.request_access(&host("a.example"), "please").unwrap();
        drop(first);

        let mut second = fixture.run(approver.clone());
        assert!(second.request_access(&host("a.example"), "please").is_ok());
        assert_eq!(approver.times_asked(), 1);
        assert!(second.request_access(&host("b.example"), "please").is_ok());
        assert_eq!(approver.times_asked(), 2);
    }

    #[test]
    fn a_short_grant_doesnt_reach_the_next_run() {
        for lifetime in [Lifetime::Once, Lifetime::ThisRun] {
            let fixture = Fixture::new();
            let approver = Scripted::new(Answer::Allow(lifetime));
            fixture
                .run(approver.clone())
                .request_access(&host("a.example"), "please")
                .unwrap();
            fixture
                .run(approver.clone())
                .request_access(&host("a.example"), "please")
                .unwrap();
            assert_eq!(approver.times_asked(), 2, "{lifetime:?}");
        }
    }

    #[test]
    fn every_request_and_decision_is_logged_with_its_run() {
        let fixture = Fixture::new();
        let mut broker = fixture.run(Scripted::new(Answer::Deny));
        let _ = broker.request_access(&host("a.example"), "because");
        let _ = broker.request_access(&host("evil.example"), "because");
        drop(broker);
        let mut broker = fixture.run(Scripted::new(Answer::Allow(Lifetime::Once)));
        broker
            .request_access(&host("b.example"), "because")
            .unwrap();

        let entries = verify(&fixture.storage).unwrap();
        let events: Vec<AuditEvent> = entries.iter().map(|entry| entry.event.clone()).collect();
        assert_eq!(
            events,
            [
                AuditEvent::Requested {
                    target: host("a.example")
                },
                AuditEvent::Refused {
                    target: host("a.example")
                },
                AuditEvent::Requested {
                    target: host("evil.example")
                },
                AuditEvent::Refused {
                    target: host("evil.example")
                },
                AuditEvent::Requested {
                    target: host("b.example")
                },
                AuditEvent::Allowed {
                    target: host("b.example"),
                    lifetime: Lifetime::Once
                },
            ]
        );
        assert!(entries.iter().all(|entry| entry.run == "1".repeat(32)));
    }

    #[test]
    fn what_the_agent_says_isnt_logged() {
        let fixture = Fixture::new();
        let mut broker = fixture.run(Scripted::new(Answer::Deny));
        let _ = broker.request_access(&host("a.example"), "sk-ant-SECRET-KEY");
        let text = fs::read_to_string(fixture.storage.audit_path()).unwrap();
        assert!(!text.contains("SECRET"), "{text}");
    }

    #[test]
    fn a_request_that_cant_be_logged_is_refused_without_asking() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.storage.audit_path()).unwrap();
        let approver = Scripted::new(Answer::Allow(Lifetime::UntilRevoked));
        let mut broker = fixture.run(approver.clone());
        assert_eq!(
            broker.request_access(&host("a.example"), "please"),
            Err(AccessRefused::Internal)
        );
        assert_eq!(approver.times_asked(), 0);
        assert!(!fixture.storage.grants_path().exists());
    }

    #[test]
    fn a_request_is_refused_if_logging_starts_failing_part_way_through_a_run() {
        let fixture = Fixture::new();
        let approver = Scripted::new(Answer::Allow(Lifetime::UntilRevoked));
        let mut broker = fixture.run(approver.clone());
        broker.request_access(&host("a.example"), "please").unwrap();
        // A folder where the next write of the log's position goes, so that it can't happen.
        let blocked = fixture.storage.audit_path().with_extension("head-new");
        fs::create_dir(&blocked).unwrap();
        assert_eq!(
            broker.request_access(&host("b.example"), "please"),
            Err(AccessRefused::Internal)
        );
        assert_eq!(approver.times_asked(), 1);
    }

    #[test]
    fn a_flood_of_requests_stops_at_the_limit() {
        let fixture = Fixture::new();
        let manifest = Manifest::parse(MANIFEST).unwrap();
        let run_id = RunId::parse(&"1".repeat(32)).unwrap();
        let mut audit = AuditLog::open(&fixture.storage, run_id).unwrap();
        audit.skip_ahead(MAX_ENTRIES_PER_RUN - 1);
        let mut broker = ForRun {
            shared: RunShared {
                audit: Arc::new(Mutex::new(Ok(audit))),
                tokens_left: Arc::new(AtomicU64::new(0)),
            },
            vault: None,
            manifest: manifest.clone(),
            grants: RunGrants::new(fixture.storage.clone()),
            approver: Arc::new(Scripted::new(Answer::Allow(Lifetime::UntilRevoked))),
            network: Arc::new(FakeNetwork::default()),
            private_read: false,
            reached: HashSet::new(),
            messages_sent: 0,
            limit: PromptLimit::default(),
            deadline: Instant::now() + Duration::from_secs(60),
        };
        // This takes the last entry to record the request, and the next to say that's the limit.
        assert!(matches!(
            broker.request_access(&host("a.example"), "please"),
            Err(AccessRefused::LimitExceeded(_))
        ));
        assert!(matches!(
            broker.request_access(&host("a.example"), "please"),
            Err(AccessRefused::LimitExceeded(_))
        ));
        // The log says that it stopped, and it still checks out.
        let entries = verify(&fixture.storage).unwrap();
        assert!(matches!(
            entries[entries.len() - 1].event,
            AuditEvent::LimitReached
        ));
    }

    #[test]
    fn an_agent_is_told_how_long_its_access_lasts() {
        let fixture = Fixture::new();
        let mut broker = fixture.run(Scripted::new(Answer::Allow(Lifetime::UntilRevoked)));
        assert_eq!(
            broker.request_access(&host("a.example"), "please"),
            Ok(Lifetime::UntilRevoked)
        );
        // It already has this, so nobody's asked, and the answer's the same.
        assert_eq!(
            broker.request_access(&host("a.example"), "please"),
            Ok(Lifetime::UntilRevoked)
        );
        let mut broker = fixture.run(NobodyThere);
        assert_eq!(
            broker.request_access(&host("a.example"), "please"),
            Ok(Lifetime::UntilRevoked)
        );
    }

    fn fetch_setup(network: FakeNetwork) -> (Fixture, Box<dyn RunBroker + Send>) {
        fetch_setup_with(
            network,
            Answer::Allow(Lifetime::UntilRevoked),
            Duration::from_secs(60),
        )
    }

    fn fetch_setup_with(
        network: FakeNetwork,
        answer: Answer,
        run_time: Duration,
    ) -> (Fixture, Box<dyn RunBroker + Send>) {
        let fixture = Fixture::new();
        let manifest = Manifest::parse(MANIFEST).unwrap();
        let limits = RunLimits {
            memory_bytes: 1,
            run_time,
            disk_bytes: 1,
        };
        let run_id = RunId::parse(&"1".repeat(32)).unwrap();
        let broker = Broker::with_network(Scripted::new(answer), network).for_run(
            &fixture.storage,
            &manifest,
            &limits,
            run_id,
        );
        (fixture, broker)
    }

    fn get(url: &str) -> FetchRequest {
        FetchRequest {
            method: FetchMethod::Get,
            url: url.to_string(),
            headers: Vec::new(),
            body: None,
        }
    }

    fn fetch_events(fixture: &Fixture) -> Vec<AuditEvent> {
        verify(&fixture.storage)
            .unwrap()
            .into_iter()
            .map(|entry| entry.event)
            .collect()
    }

    #[test]
    fn a_fetch_without_a_grant_says_so_and_sends_nothing() {
        let network = FakeNetwork::default();
        let (_fixture, mut broker) = fetch_setup(network.clone());
        assert_eq!(
            broker.http_fetch(get("https://a.example/")).err(),
            Some(FetchError::NotGranted)
        );
        assert!(network.urls_sent().is_empty());
    }

    #[test]
    fn a_fetch_of_something_undeclared_is_denied_and_sends_nothing() {
        let network = FakeNetwork::default();
        let (_fixture, mut broker) = fetch_setup(network.clone());
        broker.request_access(&host("a.example"), "please").unwrap();
        assert!(matches!(
            broker.http_fetch(get("https://evil.example/")),
            Err(FetchError::Denied(_))
        ));
        assert!(network.urls_sent().is_empty());
    }

    #[test]
    fn a_bad_url_or_header_sends_nothing() {
        let network = FakeNetwork::default();
        let (_fixture, mut broker) = fetch_setup(network.clone());
        broker.request_access(&host("a.example"), "please").unwrap();
        for url in [
            "http://a.example/",
            "https://user@a.example/",
            "https://a.example:8443/",
        ] {
            assert!(
                matches!(
                    broker.http_fetch(get(url)),
                    Err(FetchError::InvalidRequest(_))
                ),
                "{url}"
            );
        }
        let mut request = get("https://a.example/");
        request
            .headers
            .push(("Host".to_string(), "evil.example".to_string()));
        assert!(matches!(
            broker.http_fetch(request),
            Err(FetchError::InvalidRequest(_))
        ));
        assert!(network.urls_sent().is_empty());
    }

    #[test]
    fn an_allowed_fetch_returns_the_page_and_is_logged() {
        let network = FakeNetwork::default().page(
            "https://a.example/page?secret=1",
            200,
            &[("Content-Type", "text/plain"), ("Set-Cookie", "session=x")],
            b"hello",
        );
        let (fixture, mut broker) = fetch_setup(network);
        broker.request_access(&host("a.example"), "please").unwrap();
        let mut response = broker
            .http_fetch(get("https://a.example/page?secret=1"))
            .ok()
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(
            response.headers,
            [("Content-Type".to_string(), "text/plain".to_string())]
        );
        let mut body = String::new();
        response.body.read_to_string(&mut body).unwrap();
        assert_eq!(body, "hello");

        assert!(fetch_events(&fixture).contains(&AuditEvent::Fetched {
            target: host("a.example"),
            method: "GET".to_string(),
            status: 200,
        }));
        // The path and query aren't logged, as they can hold private data.
        let text = fs::read_to_string(fixture.storage.audit_path()).unwrap();
        assert!(!text.contains("secret"), "{text}");
    }

    #[test]
    fn a_redirect_to_an_approved_host_is_followed_and_each_hop_checked() {
        let network = FakeNetwork::default()
            .page(
                "https://a.example/",
                302,
                &[("Location", "https://b.example/next")],
                b"",
            )
            .page("https://b.example/next", 200, &[], b"arrived");
        let (fixture, mut broker) = fetch_setup(network.clone());
        broker.request_access(&host("a.example"), "please").unwrap();
        broker.request_access(&host("b.example"), "please").unwrap();
        let response = broker.http_fetch(get("https://a.example/")).ok().unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(
            network.urls_sent(),
            ["https://a.example/", "https://b.example/next"]
        );
        let events = fetch_events(&fixture);
        for name in ["a.example", "b.example"] {
            assert!(events.contains(&AuditEvent::Requested { target: host(name) }));
        }
    }

    #[test]
    fn a_redirect_to_an_unapproved_host_goes_no_further() {
        let network = FakeNetwork::default().page(
            "https://a.example/",
            302,
            &[("Location", "https://b.example/")],
            b"",
        );
        let (fixture, mut broker) = fetch_setup(network.clone());
        broker.request_access(&host("a.example"), "please").unwrap();
        assert_eq!(
            broker.http_fetch(get("https://a.example/")).err(),
            Some(FetchError::NotGranted)
        );
        assert_eq!(network.urls_sent(), ["https://a.example/"]);
        assert!(fetch_events(&fixture).contains(&AuditEvent::Refused {
            target: host("b.example")
        }));
    }

    #[test]
    fn a_redirect_to_an_undeclared_host_is_denied() {
        let network = FakeNetwork::default().page(
            "https://a.example/",
            302,
            &[("Location", "https://evil.example/")],
            b"",
        );
        let (_fixture, mut broker) = fetch_setup(network.clone());
        broker.request_access(&host("a.example"), "please").unwrap();
        assert!(matches!(
            broker.http_fetch(get("https://a.example/")),
            Err(FetchError::Denied(_))
        ));
        assert_eq!(network.urls_sent().len(), 1);
    }

    #[test]
    fn redirects_to_anything_but_https_are_denied() {
        for location in [
            "http://a.example/",
            "file:///etc/passwd",
            "//evil.example/",
            "https://127.0.0.1/",
        ] {
            let network = FakeNetwork::default().page(
                "https://a.example/",
                302,
                &[("Location", location)],
                b"",
            );
            let (_fixture, mut broker) = fetch_setup(network.clone());
            broker.request_access(&host("a.example"), "please").unwrap();
            assert!(
                matches!(
                    broker.http_fetch(get("https://a.example/")),
                    Err(FetchError::Denied(_))
                ),
                "{location}"
            );
            assert_eq!(network.urls_sent().len(), 1, "{location}");
        }
    }

    #[test]
    fn a_grant_for_one_request_only_covers_one_hop() {
        let network = FakeNetwork::default()
            .page("https://a.example/", 302, &[("Location", "/next")], b"")
            .page("https://a.example/next", 200, &[], b"");
        let (_fixture, mut broker) = fetch_setup_with(
            network.clone(),
            Answer::Allow(Lifetime::Once),
            Duration::from_secs(60),
        );
        broker.request_access(&host("a.example"), "please").unwrap();
        assert_eq!(
            broker.http_fetch(get("https://a.example/")).err(),
            Some(FetchError::NotGranted)
        );
        assert_eq!(network.urls_sent(), ["https://a.example/"]);
    }

    #[test]
    fn redirect_loops_stop_at_the_limit() {
        let network = FakeNetwork::default().page(
            "https://a.example/",
            302,
            &[("Location", "https://a.example/")],
            b"",
        );
        let (_fixture, mut broker) = fetch_setup(network.clone());
        broker.request_access(&host("a.example"), "please").unwrap();
        assert!(matches!(
            broker.http_fetch(get("https://a.example/")),
            Err(FetchError::LimitExceeded(_))
        ));
        assert_eq!(network.urls_sent().len(), MAX_REDIRECTS as usize + 1);
    }

    #[test]
    fn a_redirect_with_nowhere_to_go_is_just_a_response() {
        let network = FakeNetwork::default().page("https://a.example/", 302, &[], b"odd");
        let (_fixture, mut broker) = fetch_setup(network);
        broker.request_access(&host("a.example"), "please").unwrap();
        assert_eq!(
            broker
                .http_fetch(get("https://a.example/"))
                .ok()
                .unwrap()
                .status,
            302
        );
    }

    #[test]
    fn a_redirect_to_another_host_doesnt_carry_credentials() {
        let network = FakeNetwork::default()
            .page(
                "https://a.example/",
                302,
                &[("Location", "https://b.example/")],
                b"",
            )
            .page("https://b.example/", 200, &[], b"");
        let (_fixture, mut broker) = fetch_setup(network.clone());
        broker.request_access(&host("a.example"), "please").unwrap();
        broker.request_access(&host("b.example"), "please").unwrap();
        let mut request = get("https://a.example/");
        request
            .headers
            .push(("Authorization".to_string(), "secret".to_string()));
        assert!(broker.http_fetch(request).is_ok());
        let sent = network.sent.lock().unwrap();
        assert_eq!(sent[0].1.headers.len(), 1);
        assert!(sent[1].1.headers.is_empty());
    }

    #[test]
    fn an_oversized_response_ends_in_an_error_and_one_at_the_limit_doesnt() {
        for (size, ok) in [(MAX_RESPONSE_BYTES, true), (MAX_RESPONSE_BYTES + 1, false)] {
            let network = FakeNetwork {
                huge_body: Some(size),
                ..FakeNetwork::default()
            };
            let (_fixture, mut broker) = fetch_setup(network);
            broker.request_access(&host("a.example"), "please").unwrap();
            let mut response = broker.http_fetch(get("https://a.example/")).ok().unwrap();
            let copied = std::io::copy(&mut response.body, &mut std::io::sink());
            if ok {
                assert_eq!(copied.unwrap(), size);
            } else {
                assert_eq!(copied.unwrap_err().kind(), std::io::ErrorKind::FileTooLarge);
            }
        }
    }

    #[test]
    fn network_failures_are_reported_and_logged() {
        for (error, check) in [
            (NetworkError::TimedOut, FetchError::TimedOut),
            (
                NetworkError::Failed,
                FetchError::Unavailable("the request failed".to_string()),
            ),
            (
                NetworkError::Blocked,
                FetchError::Denied("that site's address isn't on the public internet".to_string()),
            ),
        ] {
            let network = FakeNetwork {
                fails_with: Some(error),
                ..FakeNetwork::default()
            };
            let (fixture, mut broker) = fetch_setup(network);
            broker.request_access(&host("a.example"), "please").unwrap();
            assert_eq!(
                broker.http_fetch(get("https://a.example/")).err(),
                Some(check)
            );
            assert!(fetch_events(&fixture).contains(&AuditEvent::Fetched {
                target: host("a.example"),
                method: "GET".to_string(),
                status: 0,
            }));
        }
    }

    #[test]
    fn a_fetch_after_the_runs_time_is_up_sends_nothing() {
        let network = FakeNetwork::default().page("https://a.example/", 200, &[], b"");
        let (_fixture, mut broker) = fetch_setup_with(
            network.clone(),
            Answer::Allow(Lifetime::ThisRun),
            Duration::ZERO,
        );
        broker.request_access(&host("a.example"), "please").unwrap();
        assert_eq!(
            broker.http_fetch(get("https://a.example/")).err(),
            Some(FetchError::TimedOut)
        );
        assert!(network.urls_sent().is_empty());
    }

    struct WithFile {
        _folder: TempDir,
        path: PathBuf,
    }

    fn a_private_file(contents: &str) -> WithFile {
        let folder = TempDir::new().unwrap();
        let path = folder.path().join("diary.txt");
        fs::write(&path, contents).unwrap();
        WithFile {
            _folder: folder,
            path,
        }
    }

    #[test]
    fn the_user_chooses_a_file_and_the_agent_gets_only_that() {
        let private = a_private_file("private words");
        let fixture = Fixture::new();
        let approver = Scripted::choosing(Answer::Deny, &private.path);
        let mut broker = fixture.run(approver.clone());
        let mut picked = broker.pick_user_file("to summarise it").ok().unwrap();
        assert_eq!(picked.name, "diary.txt");
        assert_eq!(picked.size, 13);
        let mut contents = String::new();
        picked.file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "private words");
        let prompts = approver.file_prompts.lock().unwrap();
        assert_eq!(prompts[0].agents_claim, "to summarise it");
    }

    #[test]
    fn nobody_choosing_a_file_means_no_and_isnt_asked_again() {
        let fixture = Fixture::new();
        let approver = Scripted::new(Answer::Deny);
        let mut broker = fixture.run(approver.clone());
        assert!(matches!(
            broker.pick_user_file("please"),
            Err(AccessRefused::Denied(_))
        ));
        assert!(matches!(
            broker.pick_user_file("please"),
            Err(AccessRefused::LimitExceeded(_))
        ));
        assert_eq!(approver.file_prompts.lock().unwrap().len(), 1);
    }

    #[test]
    fn asking_for_a_file_is_denied_if_its_not_declared() {
        let private = a_private_file("x");
        let fixture = Fixture::new();
        let manifest = Manifest::parse(
            "[agent]\nname = \"tester\"\nversion = \"1.0.0\"\n\n[limits]\nmemory_mb = 8\nrun_seconds = 1\ndisk_mb = 1\n",
        )
        .unwrap();
        let limits = RunLimits {
            memory_bytes: 1,
            run_time: Duration::from_secs(1),
            disk_bytes: 1,
        };
        let approver = Scripted::choosing(Answer::Deny, &private.path);
        let mut broker = Broker::with_network(approver.clone(), FakeNetwork::default()).for_run(
            &fixture.storage,
            &manifest,
            &limits,
            RunId::parse(&"1".repeat(32)).unwrap(),
        );
        assert!(matches!(
            broker.pick_user_file("please"),
            Err(AccessRefused::Denied(_))
        ));
        assert!(approver.file_prompts.lock().unwrap().is_empty());
    }

    #[test]
    fn a_link_or_a_folder_isnt_opened_and_errors_dont_reveal_the_path() {
        let private = a_private_file("private");
        let folder = TempDir::new().unwrap();
        let link = folder.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&private.path, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&private.path, &link).unwrap();
        for chosen in [
            link.as_path(),
            folder.path(),
            Path::new("/no/such/secret-path"),
        ] {
            let fixture = Fixture::new();
            let mut broker = fixture.run(Scripted::choosing(Answer::Deny, chosen));
            let error = broker.pick_user_file("please").err().unwrap();
            let text = format!("{error:?}");
            assert!(
                !text.contains("secret-path") && !text.contains("link"),
                "{text}"
            );
        }
    }

    #[test]
    fn neither_the_name_nor_the_path_of_a_file_is_logged() {
        let private = a_private_file("private words");
        let fixture = Fixture::new();
        let mut broker = fixture.run(Scripted::choosing(Answer::Deny, &private.path));
        assert!(broker.pick_user_file("please").is_ok());
        let text = fs::read_to_string(fixture.storage.audit_path()).unwrap();
        assert!(text.contains("user-file-picked"), "{text}");
        assert!(!text.contains("diary"), "{text}");
    }

    #[test]
    fn after_a_private_read_a_granted_host_needs_fresh_approval() {
        let private = a_private_file("private words");
        let network = FakeNetwork::default().page("https://a.example/", 200, &[], b"");
        let fixture = Fixture::new();
        let approver = Scripted::choosing(Answer::Allow(Lifetime::UntilRevoked), &private.path);
        let manifest = Manifest::parse(MANIFEST).unwrap();
        let limits = RunLimits {
            memory_bytes: 1,
            run_time: Duration::from_secs(60),
            disk_bytes: 1,
        };
        let run_id = RunId::parse(&"1".repeat(32)).unwrap();
        let factory = Broker::with_network(approver.clone(), network.clone());
        let mut broker = factory.for_run(&fixture.storage, &manifest, &limits, run_id);

        // A long-lived grant, used as normal.
        broker.request_access(&host("a.example"), "please").unwrap();
        assert!(broker.http_fetch(get("https://a.example/")).is_ok());
        assert_eq!(approver.times_asked(), 1);

        // It reads something private, and that grant isn't enough any more.
        broker.pick_user_file("please").ok().unwrap();
        assert_eq!(
            broker.http_fetch(get("https://a.example/")).err(),
            Some(FetchError::NotGranted)
        );
        assert_eq!(network.urls_sent().len(), 1);

        // It has to ask, the user sees why, and then that one destination can be used.
        broker
            .request_access(&host("a.example"), "to send it")
            .unwrap();
        {
            let asked = approver.asked.lock().unwrap();
            assert_eq!(asked.len(), 2);
            assert!(asked[1].0.after_private_read);
            assert!(!asked[0].0.after_private_read);
        }
        assert!(broker.http_fetch(get("https://a.example/")).is_ok());

        // Another granted host still needs its own approval, in the same run.
        fixture_grant(&fixture, "b.example");
        assert_eq!(
            broker.http_fetch(get("https://b.example/")).err(),
            Some(FetchError::NotGranted)
        );
    }

    fn fixture_grant(fixture: &Fixture, name: &str) {
        let mut grants = RunGrants::new(fixture.storage.clone());
        grants.grant(host(name), Lifetime::UntilRevoked).unwrap();
    }

    #[test]
    fn a_refused_approval_after_a_private_read_leaves_the_host_unreachable() {
        let private = a_private_file("private words");
        let network = FakeNetwork::default().page("https://a.example/", 200, &[], b"");
        let fixture = Fixture::new();
        fixture_grant(&fixture, "a.example");
        let manifest = Manifest::parse(MANIFEST).unwrap();
        let limits = RunLimits {
            memory_bytes: 1,
            run_time: Duration::from_secs(60),
            disk_bytes: 1,
        };
        let run_id = RunId::parse(&"1".repeat(32)).unwrap();
        let factory = Broker::with_network(
            Scripted::choosing(Answer::Deny, &private.path),
            network.clone(),
        );
        let mut broker = factory.for_run(&fixture.storage, &manifest, &limits, run_id);
        broker.pick_user_file("please").ok().unwrap();
        assert!(matches!(
            broker.request_access(&host("a.example"), "to send it"),
            Err(AccessRefused::Denied(_))
        ));
        assert_eq!(
            broker.http_fetch(get("https://a.example/")).err(),
            Some(FetchError::NotGranted)
        );
        assert!(network.urls_sent().is_empty());
    }

    #[test]
    fn a_redirect_after_a_private_read_needs_approval_too() {
        let private = a_private_file("private words");
        let network = FakeNetwork::default()
            .page(
                "https://a.example/",
                302,
                &[("Location", "https://b.example/")],
                b"",
            )
            .page("https://b.example/", 200, &[], b"");
        let fixture = Fixture::new();
        fixture_grant(&fixture, "b.example");
        let manifest = Manifest::parse(MANIFEST).unwrap();
        let limits = RunLimits {
            memory_bytes: 1,
            run_time: Duration::from_secs(60),
            disk_bytes: 1,
        };
        let run_id = RunId::parse(&"1".repeat(32)).unwrap();
        let factory = Broker::with_network(
            Scripted::choosing(Answer::Allow(Lifetime::ThisRun), &private.path),
            network.clone(),
        );
        let mut broker = factory.for_run(&fixture.storage, &manifest, &limits, run_id);
        broker.pick_user_file("please").ok().unwrap();
        broker
            .request_access(&host("a.example"), "to send it")
            .unwrap();
        assert_eq!(
            broker.http_fetch(get("https://a.example/")).err(),
            Some(FetchError::NotGranted)
        );
        assert_eq!(network.urls_sent(), ["https://a.example/"]);
    }

    #[test]
    fn the_rule_starts_fresh_with_each_run() {
        let private = a_private_file("private words");
        let network = FakeNetwork::default().page("https://a.example/", 200, &[], b"");
        let fixture = Fixture::new();
        fixture_grant(&fixture, "a.example");
        let manifest = Manifest::parse(MANIFEST).unwrap();
        let limits = RunLimits {
            memory_bytes: 1,
            run_time: Duration::from_secs(60),
            disk_bytes: 1,
        };
        let run_id = RunId::parse(&"1".repeat(32)).unwrap();
        let factory =
            Broker::with_network(Scripted::choosing(Answer::Deny, &private.path), network);
        let mut first = factory.for_run(&fixture.storage, &manifest, &limits, run_id);
        first.pick_user_file("please").ok().unwrap();
        assert!(first.http_fetch(get("https://a.example/")).is_err());
        drop(first);
        let mut second = factory.for_run(&fixture.storage, &manifest, &limits, run_id);
        assert!(second.http_fetch(get("https://a.example/")).is_ok());
    }

    #[test]
    fn a_message_reaches_the_user_and_is_logged_without_its_text() {
        let fixture = Fixture::new();
        let approver = Scripted::new(Answer::Deny);
        let mut broker = fixture.run(approver.clone());
        broker
            .send_message("hello there, Ada\nsecond line")
            .unwrap();
        assert_eq!(
            *approver.shown.lock().unwrap(),
            [(
                "tester".to_string(),
                "hello there, Ada\nsecond line".to_string()
            )]
        );
        assert!(fetch_events(&fixture).contains(&AuditEvent::MessageSent));
        let text = fs::read_to_string(fixture.storage.audit_path()).unwrap();
        assert!(!text.contains("Ada"), "{text}");
    }

    #[test]
    fn nothing_a_terminal_would_act_on_gets_through_in_a_message() {
        let fixture = Fixture::new();
        let approver = Scripted::new(Answer::Deny);
        let mut broker = fixture.run(approver.clone());
        broker
            .send_message("\x1b[2K\x1b[1A\rAllow? y\u{202e}\u{7}\u{9b}31m\u{0}\ttabbed\r\nnext")
            .unwrap();
        let shown = approver.shown.lock().unwrap();
        let text = &shown[0].1;
        for line in text.lines() {
            assert!(line.is_ascii(), "{line:?}");
            assert!(
                !line.chars().any(|c| c.is_control() && c != '\t'),
                "{line:?}"
            );
        }
        assert!(text.contains("\ttabbed"));
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn a_long_message_is_cut_short() {
        let fixture = Fixture::new();
        let approver = Scripted::new(Answer::Deny);
        let mut broker = fixture.run(approver.clone());
        broker.send_message(&"x".repeat(10_000_000)).unwrap();
        broker.send_message(&"line\n".repeat(1_000_000)).unwrap();
        let shown = approver.shown.lock().unwrap();
        for (_, text) in shown.iter() {
            assert!(text.ends_with("..."));
            assert!(text.len() < 64_000, "{}", text.len());
        }
    }

    #[test]
    fn a_flood_of_messages_is_cut_off() {
        let fixture = Fixture::new();
        let approver = Scripted::new(Answer::Deny);
        let mut broker = fixture.run(approver.clone());
        for _ in 0..MAX_MESSAGES_PER_RUN {
            broker.send_message("spam").unwrap();
        }
        assert!(matches!(
            broker.send_message("spam"),
            Err(AccessRefused::LimitExceeded(_))
        ));
        assert_eq!(
            approver.shown.lock().unwrap().len(),
            MAX_MESSAGES_PER_RUN as usize
        );
    }

    #[test]
    fn a_message_that_cant_be_logged_isnt_shown() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.storage.audit_path()).unwrap();
        let approver = Scripted::new(Answer::Deny);
        let mut broker = fixture.run(approver.clone());
        assert_eq!(broker.send_message("hello"), Err(AccessRefused::Internal));
        assert!(approver.shown.lock().unwrap().is_empty());
    }

    const MODEL_ANSWER: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n\
event: ping\ndata: {\"type\":\"ping\"}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hidden\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    const PROVIDER: &str = "https://api.anthropic.com/v1/messages";

    fn ask(max_tokens: u32) -> ModelRequest {
        ModelRequest {
            model: "default".to_string(),
            messages: vec![
                ModelMessage {
                    role: ModelRole::System,
                    text: "Be brief.".to_string(),
                },
                ModelMessage {
                    role: ModelRole::User,
                    text: "PRIVATE-QUESTION".to_string(),
                },
            ],
            max_tokens,
        }
    }

    struct ModelSetup {
        fixture: Fixture,
        _root: TempDir,
        broker: Box<dyn RunBroker + Send>,
        store: FakeStore,
    }

    /// A run with a key in its vault, and its model approved.
    fn model_setup(network: FakeNetwork, budget: u64, with_key: bool) -> ModelSetup {
        let fixture = Fixture::new();
        let root = TempDir::new().unwrap();
        let store = FakeStore::default();
        let vault = Vault::new(root.path(), store.clone());
        if with_key {
            vault
                .add(NewSecret {
                    name: SECRET_NAME,
                    hosts: &["api.anthropic.com".to_string()],
                    header: "x-api-key",
                    prefix: "",
                    value: SecretValue::new(SECRET.as_bytes().to_vec()),
                })
                .unwrap();
        }
        let manifest = Manifest::parse(&format!(
            "[agent]\nname = \"tester\"\nversion = \"1.0.0\"\n\n[limits]\nmemory_mb = 8\nrun_seconds = 60\ndisk_mb = 1\nmodel_budget_tokens = {budget}\n\n[requests]\nmodels = [\"default\"]\nnetwork = [\"a.example\"]\n"
        ))
        .unwrap();
        let limits = RunLimits {
            memory_bytes: 1,
            run_time: Duration::from_secs(60),
            disk_bytes: 1,
        };
        let mut broker =
            Broker::with_network(Scripted::new(Answer::Allow(Lifetime::ThisRun)), network)
                .with_vault(vault)
                .for_run(
                    &fixture.storage,
                    &manifest,
                    &limits,
                    RunId::parse(&"1".repeat(32)).unwrap(),
                );
        broker
            .request_access(&Target::Model("default".to_string()), "please")
            .unwrap();
        ModelSetup {
            fixture,
            _root: root,
            broker,
            store,
        }
    }

    fn read_answer(answer: &mut Box<dyn ModelAnswer + Send>) -> String {
        let mut text = String::new();
        while let Some(piece) = answer.next().unwrap() {
            text.push_str(&piece);
        }
        text
    }

    #[test]
    fn an_agent_gets_an_answer_without_ever_holding_the_key() {
        let network = FakeNetwork::default().page(PROVIDER, 200, &[], MODEL_ANSWER.as_bytes());
        let mut setup = model_setup(network.clone(), 1000, true);
        let mut answer = setup.broker.model_complete(ask(100)).ok().unwrap();
        assert_eq!(answer.usage(), None, "not until the end");
        assert_eq!(read_answer(&mut answer), "Hello there");
        assert_eq!(
            answer.usage(),
            Some(TokenUsage {
                input_tokens: 10,
                output_tokens: 7,
            })
        );

        // The core added the key, in its own header, for that one host.
        assert_eq!(
            *network.secrets_sent.lock().unwrap(),
            [(
                PROVIDER.to_string(),
                "x-api-key".to_string(),
                SECRET.to_string()
            )]
        );
        // And it isn't in what the request itself holds, nor in the log.
        let sent = network.sent.lock().unwrap();
        let request = format!("{:?}", sent[0].1);
        assert!(!request.contains(SECRET), "{request}");
        let body = String::from_utf8(sent[0].1.body.clone().unwrap()).unwrap();
        assert!(!body.contains(SECRET));
        drop(answer);
        let log = fs::read_to_string(setup.fixture.storage.audit_path()).unwrap();
        assert!(!log.contains(SECRET), "{log}");
        assert!(setup.store.values.lock().unwrap().contains_key(SECRET_NAME));
    }

    #[test]
    fn the_request_is_what_the_api_expects() {
        let network = FakeNetwork::default().page(PROVIDER, 200, &[], MODEL_ANSWER.as_bytes());
        let mut setup = model_setup(network.clone(), 1000, true);
        let _ = setup.broker.model_complete(ask(100)).ok().unwrap();
        let sent = network.sent.lock().unwrap();
        let request = &sent[0].1;
        assert_eq!(request.method, FetchMethod::Post);
        let header = |name: &str| {
            request
                .headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(header("anthropic-version"), Some("2026-10-21"));
        assert_eq!(
            header("anthropic-beta"),
            Some("server-side-fallback-2026-07-01")
        );
        let body: serde_json::Value =
            serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["model"], "claude-opus-5-5");
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["stream"], true);
        assert_eq!(body["system"], "Be brief.");
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "PRIVATE-QUESTION");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn the_budget_limits_each_request_and_stops_new_ones() {
        let network = FakeNetwork::default().page(PROVIDER, 200, &[], MODEL_ANSWER.as_bytes());
        // The answer uses 17 tokens, of 20.
        let mut setup = model_setup(network.clone(), 20, true);
        let mut answer = setup.broker.model_complete(ask(1000)).ok().unwrap();
        read_answer(&mut answer);
        // It's capped at what's left, whatever it asked for.
        {
            let sent = network.sent.lock().unwrap();
            let body: serde_json::Value =
                serde_json::from_slice(sent[0].1.body.as_ref().unwrap()).unwrap();
            assert_eq!(body["max_tokens"], 20);
        }
        let mut second = setup.broker.model_complete(ask(1000)).ok().unwrap();
        {
            let sent = network.sent.lock().unwrap();
            let body: serde_json::Value =
                serde_json::from_slice(sent[1].1.body.as_ref().unwrap()).unwrap();
            assert_eq!(body["max_tokens"], 3);
        }
        read_answer(&mut second);
        // Now it's used more than it had, and nothing more goes out.
        let calls = network.urls_sent().len();
        assert!(matches!(
            setup.broker.model_complete(ask(1)),
            Err(FetchError::LimitExceeded(_))
        ));
        assert_eq!(network.urls_sent().len(), calls);
    }

    #[test]
    fn an_answer_thats_given_up_on_still_counts_and_is_logged() {
        let network = FakeNetwork::default().page(PROVIDER, 200, &[], MODEL_ANSWER.as_bytes());
        let mut setup = model_setup(network, 1000, true);
        let mut answer = setup.broker.model_complete(ask(100)).ok().unwrap();
        assert_eq!(answer.next().unwrap().as_deref(), Some("Hello"));
        drop(answer);
        let events = fetch_events(&setup.fixture);
        assert!(
            events.contains(&AuditEvent::ModelUsed {
                model: "default".to_string(),
                input_tokens: 10,
                output_tokens: 1,
            }),
            "{events:?}"
        );
    }

    #[test]
    fn a_models_use_is_logged_without_what_was_said() {
        let network = FakeNetwork::default().page(PROVIDER, 200, &[], MODEL_ANSWER.as_bytes());
        let mut setup = model_setup(network, 1000, true);
        let mut answer = setup.broker.model_complete(ask(100)).ok().unwrap();
        read_answer(&mut answer);
        let events = fetch_events(&setup.fixture);
        assert!(events.contains(&AuditEvent::ModelUsed {
            model: "default".to_string(),
            input_tokens: 10,
            output_tokens: 7,
        }));
        let log = fs::read_to_string(setup.fixture.storage.audit_path()).unwrap();
        for private in ["PRIVATE-QUESTION", "Be brief", "Hello"] {
            assert!(!log.contains(private), "{log}");
        }
    }

    #[test]
    fn without_a_key_it_says_so_and_sends_nothing() {
        let network = FakeNetwork::default();
        let mut setup = model_setup(network.clone(), 1000, false);
        let Err(FetchError::Unavailable(why)) = setup.broker.model_complete(ask(10)) else {
            panic!("this should have said there's no key");
        };
        assert!(why.contains("API key"), "{why}");
        assert!(network.urls_sent().is_empty());
    }

    #[test]
    fn an_unapproved_or_undeclared_model_sends_nothing() {
        let network = FakeNetwork::default();
        let fixture = Fixture::new();
        let mut broker = Broker::with_network(Scripted::new(Answer::Deny), network.clone())
            .for_run(
                &fixture.storage,
                &Manifest::parse(MANIFEST).unwrap(),
                &RunLimits {
                    memory_bytes: 1,
                    run_time: Duration::from_secs(60),
                    disk_bytes: 1,
                },
                RunId::parse(&"1".repeat(32)).unwrap(),
            );
        assert_eq!(
            broker.model_complete(ask(10)).err(),
            Some(FetchError::NotGranted)
        );
        let mut other = ask(10);
        other.model = "other".to_string();
        assert!(matches!(
            broker.model_complete(other),
            Err(FetchError::Denied(_))
        ));
        assert!(network.urls_sent().is_empty());
    }

    #[test]
    fn a_model_that_isnt_the_providers_is_refused() {
        let network = FakeNetwork::default();
        let root = TempDir::new().unwrap();
        let shared = RunShared {
            audit: Arc::new(Mutex::new(Err(AuditError::TooManyEntries))),
            tokens_left: Arc::new(AtomicU64::new(100)),
        };
        let mut request = ask(10);
        request.model = "gpt-5".to_string();
        let vault = Vault::new(root.path(), FakeStore::default());
        assert!(matches!(
            model::start(
                &network,
                Some(&vault),
                &request,
                shared,
                Duration::from_secs(1)
            ),
            Err(FetchError::Denied(_))
        ));
    }

    #[test]
    fn provider_errors_become_clear_errors_and_hold_nothing_from_the_provider() {
        let echo = format!("{{\"error\":\"bad key {SECRET}\"}}");
        for (status, expected) in [
            (400, "InvalidRequest"),
            (401, "Denied"),
            (403, "Denied"),
            (404, "InvalidRequest"),
            (429, "Unavailable"),
            (500, "Unavailable"),
            (529, "Unavailable"),
        ] {
            let network = FakeNetwork::default().page(PROVIDER, status, &[], echo.as_bytes());
            let mut setup = model_setup(network, 1000, true);
            let error = setup.broker.model_complete(ask(10)).err().unwrap();
            let text = format!("{error:?}");
            assert!(text.contains(expected), "{status}: {text}");
            assert!(!text.contains(SECRET), "{status}: {text}");
        }
    }

    #[test]
    fn a_redirect_from_the_provider_isnt_followed_with_the_key() {
        let network = FakeNetwork::default().page(
            PROVIDER,
            302,
            &[("Location", "https://evil.example/")],
            b"",
        );
        let mut setup = model_setup(network.clone(), 1000, true);
        assert!(setup.broker.model_complete(ask(10)).is_err());
        assert_eq!(network.urls_sent(), [PROVIDER]);
    }

    #[test]
    fn a_streams_problems_reach_the_agent() {
        let start =
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n";
        for (rest, expected) in [
            ("", "Unavailable"),
            ("data: not json\n\n", "Unavailable"),
            (
                "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n",
                "Unavailable",
            ),
            (
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\"},\"usage\":{\"output_tokens\":1}}\n\n",
                "Denied",
            ),
        ] {
            let body = format!("{start}{rest}");
            let network = FakeNetwork::default().page(PROVIDER, 200, &[], body.as_bytes());
            let mut setup = model_setup(network, 1000, true);
            let mut answer = setup.broker.model_complete(ask(10)).ok().unwrap();
            let error = answer.next().unwrap_err();
            assert!(format!("{error:?}").contains(expected), "{rest}: {error:?}");
            // It stays finished after that.
            assert_eq!(answer.next().unwrap(), None);
        }
    }

    #[test]
    fn a_stream_with_a_huge_line_is_cut_off() {
        let network = FakeNetwork {
            huge_body: Some(10 * 1024 * 1024),
            ..FakeNetwork::default()
        };
        let mut setup = model_setup(network, 1000, true);
        let mut answer = setup.broker.model_complete(ask(10)).ok().unwrap();
        assert!(matches!(answer.next(), Err(FetchError::LimitExceeded(_))));
    }

    #[test]
    fn requests_that_are_too_large_or_empty_are_refused() {
        let network = FakeNetwork::default().page(PROVIDER, 200, &[], MODEL_ANSWER.as_bytes());
        let mut setup = model_setup(network.clone(), 1000, true);
        let user = |text: String| ModelMessage {
            role: ModelRole::User,
            text,
        };
        for request in [
            ModelRequest {
                model: "default".to_string(),
                messages: vec![],
                max_tokens: 10,
            },
            ModelRequest {
                model: "default".to_string(),
                messages: vec![ModelMessage {
                    role: ModelRole::System,
                    text: "only this".to_string(),
                }],
                max_tokens: 10,
            },
            ModelRequest {
                model: "default".to_string(),
                messages: vec![user("x".to_string())],
                max_tokens: 0,
            },
            ModelRequest {
                model: "default".to_string(),
                messages: vec![user("x".repeat(9 * 1024 * 1024))],
                max_tokens: 10,
            },
            ModelRequest {
                model: "default".to_string(),
                messages: (0..1025).map(|_| user("x".to_string())).collect(),
                max_tokens: 10,
            },
        ] {
            assert!(matches!(
                setup.broker.model_complete(request),
                Err(FetchError::InvalidRequest(_))
            ));
        }
        assert!(network.urls_sent().is_empty());
    }

    #[test]
    fn after_a_private_read_a_model_needs_fresh_approval_too() {
        let private = a_private_file("private words");
        let network = FakeNetwork::default().page(PROVIDER, 200, &[], MODEL_ANSWER.as_bytes());
        let fixture = Fixture::new();
        let mut grants = RunGrants::new(fixture.storage.clone());
        grants
            .grant(Target::Model("default".to_string()), Lifetime::UntilRevoked)
            .unwrap();
        let root = TempDir::new().unwrap();
        let mut broker = Broker::with_network(
            Scripted::choosing(Answer::Deny, &private.path),
            network.clone(),
        )
        .with_vault(Vault::new(root.path(), FakeStore::default()))
        .for_run(
            &fixture.storage,
            &Manifest::parse(MANIFEST).unwrap(),
            &RunLimits {
                memory_bytes: 1,
                run_time: Duration::from_secs(60),
                disk_bytes: 1,
            },
            RunId::parse(&"1".repeat(32)).unwrap(),
        );
        broker.pick_user_file("please").ok().unwrap();
        assert_eq!(
            broker.model_complete(ask(10)).err(),
            Some(FetchError::NotGranted)
        );
        assert!(network.urls_sent().is_empty());
    }

    #[test]
    fn refusals_hold_nothing_from_the_agent() {
        let fixture = Fixture::new();
        let hostile = "\x1b[31mSECRET-REASON";
        let mut broker = fixture.run(Scripted::new(Answer::Deny));
        for request in [
            broker.request_access(&host("a.example"), hostile),
            broker.request_access(&host("a.example"), hostile),
            broker.request_access(&host(hostile), hostile),
        ] {
            let text = format!("{request:?}");
            assert!(!text.contains("SECRET-REASON"), "{text}");
        }
    }
}
