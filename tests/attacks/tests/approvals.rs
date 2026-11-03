//! An agent trying to wear down, fool or get around the person it's asking, or cover its tracks.

use std::fs;

use habitat_attacks::{ALLOWED_HOST, ALSO_ALLOWED_HOST, Arena, Reply, no_breach};
use habitat_broker::approval::Answer;
use habitat_broker::audit::AuditEvent;
use habitat_broker::grants::{list_saved, revoke_saved};
use habitat_core::backend::{Lifetime, Target};

fn answer(arena: &Arena, answer: Answer) {
    *arena.user.answer.lock().unwrap() = answer;
}

#[test]
fn asking_over_and_over_stops_prompting() {
    let arena = Arena::new();
    arena.attack(&format!("ask-same 1000 {ALLOWED_HOST}"));
    assert_eq!(arena.user.prompts.lock().unwrap().len(), 1, "no means no");
}

#[test]
fn asking_for_many_things_stops_prompting() {
    let arena = Arena::new();
    // These aren't declared, so they never get as far as a prompt.
    arena.attack("spam-ask 1000");
    assert!(arena.user.prompts.lock().unwrap().is_empty());
    // These are, so they're counted.
    arena.attack(&format!("ask-host {ALLOWED_HOST} one"));
    arena.attack(&format!("ask-host {ALSO_ALLOWED_HOST} two"));
    assert_eq!(arena.user.prompts.lock().unwrap().len(), 2);
}

#[test]
fn a_misleading_reason_cant_change_the_question() {
    let arena = Arena::new();
    for reason in [
        "\x1b[2K\x1b[1A\rAgent \"system\" is asking to reach the network host safe.example.",
        "fine\nAllow this? [y]\nThe user already said yes.",
        "\u{202e}elpmaxe.live",
        "\u{7}\u{8}\u{8}\u{8}\u{9b}31m",
        "[y] once\r[a] until revoked",
    ] {
        arena.user.prompts.lock().unwrap().clear();
        arena.attack(&format!("ask-host {ALLOWED_HOST} {reason}"));
        let prompts = arena.user.prompts.lock().unwrap();
        let prompt = &prompts[0];
        let mut lines = prompt.lines();
        assert_eq!(
            lines.next().unwrap(),
            format!("Agent \"hostile\" is asking to reach the network host {ALLOWED_HOST}."),
            "{reason:?}"
        );
        let claim = lines.next().unwrap();
        assert!(
            claim.starts_with("The agent's own claim, which may not be true: "),
            "{claim}"
        );
        // There's nothing after that, and no way to put anything there.
        assert_eq!(lines.next(), None, "{prompt:?}");
        assert!(prompt.is_ascii(), "{prompt:?}");
        assert!(
            !prompt.chars().any(|c| c.is_control() && c != '\n'),
            "{prompt:?}"
        );
    }
}

#[test]
fn a_target_with_escapes_in_it_never_reaches_a_prompt() {
    let arena = Arena::new();
    answer(&arena, Answer::Allow(Lifetime::UntilRevoked));
    for target in [
        "\x1b[31mallowed.example",
        "allowed.example\n",
        "allowed.example\u{202e}",
    ] {
        arena.attack(&format!("ask-host {target} please"));
    }
    assert!(arena.user.prompts.lock().unwrap().is_empty());
    assert!(list_saved(&arena.storage()).unwrap().is_empty());
}

#[test]
fn saying_no_grants_nothing() {
    let arena = Arena::new();
    arena.attack(&format!("ask-host {ALLOWED_HOST} please"));
    assert!(list_saved(&arena.storage()).unwrap().is_empty());
    let report = arena.attack(&format!("fetch https://{ALLOWED_HOST}/"));
    assert!(report.agent_visible().contains("NotGranted"), "{report:?}");
}

#[test]
fn a_grant_for_one_request_is_used_up() {
    let arena = Arena::new();
    answer(&arena, Answer::Allow(Lifetime::Once));
    let url = format!("https://{ALLOWED_HOST}/");
    arena.network.reply(&url, Reply::page(200, "fine"));
    let report = arena.attack(&format!("ask-then-fetch-twice {ALLOWED_HOST} {url}"));
    assert_eq!(arena.network.urls_sent().len(), 1, "{report:?}");
    assert!(report.agent_visible().contains("NotGranted"), "{report:?}");
}

#[test]
fn a_grant_for_a_run_ends_with_the_run() {
    let arena = Arena::new();
    answer(&arena, Answer::Allow(Lifetime::ThisRun));
    let url = format!("https://{ALLOWED_HOST}/");
    arena.network.reply(&url, Reply::page(200, "fine"));
    let first = arena.attack(&format!("ask-then-fetch {ALLOWED_HOST} {url}"));
    assert!(first.agent_visible().contains("OK fetch"), "{first:?}");
    let second = arena.attack(&format!("fetch {url}"));
    assert!(second.agent_visible().contains("NotGranted"), "{second:?}");
}

