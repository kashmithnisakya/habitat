//! An agent trying to reach what it shouldn't, or to get hold of an API key.

use habitat_attacks::{
    ALLOWED_HOST, ALSO_ALLOWED_HOST, Arena, FakeNetwork, Limits, PLANTED_SECRET, Reply, no_breach,
};
use habitat_broker::approval::Answer;
use habitat_broker::audit::AuditEvent;
use habitat_broker::gateway::NetworkError;
use habitat_core::backend::{Lifetime, Target};
use habitat_core::package::Package;
use habitat_core::run_log::RunEnding;

const PROVIDER: &str = "https://api.anthropic.com/v1/messages";

fn host(name: &str) -> Target {
    Target::NetworkHost(name.to_string())
}

fn allow(arena: &Arena, lifetime: Lifetime) {
    *arena.user.answer.lock().unwrap() = Answer::Allow(lifetime);
}

fn sent_nothing(network: &FakeNetwork) {
    assert!(network.urls_sent().is_empty(), "{:?}", network.urls_sent());
}

#[test]
fn an_unapproved_host_is_blocked_and_logged() {
    let arena = Arena::new();
    let report = arena.attack(&format!("fetch https://{ALLOWED_HOST}/"));
    assert!(report.agent_visible().contains("NotGranted"), "{report:?}");
    sent_nothing(&arena.network);
    let log = arena.log().unwrap();
    assert!(log.contains(&AuditEvent::Refused {
        target: host(ALLOWED_HOST)
    }));
}

#[test]
fn a_host_the_manifest_doesnt_list_cant_even_be_asked_for() {
    let arena = Arena::new();
    allow(&arena, Lifetime::UntilRevoked);
    for attack in [
        "ask-host evil.example please",
        "fetch https://evil.example/",
    ] {
        let report = arena.attack(attack);
        assert!(
            report.agent_visible().contains("Denied"),
            "{attack}: {report:?}"
        );
    }
    assert!(arena.user.prompts.lock().unwrap().is_empty());
    sent_nothing(&arena.network);
    let log = arena.log().unwrap();
    assert!(log.contains(&AuditEvent::Refused {
        target: host("evil.example")
    }));
}

#[test]
fn urls_that_try_to_confuse_what_gets_checked_are_refused() {
    let arena = Arena::new();
    allow(&arena, Lifetime::UntilRevoked);
    arena.attack(&format!("ask-host {ALLOWED_HOST} please"));
    for url in [
        "http://allowed.example/",
        "HTTPS://allowed.example/",
        "ftp://allowed.example/",
        "file:///etc/passwd",
        "//allowed.example/",
        "allowed.example",
        "https://allowed.example@evil.example/",
        "https://evil.example@allowed.example/",
        "https://allowed.example:pw@evil.example/",
        "https://evil.example#@allowed.example/",
        "https://evil.example\\@allowed.example/",
        "https://evil.example?@allowed.example/",
        "https://allowed.example:8443/",
        "https://allowed.example:80/",
        "https://allowed.example.evil.example/",
        "https://ALLOWED.example/",
        "https://allowed.example./",
        "https://127.0.0.1/",
        "https://[::1]/",
        "https://[::ffff:127.0.0.1]/",
        "https://localhost/",
        "https://169.254.169.254/latest/meta-data/",
        "https://2130706433/",
        "https://0x7f000001/",
        "https://017700000001/",
        "https://allowed.example/\r\nHost:evil.example",
        "https://allowed.example/%0d%0aHost:evil.example\r\n",
        "https://аllowed.example/",
    ] {
        let report = arena.attack(&format!("fetch {url}"));
        assert!(
            report.messages[0].starts_with("NO"),
            "{url}: {:?}",
            report.messages
        );
    }
    sent_nothing(&arena.network);
}

#[test]
fn a_redirect_to_an_unapproved_host_goes_no_further() {
    let arena = Arena::new();
    allow(&arena, Lifetime::ThisRun);
    let start = format!("https://{ALLOWED_HOST}/");
    for location in [
        "https://evil.example/",
        "https://also-allowed.example/",
        "http://allowed.example/",
        "https://127.0.0.1/",
        "https://localhost/",
        "https://169.254.169.254/latest/meta-data/",
        "//evil.example/",
        "file:///etc/passwd",
    ] {
        arena.network.sent.lock().unwrap().clear();
        arena.network.reply(&start, Reply::redirect(location));
        // Both are asked for in one run, and then it's the fetch that matters.
        let report = arena.attack(&format!("ask-then-fetch {ALLOWED_HOST} {start}"));
        assert_eq!(
            arena.network.urls_sent(),
            std::slice::from_ref(&start),
            "{location}: {report:?}"
        );
        assert!(
            report.agent_visible().contains("NO fetch"),
            "{location}: {report:?}"
        );
    }
}

