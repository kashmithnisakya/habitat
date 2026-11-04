//! Requests from an agent. A malformed one, or one that isn't declared, is never allowed or asked
//! about, whatever grants there are.
#![no_main]

use habitat_broker::policy::{Action, Context, Decision, DenyReason, Grants, LeakCheck, Target, decide};
use habitat_core::manifest::{Manifest, is_host_name, is_model_name};
use libfuzzer_sys::fuzz_target;

struct GrantsEverything;

impl Grants for GrantsEverything {
    fn covers(&self, _: &Target) -> bool {
        true
    }
}

const MANIFEST: &str = "[agent]\nname = \"a\"\nversion = \"1.0.0\"\n\n[limits]\nmemory_mb = 8\nrun_seconds = 1\ndisk_mb = 1\nmodel_budget_tokens = 10\n\n[requests]\nnetwork = [\"a.example\"]\nmodels = [\"default\"]\n";

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let manifest = Manifest::parse(MANIFEST).unwrap();
    let context = Context {
        manifest: &manifest,
        grants: &GrantsEverything,
        tokens_left: 10,
        leak_check: LeakCheck::NothingPrivateRead,
    };
    for (target, valid, declared) in [
        (Target::NetworkHost(text.to_string()), is_host_name(text), text == "a.example"),
        (Target::Model(text.to_string()), is_model_name(text), text == "default"),
    ] {
        let decision = decide(&context, &Action::Reach(target));
        if !valid {
            assert_eq!(decision, Decision::Deny(DenyReason::Malformed), "{text:?}");
        } else if !declared {
            assert_eq!(decision, Decision::Deny(DenyReason::NotDeclared), "{text:?}");
        } else {
            assert_eq!(decision, Decision::Allow, "{text:?}");
        }
    }
});
