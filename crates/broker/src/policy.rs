//! Decides on everything an agent asks for that leaves its home.
//!
//! This only decides. It does no I/O, so that every decision can be tested and nothing here can
//! fail halfway. `docs/permissions.md` has the rules, and anything they don't cover is denied.

use std::collections::HashSet;
use std::fmt;

pub use habitat_core::backend::Target;
use habitat_core::manifest::{FileRequest, Manifest, is_host_name, is_model_name};

/// Something an agent is trying to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Using a file in its own home, which the home store limits.
    UseHome,
    /// Reaching a network host or using a model.
    Reach(Target),
    /// Reading a file that belongs to the user, who has to choose it each time.
    ReadUserFile,
    /// Getting hold of a raw secret, such as an API key.
    ReadSecret,
}

/// The decision on an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Only the user can decide, so the shell has to ask them.
    Ask,
    Deny(DenyReason),
}

/// Why an action was denied. The text holds nothing from the agent, so it's safe to show to anyone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// The request doesn't follow the rules for what it's asking for.
    Malformed,
    /// The agent's manifest doesn't list this, so there's nothing to ask the user.
    NotDeclared,
    /// The run has used up its budget for AI models.
    OverBudget,
    /// Nothing can allow this.
    NeverAllowed,
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "the request isn't valid",
            Self::NotDeclared => "the agent's manifest doesn't ask for this",
            Self::OverBudget => "the run has used up its budget for AI models",
            Self::NeverAllowed => "this is never allowed",
        })
    }
}

/// The grants that a decision can rely on. Keeping this as a question, and not a list, leaves how
/// grants are stored and when they expire to the grant store.
pub trait Grants {
    /// Whether a current grant covers exactly this target.
    fn covers(&self, target: &Target) -> bool;
}

/// What a decision depends on, apart from the action itself.
pub struct Context<'a> {
    /// The installed agent's manifest, which has already been validated.
    pub manifest: &'a Manifest,
    pub grants: &'a dyn Grants,
    /// How many tokens this run has left for AI models.
    pub tokens_left: u64,
    /// Whether this run has read the user's private data, and so what it can still reach without
    /// asking again. See [`decide`].
    pub leak_check: LeakCheck<'a>,
}

/// What the data-leak rule needs to know about a run. An agent that's been tricked could send
/// private data to somewhere it's already allowed to reach. So once it's read any, somewhere it
/// hasn't already been in this run needs a fresh approval, whatever grants there are. This reduces
/// the risk, but it doesn't remove it, as `docs/threat-model.md` explains.
#[derive(Debug, Clone, Copy)]
pub enum LeakCheck<'a> {
    /// The run hasn't read anything private.
    NothingPrivateRead,
    /// It has, and the user has approved these since.
    PrivateRead { reached: &'a HashSet<Target> },
}

/// Decides on an action. Declaring something in a manifest only lets an agent ask, and a grant
/// counts only for something that's still declared, so a changed manifest can't keep old access.
pub fn decide(context: &Context, action: &Action) -> Decision {
    match action {
        Action::UseHome => Decision::Allow,
        Action::ReadSecret => Decision::Deny(DenyReason::NeverAllowed),
        Action::ReadUserFile => match context.manifest.requested_files() {
            FileRequest::Ask => Decision::Ask,
            _ => Decision::Deny(DenyReason::NotDeclared),
        },
        Action::Reach(target) => decide_reach(context, target),
    }
}