#[test]
fn a_name_that_resolves_to_a_private_address_is_reported_as_refused() {
    let arena = Arena::new();
    allow(&arena, Lifetime::ThisRun);
    let url = format!("https://{ALLOWED_HOST}/");
    // The real check is tested with the real client, in the gateway's own tests. This is what an
    // agent gets from it, which says nothing about what the address was.
    arena
        .network
        .reply(&url, Reply::Fail(NetworkError::Blocked));
    let report = arena.attack(&format!("ask-then-fetch {ALLOWED_HOST} {url}"));
    assert!(
        report.agent_visible().contains("public internet"),
        "{report:?}"
    );
}

#[test]
fn a_component_that_imports_sockets_or_http_isnt_loaded() {
    for interface in [
        "wasi:sockets/tcp@0.2.0",
        "wasi:sockets/udp@0.2.0",
        "wasi:http/outgoing-handler@0.2.0",
        "wasi:filesystem/types@0.2.0",
        "wasi:cli/environment@0.2.0",
        "habitat:host/secrets@0.1.0",
    ] {
        let arena = Arena::new();
        let code =
            wat::parse_str(format!(r#"(component (import "{interface}" (instance)))"#)).unwrap();
        let storage = habitat_core::home::AgentStorage::open(arena.root(), "importer").unwrap();
        Package::new(
            "[agent]\nname = \"importer\"\nversion = \"0.1.0\"\n\n[limits]\nmemory_mb = 8\nrun_seconds = 1\ndisk_mb = 1\n",
            code,
        )
        .unwrap()
        .install(&storage)
        .unwrap();
        let record = arena
            .supervisor
            .run(
                "importer",
                &habitat_core::backend::Event::UserMessage("x".to_string()),
            )
            .unwrap();
        let Some(RunEnding::NotStarted(reason)) = &record.ending else {
            panic!("{interface}: {record:?}");
        };
        assert!(
            reason.contains("isn't part of the agent API"),
            "{interface}: {reason}"
        );
    }
}

#[test]
fn a_model_answer_doesnt_give_the_agent_the_key() {
    let arena = Arena::new();
    allow(&arena, Lifetime::ThisRun);
    arena.network.reply(PROVIDER, Reply::page(200, ANSWER));
    let report = arena.attack("ask-then-model default hello");
    assert!(report.agent_visible().contains("OK model"), "{report:?}");
    assert!(report.agent_visible().contains("Bees"), "{report:?}");
    no_breach(arena.check_no_leak(&report));
    no_breach(arena.check_secret_only_in_keystore());
    // It really was sent, by the core, to the provider and nowhere else.
    let sent = arena.network.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].secrets, 1);
}

const ANSWER: &str = "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Bees\"}}\n\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n\
data: {\"type\":\"message_stop\"}\n\n";

/// A provider or a server that echoes the key it was sent, in pieces and whole.
fn echoing_answer() -> String {
    let (first, second) = PLANTED_SECRET.split_at(7);
    format!(
        "data: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}}}\n\n\
data: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"whole {PLANTED_SECRET} and split {first}\"}}}}\n\n\
data: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"{second} end\"}}}}\n\n\
data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":2}}}}\n\n\
data: {{\"type\":\"message_stop\"}}\n\n"
    )
}

#[test]
fn an_echoing_provider_cant_return_the_key_to_the_agent() {
    let arena = Arena::new();
    allow(&arena, Lifetime::ThisRun);
    arena
        .network
        .reply(PROVIDER, Reply::page(200, &echoing_answer()));
    let report = arena.attack("ask-then-model default hello");
    no_breach(arena.check_no_leak(&report));
    let visible = report.agent_visible();
    assert!(
        visible.contains("whole") && visible.contains("end"),
        "{visible}"
    );
    // Not even a piece of it, joined up or not.
    assert!(!visible.contains(&PLANTED_SECRET[..7]), "{visible}");
}

#[test]
fn an_error_from_the_provider_cant_return_the_key_to_the_agent() {
    let body = format!("{{\"error\":\"bad key {PLANTED_SECRET}\"}}");
    for status in [400, 401, 403, 429, 500] {
        let arena = Arena::new();
        allow(&arena, Lifetime::ThisRun);
        arena.network.reply(PROVIDER, Reply::page(status, &body));
        let report = arena.attack("ask-then-model default hello");
        no_breach(arena.check_no_leak(&report));
        assert!(
            report.agent_visible().contains("NO model"),
            "{status}: {report:?}"
        );
    }
}