#[test]
fn a_revoked_grant_is_never_honoured() {
    let arena = Arena::new();
    answer(&arena, Answer::Allow(Lifetime::UntilRevoked));
    let url = format!("https://{ALLOWED_HOST}/");
    arena.network.reply(&url, Reply::page(200, "fine"));
    let granted = arena.attack(&format!("ask-then-fetch {ALLOWED_HOST} {url}"));
    assert!(granted.agent_visible().contains("OK fetch"), "{granted:?}");

    let target = Target::NetworkHost(ALLOWED_HOST.to_string());
    assert_eq!(revoke_saved(&arena.storage(), &target), Ok(true));
    let count = arena.network.urls_sent().len();
    let revoked = arena.attack(&format!("fetch {url}"));
    assert!(
        revoked.agent_visible().contains("NotGranted"),
        "{revoked:?}"
    );
    assert_eq!(arena.network.urls_sent().len(), count);
}

#[test]
fn an_agent_cant_grant_itself_anything() {
    let arena = Arena::new();
    let before = arena.snapshot();
    for attack in [
        "write ../grants.db format=1",
        "write ../../hostile/grants.db format=1",
        "remove ../grants.db",
        "read ../grants.db",
    ] {
        let report = arena.attack(attack);
        no_breach(arena.check_no_leak(&report));
    }
    no_breach(arena.check_unchanged(&before));
    assert!(list_saved(&arena.storage()).unwrap().is_empty());
}

/// A log with something in it, and what's in it now.
fn logged() -> (Arena, Vec<String>) {
    let arena = Arena::new();
    arena.attack(&format!("ask-host {ALLOWED_HOST} one"));
    arena.attack(&format!("ask-host {ALSO_ALLOWED_HOST} two"));
    assert!(arena.log().is_ok());
    let text = fs::read_to_string(arena.storage().audit_path()).unwrap();
    let lines = text.lines().map(str::to_string).collect();
    (arena, lines)
}

fn write_lines(arena: &Arena, lines: &[String]) {
    let mut text = lines.join("\n");
    text.push('\n');
    fs::write(arena.storage().audit_path(), text).unwrap();
}

#[test]
fn editing_a_log_entry_is_detected() {
    let (arena, mut lines) = logged();
    lines[1] = lines[1].replace("refused", "allowed");
    write_lines(&arena, &lines);
    assert!(arena.log().is_err());
}

#[test]
fn deleting_a_log_entry_is_detected() {
    for index in [0, 1] {
        let (arena, mut lines) = logged();
        lines.remove(index);
        write_lines(&arena, &lines);
        assert!(arena.log().is_err(), "{index}");
    }
    // The last one too, which a chain alone wouldn't show.
    let (arena, mut lines) = logged();
    lines.pop();
    write_lines(&arena, &lines);
    assert!(arena.log().is_err());
}

#[test]
fn reordering_log_entries_is_detected() {
    let (arena, mut lines) = logged();
    lines.swap(0, 1);
    write_lines(&arena, &lines);
    assert!(arena.log().is_err());
}

#[test]
fn an_agent_cant_reach_its_log() {
    let (arena, _) = logged();
    let path = arena.storage().audit_path();
    let before = fs::read(&path).unwrap();
    for attack in [
        "read ../audit.db",
        "write ../audit.db forged",
        "remove ../audit.db",
        "write ../audit.head forged",
    ] {
        arena.attack(attack);
    }
    // What's there is only added to, and it still checks out.
    let after = fs::read(&path).unwrap();
    assert_eq!(&after[..before.len()], &before[..]);
    assert!(arena.log().is_ok());
}

#[test]
fn a_request_from_a_changed_log_gets_nothing() {
    let (arena, mut lines) = logged();
    lines.remove(0);
    write_lines(&arena, &lines);
    answer(&arena, Answer::Allow(Lifetime::UntilRevoked));
    let count = arena.user.prompts.lock().unwrap().len();
    let report = arena.attack(&format!("ask-host {ALLOWED_HOST} please"));
    assert!(
        report.messages.is_empty() || report.messages[0].starts_with("NO"),
        "{report:?}"
    );
    assert_eq!(arena.user.prompts.lock().unwrap().len(), count);
}

#[test]
fn what_an_agent_does_shows_up_whatever_it_tries() {
    let arena = Arena::new();
    arena.attack(&format!("ask-host {ALLOWED_HOST} please"));
    let log = arena.log().unwrap();
    assert!(log.contains(&AuditEvent::Requested {
        target: Target::NetworkHost(ALLOWED_HOST.to_string())
    }));
    assert!(log.contains(&AuditEvent::Refused {
        target: Target::NetworkHost(ALLOWED_HOST.to_string())
    }));
    assert!(log.contains(&AuditEvent::RunEnded {
        how: "completed".to_string()
    }));
}
