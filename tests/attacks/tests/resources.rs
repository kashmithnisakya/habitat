//! An agent trying to make the device unusable. Each of these has to end within its limit, and the
//! next run has to work.

use std::time::{Duration, Instant};

use habitat_attacks::{ALLOWED_HOST, Arena, Limits, Reply, no_breach};
use habitat_broker::approval::Answer;
use habitat_core::backend::{Lifetime, RunOutcome};
use habitat_core::run_log::RunEnding;

/// How far past a limit a run can go before something's wrong, as CI can be slow to compile.
const SLACK: Duration = Duration::from_secs(60);

fn quick() -> Arena {
    Arena::with_limits(Limits {
        memory_mb: 32,
        run_seconds: 2,
        disk_mb: 1,
        tokens: 100,
    })
}

fn ended_as(arena: &Arena, attack: &str, expected: RunOutcome) {
    let start = Instant::now();
    let report = arena.attack(attack);
    assert_eq!(
        report.record.ending,
        Some(RunEnding::Ran(expected)),
        "{attack}: {report:?}"
    );
    assert!(start.elapsed() < Duration::from_secs(2) + SLACK, "{attack}");
    no_breach(arena.check_host_still_works());
}

#[test]
fn a_cpu_loop_is_stopped_at_the_time_limit() {
    let arena = quick();
    ended_as(&arena, "spin", RunOutcome::TimeLimit);
}

#[test]
fn a_loop_of_host_calls_is_stopped_at_the_time_limit() {
    let arena = quick();
    ended_as(&arena, "spin-on-host", RunOutcome::TimeLimit);
}

#[test]
fn memory_growth_is_stopped_at_the_memory_limit() {
    let arena = quick();
    ended_as(&arena, "grow", RunOutcome::MemoryLimit);
}

#[test]
fn deep_recursion_crashes_the_agent_and_not_the_host() {
    let arena = quick();
    let report = arena.attack("recurse");
    assert!(
        matches!(
            report.record.ending,
            Some(RunEnding::Ran(RunOutcome::Crashed(_)))
        ),
        "{report:?}"
    );
    no_breach(arena.check_host_still_works());
}

#[test]
fn filling_the_disk_ends_and_the_next_run_works() {
    let arena = quick();
    let report = arena.attack("fill-disk");
    assert_eq!(
        report.record.ending,
        Some(RunEnding::Ran(RunOutcome::Completed))
    );
    // It's full, so the next run can't save anything, but it can run, and tell you so.
    let next = arena.attack("write another.txt x");
    assert_eq!(
        next.record.ending,
        Some(RunEnding::Ran(RunOutcome::Completed))
    );
    assert!(next.messages[0].starts_with("NO"), "{:?}", next.messages);
    // Once there's room again, all is well.
    for entry in std::fs::read_dir(arena.home()).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    no_breach(arena.check_host_still_works());
}

#[test]
fn very_large_inputs_to_host_functions_are_refused_and_the_host_is_fine() {
    let arena = Arena::with_limits(Limits {
        memory_mb: 256,
        run_seconds: 30,
        disk_mb: 1,
        tokens: 100,
    });
    *arena.user.answer.lock().unwrap() = Answer::Allow(Lifetime::ThisRun);
    for attack in [
        "big-write 100000000",
        "big-path 100000000",
        "big-url 100000000",
        "big-random 0",
    ] {
        let report = arena.attack(attack);
        assert!(
            report.messages[0].starts_with("NO"),
            "{attack}: {:?}",
            report.messages
        );
    }
    no_breach(arena.check_host_still_works());
}

#[test]
fn a_huge_message_or_reason_is_cut_short() {
    let arena = Arena::with_limits(Limits {
        memory_mb: 256,
        run_seconds: 30,
        disk_mb: 1,
        tokens: 100,
    });
    let report = arena.attack("big-message 100000000");
    let total: usize = report.messages.iter().map(String::len).sum();
    assert!(total < 100_000, "{total}");

    arena.attack(&format!("big-reason 100000000 {ALLOWED_HOST}"));
    let prompts = arena.user.prompts.lock().unwrap();
    assert!(prompts[0].len() < 10_000, "{}", prompts[0].len());
    drop(prompts);
    no_breach(arena.check_host_still_works());
}

#[test]
fn a_huge_error_from_an_agent_is_cut_short_in_its_record() {
    let arena = Arena::with_limits(Limits {
        memory_mb: 256,
        run_seconds: 30,
        disk_mb: 1,
        tokens: 100,
    });
    let report = arena.attack("fail-huge 100000000");
    let Some(RunEnding::Ran(RunOutcome::Failed(text))) = &report.record.ending else {
        panic!("{report:?}");
    };
    assert!(text.len() < 10_000, "{}", text.len());
    no_breach(arena.check_host_still_works());
}

#[test]
fn a_flood_of_messages_is_cut_off() {
    let arena = quick();
    let report = arena.attack("spam-messages 100000");
    assert!(report.messages.len() <= 100, "{}", report.messages.len());
    no_breach(arena.check_host_still_works());
}

#[test]
fn a_very_large_response_ends_in_an_error_or_a_stopped_run_and_the_host_is_fine() {
    let arena = Arena::with_limits(Limits {
        memory_mb: 32,
        run_seconds: 30,
        disk_mb: 1,
        tokens: 100,
    });
    *arena.user.answer.lock().unwrap() = Answer::Allow(Lifetime::ThisRun);
    let url = format!("https://{ALLOWED_HOST}/");
    arena.network.reply(
        &url,
        Reply::Page {
            status: 200,
            headers: Vec::new(),
            body: vec![7; 65 * 1024 * 1024],
        },
    );
    let report = arena.attack(&format!("ask-then-fetch {ALLOWED_HOST} {url}"));
    // Either way, it doesn't get the whole thing.
    assert!(
        !report.agent_visible().contains("OK fetch"),
        "{:?}",
        report.record.ending
    );
    no_breach(arena.check_host_still_works());
}

#[test]
fn the_model_budget_stops_new_requests() {
    let arena = Arena::with_limits(Limits {
        tokens: 10,
        ..Limits::default()
    });
    *arena.user.answer.lock().unwrap() = Answer::Allow(Lifetime::ThisRun);
    const PROVIDER: &str = "https://api.anthropic.com/v1/messages";
    arena.network.reply(
        PROVIDER,
        Reply::page(
            200,
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":20}}}\n\n\
data: {\"type\":\"message_stop\"}\n\n",
        ),
    );
    let report = arena.attack("ask-then-model-twice default hello");
    assert_eq!(arena.network.urls_sent().len(), 1, "{:?}", report.messages);
    assert!(
        report.agent_visible().contains("LimitExceeded"),
        "{report:?}"
    );
    // The request it did make couldn't ask for more than the budget.
    let sent = arena.network.sent.lock().unwrap();
    let body = String::from_utf8(sent[0].request.body.clone().unwrap()).unwrap();
    assert!(body.contains("\"max_tokens\":10"), "{body}");
    drop(sent);
    no_breach(arena.check_host_still_works());
}
