//! Puts the policy, the grants and the user together, for each request that one run makes.

use std::sync::Arc;

use habitat_core::backend::{AccessRefused, BrokerFactory, Lifetime, RunBroker, RunLimits, Target};
use habitat_core::home::AgentStorage;
use habitat_core::manifest::Manifest;

use habitat_core::run_log::RunId;

use crate::approval::{ANSWER_TIME, Answer, Approver, Prompt, PromptLimit};
use crate::audit::{AuditError, AuditEvent, AuditLog};
use crate::grants::RunGrants;
use crate::policy::{Action, Context, Decision, DenyReason, decide};

/// Makes a broker for each run, which asks the user through the given shell.
pub struct Broker {
    approver: Arc<dyn Approver>,
}

impl Broker {
    pub fn new(approver: impl Approver + 'static) -> Self {
        Self {
            approver: Arc::new(approver),
        }
    }
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
        _limits: &RunLimits,
        run_id: RunId,
    ) -> Box<dyn RunBroker + Send> {
        Box::new(ForRun {
            audit: AuditLog::open(storage, run_id),
            manifest: manifest.clone(),
            grants: RunGrants::new(storage.clone()),
            approver: Arc::clone(&self.approver),
            limit: PromptLimit::default(),
            tokens_left: manifest.limits().model_budget_tokens,
        })
    }
}

/// The broker for one run. Nothing here outlives the run, except a grant until revoked.
struct ForRun {
    audit: Result<AuditLog, AuditError>,
    manifest: Manifest,
    grants: RunGrants,
    approver: Arc<dyn Approver>,
    limit: PromptLimit,
    tokens_left: u64,
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
        let audit = self.audit.as_mut().map_err(|_| AccessRefused::Internal)?;
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
}

impl ForRun {
    fn decide_request(&mut self, target: &Target, reason: &str) -> Result<Lifetime, AccessRefused> {
        let context = Context {
            manifest: &self.manifest,
            grants: &self.grants,
            tokens_left: self.tokens_left,
        };
        match decide(&context, &Action::Reach(target.clone())) {
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
                let prompt = Prompt::new(self.manifest.name(), target, reason);
                match self.approver.ask(&prompt, ANSWER_TIME) {
                    Answer::Allow(lifetime) => {
                        self.grants
                            .grant(target.clone(), lifetime)
                            .map_err(|_| AccessRefused::Internal)?;
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
    use std::fs;
    use std::sync::Mutex;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::approval::{MAX_PROMPTS_PER_RUN, NobodyThere};
    use crate::audit::{MAX_ENTRIES_PER_RUN, verify};

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
    "#;

    /// Gives the same answer each time, and remembers what it was asked.
    #[derive(Clone)]
    struct Scripted {
        answer: Answer,
        asked: Arc<Mutex<Vec<(Prompt, Duration)>>>,
    }

    impl Scripted {
        fn new(answer: Answer) -> Self {
            Self {
                answer,
                asked: Arc::default(),
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
            Broker::new(approver).for_run(&self.storage, &manifest, &limits, run_id)
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
    fn a_flood_of_requests_stops_at_the_limit() {
        let fixture = Fixture::new();
        let manifest = Manifest::parse(MANIFEST).unwrap();
        let run_id = RunId::parse(&"1".repeat(32)).unwrap();
        let mut audit = AuditLog::open(&fixture.storage, run_id).unwrap();
        audit.skip_ahead(MAX_ENTRIES_PER_RUN - 1);
        let mut broker = ForRun {
            audit: Ok(audit),
            manifest: manifest.clone(),
            grants: RunGrants::new(fixture.storage.clone()),
            approver: Arc::new(Scripted::new(Answer::Allow(Lifetime::UntilRevoked))),
            limit: PromptLimit::default(),
            tokens_left: 0,
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
