//! Proves that the harness can fail. A check that can't catch anything would pass forever, and say
//! nothing, so these run the same checks against a host that does everything wrong, and they have to
//! find it.

use habitat_attacks::{Arena, Breach, OUTSIDE_CANARY, PLANTED_SECRET, VulnerableBroker, no_breach};

fn found(result: Result<(), Breach>, what: &str) {
    let Err(breach) = result else {
        panic!("the harness missed this: {what}");
    };
    assert!(breach.0.contains(what), "{breach}");
}

/// Sends a message that reaches `show_message`, which the vulnerable host doesn't have, so this
/// builds the report by hand from what the agent would have got.
#[test]
fn a_host_that_leaks_a_secret_is_caught() {
    let arena = Arena::with_broker(VulnerableBroker);
    let mut report = arena.attack("fetch https://evil.example/");
    // The vulnerable broker doesn't pass on messages, so this is what the agent got hold of.
    report
        .messages
        .push(format!("OK fetch: the key is {PLANTED_SECRET}"));
    found(arena.check_no_leak(&report), PLANTED_SECRET);
}

#[test]
fn a_secret_in_the_agents_home_is_caught() {
    let arena = Arena::new();
    let report = arena.attack("write a.txt fine");
    no_breach(arena.check_no_leak(&report));
    std::fs::write(arena.home().join("stolen.txt"), OUTSIDE_CANARY).unwrap();
    found(arena.check_no_leak(&report), OUTSIDE_CANARY);
}

#[test]
fn a_host_that_changes_files_outside_the_home_is_caught() {
    let arena = Arena::with_broker(VulnerableBroker);
    let before = arena.snapshot();
    arena.attack("fetch https://evil.example/");
    found(arena.check_unchanged(&before), "was created");
}

#[test]
fn a_snapshot_really_holds_what_it_should_wherever_the_temporary_folder_is() {
    // On Linux, temporary folders are under `/tmp`. A check that skipped everything with that in
    // its path once passed there by looking at nothing.
    let snapshot = Arena::new().snapshot();
    for expected in [
        "secret.txt",
        "agent.wasm",
        "agent.toml",
        "package.sha256",
        "secrets.toml",
    ] {
        assert!(snapshot.holds(expected), "{expected} isn't in {snapshot:?}");
    }
}

#[test]
fn a_changed_or_removed_file_is_caught() {
    let arena = Arena::new();
    let before = arena.snapshot();
    no_breach(arena.check_unchanged(&before));

    let secret = arena.outside().join("secret.txt");
    std::fs::write(&secret, "changed").unwrap();
    found(arena.check_unchanged(&before), "was changed");
    std::fs::remove_file(&secret).unwrap();
    found(arena.check_unchanged(&before), "was removed");
}

#[test]
fn a_secret_saved_in_a_file_is_caught() {
    let arena = Arena::new();
    no_breach(arena.check_secret_only_in_keystore());
    std::fs::write(arena.agent_folder().join("oops.txt"), PLANTED_SECRET).unwrap();
    found(arena.check_secret_only_in_keystore(), "the secret is in");
}

#[test]
fn a_changed_log_is_caught() {
    let arena = Arena::new();
    arena.attack("ask-host allowed.example please");
    assert!(arena.log().is_ok());
    let path = arena.storage().audit_path();
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace("allowed.example", "harmless.example")).unwrap();
    found(arena.log().map(|_| ()), "doesn't check out");
}

#[test]
fn a_host_that_stops_working_is_caught() {
    let arena = Arena::new();
    no_breach(arena.check_host_still_works());
    // A folder where its home should be, so that it can't run.
    let home = arena.home();
    std::fs::remove_dir_all(&home).unwrap();
    std::fs::write(&home, "not a folder").unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        arena.check_host_still_works()
    }));
    assert!(!matches!(result, Ok(Ok(()))), "the harness missed this");
}

#[test]
fn the_real_host_gives_the_hostile_agent_nothing_to_begin_with() {
    let arena = Arena::new();
    let before = arena.snapshot();
    let report = arena.attack("fetch https://allowed.example/");
    assert!(report.agent_visible().contains("NotGranted"), "{report:?}");
    no_breach(arena.check_no_leak(&report));
    no_breach(arena.check_unchanged(&before));
    no_breach(arena.check_secret_only_in_keystore());
    no_breach(arena.check_host_still_works());
}