fn decide_reach(context: &Context, target: &Target) -> Decision {
    let (well_formed, declared, needs_tokens) = match target {
        Target::NetworkHost(host) => (
            is_host_name(host),
            context.manifest.requested_hosts().contains(host),
            false,
        ),
        Target::Model(model) => (
            is_model_name(model),
            context.manifest.requested_models().contains(model),
            true,
        ),
    };
    // In this order, so that what's reported doesn't reveal anything about what's been granted.
    if !well_formed {
        return Decision::Deny(DenyReason::Malformed);
    }
    if !declared {
        return Decision::Deny(DenyReason::NotDeclared);
    }
    if needs_tokens && context.tokens_left == 0 {
        return Decision::Deny(DenyReason::OverBudget);
    }
    // Hosted AI models count here too, as they're somewhere that data goes.
    let needs_fresh_approval = match context.leak_check {
        LeakCheck::NothingPrivateRead => false,
        LeakCheck::PrivateRead { reached } => !reached.contains(target),
    };
    if context.grants.covers(target) && !needs_fresh_approval {
        Decision::Allow
    } else {
        Decision::Ask
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        network = ["api.github.com", "xn--bcher-kva.example"]
        models = ["default", "gpt.4-o_x"]
        files = "ask"
    "#;

    const NOTHING_REQUESTED: &str = r#"
        [agent]
        name = "tester"
        version = "1.0.0"

        [limits]
        memory_mb = 8
        run_seconds = 1
        disk_mb = 1
    "#;

    const GITHUB: &str = "api.github.com";

    struct Granted(Vec<Target>);

    impl Grants for Granted {
        fn covers(&self, target: &Target) -> bool {
            self.0.contains(target)
        }
    }

    fn host(name: &str) -> Target {
        Target::NetworkHost(name.to_string())
    }

    fn model(name: &str) -> Target {
        Target::Model(name.to_string())
    }

    fn decide_with(
        manifest: &str,
        granted: Vec<Target>,
        tokens_left: u64,
        action: Action,
    ) -> Decision {
        let manifest = Manifest::parse(manifest).unwrap();
        let grants = Granted(granted);
        let context = Context {
            manifest: &manifest,
            grants: &grants,
            tokens_left,
            leak_check: LeakCheck::NothingPrivateRead,
        };
        decide(&context, &action)
    }

    fn deny(reason: DenyReason) -> Decision {
        Decision::Deny(reason)
    }

    /// The table in `docs/permissions.md`, row by row.
    #[test]
    fn using_the_home_is_always_allowed() {
        for manifest in [MANIFEST, NOTHING_REQUESTED] {
            assert_eq!(
                decide_with(manifest, vec![], 0, Action::UseHome),
                Decision::Allow
            );
        }
    }

    #[test]
    fn a_declared_host_asks_until_its_granted() {
        let reach = Action::Reach(host(GITHUB));
        assert_eq!(
            decide_with(MANIFEST, vec![], 0, reach.clone()),
            Decision::Ask
        );
        assert_eq!(
            decide_with(MANIFEST, vec![host(GITHUB)], 0, reach),
            Decision::Allow
        );
    }

    #[test]
    fn an_undeclared_host_is_denied_without_asking() {
        assert_eq!(
            decide_with(MANIFEST, vec![], 100, Action::Reach(host("example.com"))),
            deny(DenyReason::NotDeclared)
        );
        assert_eq!(
            decide_with(NOTHING_REQUESTED, vec![], 100, Action::Reach(host(GITHUB))),
            deny(DenyReason::NotDeclared)
        );
    }

    #[test]
    fn a_grant_doesnt_help_with_something_undeclared() {
        // For example, a grant left over from before a manifest changed.
        for (target, manifest) in [
            (host("example.com"), MANIFEST),
            (model("other"), MANIFEST),
            (host(GITHUB), NOTHING_REQUESTED),
            (model("default"), NOTHING_REQUESTED),
        ] {
            assert_eq!(
                decide_with(manifest, vec![target.clone()], 100, Action::Reach(target)),
                deny(DenyReason::NotDeclared)
            );
        }
    }

    #[test]
    fn a_grant_only_covers_exactly_what_it_names() {
        let granted = vec![host("xn--bcher-kva.example"), model("gpt.4-o_x")];
        assert_eq!(
            decide_with(MANIFEST, granted.clone(), 100, Action::Reach(host(GITHUB))),
            Decision::Ask
        );
        assert_eq!(
            decide_with(MANIFEST, granted, 100, Action::Reach(model("default"))),
            Decision::Ask
        );
    }

    #[test]
    fn a_host_and_a_model_with_the_same_name_are_different_things() {
        let manifest = r#"
            [agent]
            name = "tester"
            version = "1.0.0"

            [limits]
            memory_mb = 8
            run_seconds = 1
            disk_mb = 1
            model_budget_tokens = 100

            [requests]
            network = ["same.name"]
            models = ["same.name"]
        "#;
        assert_eq!(
            decide_with(
                manifest,
                vec![host("same.name")],
                100,
                Action::Reach(model("same.name"))
            ),
            Decision::Ask
        );
        assert_eq!(
            decide_with(
                manifest,
                vec![model("same.name")],
                100,
                Action::Reach(host("same.name"))
            ),
            Decision::Ask
        );
    }

    #[test]
    fn a_model_is_allowed_if_granted_and_within_its_budget() {
        let reach = Action::Reach(model("default"));
        assert_eq!(
            decide_with(MANIFEST, vec![], 1, reach.clone()),
            Decision::Ask
        );
        assert_eq!(
            decide_with(MANIFEST, vec![model("default")], 1, reach.clone()),
            Decision::Allow
        );
        assert_eq!(
            decide_with(MANIFEST, vec![model("default")], 0, reach.clone()),
            deny(DenyReason::OverBudget)
        );
        // There's no point in asking the user for something that can't happen.
        assert_eq!(
            decide_with(MANIFEST, vec![], 0, reach),
            deny(DenyReason::OverBudget)
        );
    }

    #[test]
    fn the_budget_doesnt_apply_to_the_network() {
        assert_eq!(
            decide_with(MANIFEST, vec![host(GITHUB)], 0, Action::Reach(host(GITHUB))),
            Decision::Allow
        );
    }

    #[test]
    fn an_undeclared_model_is_denied_without_asking() {
        assert_eq!(
            decide_with(MANIFEST, vec![], 100, Action::Reach(model("other"))),
            deny(DenyReason::NotDeclared)
        );
    }

    #[test]
    fn a_user_file_always_asks_if_declared_and_is_otherwise_denied() {
        assert_eq!(
            decide_with(MANIFEST, vec![], 0, Action::ReadUserFile),
            Decision::Ask
        );
        assert_eq!(
            decide_with(NOTHING_REQUESTED, vec![], 0, Action::ReadUserFile),
            deny(DenyReason::NotDeclared)
        );
    }

    #[test]
    fn a_raw_secret_is_never_allowed() {
        for manifest in [MANIFEST, NOTHING_REQUESTED] {
            assert_eq!(
                decide_with(
                    manifest,
                    vec![host(GITHUB), model("default")],
                    100,
                    Action::ReadSecret
                ),
                deny(DenyReason::NeverAllowed)
            );
        }
    }

    #[test]
    fn malformed_hosts_are_denied_even_if_something_claims_to_grant_them() {
        let long = format!("{}.example.com", "a".repeat(300));
        for name in [
            "",
            " ",
            ".",
            "..",
            "com",
            "localhost",
            "127.0.0.1",
            "[::1]",
            "::1",
            "0x7f.0.0.1",
            "api.github.com.",
            ".api.github.com",
            "API.GITHUB.COM",
            "Api.github.com",
            "api.github.com:443",
            "https://api.github.com",
            "api.github.com/path",
            "api.github.com?x",
            "user@api.github.com",
            "*.github.com",
            "api github.com",
            "api.github.com\n",
            "api.github.com\0",
            "\x1b[31mapi.github.com",
            "аpi.github.com",
            "bücher.example",
            long.as_str(),
        ] {
            let target = host(name);
            assert_eq!(
                decide_with(MANIFEST, vec![target.clone()], 100, Action::Reach(target)),
                deny(DenyReason::Malformed),
                "{name:?}"
            );
        }
    }

    #[test]
    fn malformed_models_are_denied_even_if_something_claims_to_grant_them() {
        let long = "m".repeat(65);
        for name in [
            "",
            " ",
            ".hidden",
            "-dash",
            "DEFAULT",
            "default ",
            "de fault",
            "default\n",
            "default\0",
            "../default",
            "a/b",
            "\x1b[31mdefault",
            "défaut",
            long.as_str(),
        ] {
            let target = model(name);
            assert_eq!(
                decide_with(MANIFEST, vec![target.clone()], 100, Action::Reach(target)),
                deny(DenyReason::Malformed),
                "{name:?}"
            );
        }
    }

    #[test]
    fn whats_denied_doesnt_reveal_whats_been_granted() {
        // The same answer with and without a grant, unless the grant really applies.
        for action in [
            Action::Reach(host("example.com")),
            Action::Reach(host("BAD HOST")),
            Action::Reach(model("other")),
        ] {
            let target = match &action {
                Action::Reach(target) => target.clone(),
                _ => unreachable!(),
            };
            assert_eq!(
                decide_with(MANIFEST, vec![], 100, action.clone()),
                decide_with(MANIFEST, vec![target], 100, action)
            );
        }
    }

    fn decide_after_private_read(target: Target, granted: bool, reached: &[Target]) -> Decision {
        let manifest = Manifest::parse(MANIFEST).unwrap();
        let grants = Granted(if granted {
            vec![target.clone()]
        } else {
            vec![]
        });
        let reached: HashSet<Target> = reached.iter().cloned().collect();
        let context = Context {
            manifest: &manifest,
            grants: &grants,
            tokens_left: 100,
            leak_check: LeakCheck::PrivateRead { reached: &reached },
        };
        decide(&context, &Action::Reach(target))
    }

    #[test]
    fn after_reading_private_data_a_new_destination_asks_even_if_its_granted() {
        assert_eq!(
            decide_after_private_read(host(GITHUB), true, &[]),
            Decision::Ask
        );
        assert_eq!(
            decide_after_private_read(model("default"), true, &[]),
            Decision::Ask
        );
    }

    #[test]
    fn after_reading_private_data_somewhere_already_reached_doesnt_ask_again() {
        assert_eq!(
            decide_after_private_read(host(GITHUB), true, &[host(GITHUB)]),
            Decision::Allow
        );
        // Somewhere else being reached doesn't count.
        assert_eq!(
            decide_after_private_read(host(GITHUB), true, &[model("default")]),
            Decision::Ask
        );
    }

    #[test]
    fn reaching_somewhere_doesnt_stand_in_for_a_grant() {
        assert_eq!(
            decide_after_private_read(host(GITHUB), false, &[host(GITHUB)]),
            Decision::Ask
        );
    }

    #[test]
    fn the_leak_rule_doesnt_help_with_anything_else_thats_refused() {
        assert_eq!(
            decide_after_private_read(host("evil.example"), true, &[host("evil.example")]),
            deny(DenyReason::NotDeclared)
        );
        assert_eq!(
            decide_after_private_read(host("BAD"), true, &[host("BAD")]),
            deny(DenyReason::Malformed)
        );
    }

    #[test]
    fn reasons_hold_nothing_from_the_agent() {
        for reason in [
            DenyReason::Malformed,
            DenyReason::NotDeclared,
            DenyReason::OverBudget,
            DenyReason::NeverAllowed,
        ] {
            let text = reason.to_string();
            assert!(!text.is_empty());
            assert!(
                text.is_ascii() && !text.chars().any(|c| c.is_control()),
                "{text}"
            );
        }
    }
}