#[test]
fn an_agent_gets_no_key_by_fetching_what_the_host_would_send_it_to() {
    let arena = Arena::new();
    allow(&arena, Lifetime::ThisRun);
    let url = format!("https://{ALLOWED_HOST}/");
    arena.network.reply(&url, Reply::Echo);
    let report = arena.attack(&format!("ask-then-fetch {ALLOWED_HOST} {url}"));
    assert!(report.agent_visible().contains("OK fetch"), "{report:?}");
    no_breach(arena.check_no_leak(&report));
    assert_eq!(arena.network.sent.lock().unwrap()[0].secrets, 0);
}

#[test]
fn nothing_the_agent_can_do_gets_it_the_key() {
    let arena = Arena::with_limits(Limits {
        tokens: 100,
        ..Limits::default()
    });
    allow(&arena, Lifetime::ThisRun);
    let url = format!("https://{ALLOWED_HOST}/");
    arena.network.reply(&url, Reply::Echo);
    arena.network.reply(PROVIDER, Reply::Echo);
    for attack in [
        "read ../secrets.toml".to_string(),
        "read ../../../secrets.toml".to_string(),
        "read ../../../secrets.new".to_string(),
        "list ../../..".to_string(),
        "ask-model anthropic please".to_string(),
        "ask-host api.anthropic.com please".to_string(),
        "fetch https://api.anthropic.com/v1/messages".to_string(),
        format!("ask-then-fetch {ALLOWED_HOST} {url}"),
        "ask-then-model default hello".to_string(),
        "ask-then-model anthropic hello".to_string(),
        "panic".to_string(),
    ] {
        let report = arena.attack(&attack);
        no_breach(arena.check_no_leak(&report));
    }
    no_breach(arena.check_secret_only_in_keystore());
    assert_eq!(
        arena.store.get_for_tests("anthropic").as_deref(),
        Some(PLANTED_SECRET.as_bytes())
    );
}

#[test]
fn a_headers_the_agent_cant_set_to_change_where_a_request_goes() {
    let arena = Arena::new();
    allow(&arena, Lifetime::ThisRun);
    let url = format!("https://{ALLOWED_HOST}/");
    for header in [
        "Host=evil.example",
        "Transfer-Encoding=chunked",
        "Proxy-Authorization=x",
        "Content-Length=0",
    ] {
        arena.attack(&format!("ask-host {ALLOWED_HOST} please"));
        let report = arena.attack(&format!(
            "ask-then-fetch-with-header {ALLOWED_HOST} {url} {header}"
        ));
        assert!(
            report.agent_visible().contains("NO fetch"),
            "{header}: {report:?}"
        );
    }
    assert!(arena.network.urls_sent().is_empty());
}

#[test]
fn the_data_leak_rule_stops_a_long_lived_grant_after_a_private_read() {
    let arena = Arena::new();
    let private = arena.outside().join("secret.txt");
    *arena.user.file.lock().unwrap() = Some(private);
    allow(&arena, Lifetime::UntilRevoked);
    let url = format!("https://{ALLOWED_HOST}/");
    arena.network.reply(&url, Reply::page(200, "fine"));
    // Approved for good, and it works.
    let fine = arena.attack(&format!("ask-then-fetch {ALLOWED_HOST} {url}"));
    assert!(fine.agent_visible().contains("OK fetch"), "{fine:?}");
    let count = arena.network.urls_sent().len();

    // After reading a file, that grant isn't enough, and nothing goes out.
    *arena.user.answer.lock().unwrap() = Answer::Deny;
    let report = arena.attack(&format!("pick-then-fetch {url}"));
    assert!(report.agent_visible().contains("NO fetch"), "{report:?}");
    assert_eq!(arena.network.urls_sent().len(), count);

    // Asking gets a prompt that says why.
    arena.attack(&format!("pick-then-ask {ALLOWED_HOST}"));
    let prompts = arena.user.prompts.lock().unwrap();
    assert!(
        prompts
            .last()
            .unwrap()
            .contains("already read a private file"),
        "{prompts:?}"
    );
}

#[test]
fn a_grant_for_one_host_doesnt_cover_another() {
    let arena = Arena::new();
    allow(&arena, Lifetime::UntilRevoked);
    arena.attack(&format!("ask-host {ALLOWED_HOST} please"));
    let report = arena.attack(&format!("fetch https://{ALSO_ALLOWED_HOST}/"));
    assert!(report.agent_visible().contains("NotGranted"), "{report:?}");
    assert!(arena.network.urls_sent().is_empty());
}
