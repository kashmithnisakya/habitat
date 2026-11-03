//! An agent that tries to do what it shouldn't, as each message tells it to, and says what happened.
//! The attack tests don't rely on what it says. They check for themselves, from outside.
//!
//! Every attempt reports `OK <what>: <what it got>` or `NO <what>: <why>`, so that anything it
//! manages to get hold of ends up somewhere the harness can see it.

use habitat_sdk::bindings::Guest;
use habitat_sdk::bindings::habitat::host::agent_home as api;
use habitat_sdk::bindings::habitat::host::types::{
    AccessTarget, Event, HttpMethod, HttpRequest, ModelMessage, ModelRequest, ModelRole,
};

struct Agent;

fn report<T: std::fmt::Debug, E: std::fmt::Debug>(what: &str, result: Result<T, E>) {
    let text = match result {
        Ok(value) => format!("OK {what}: {value:?}"),
        Err(error) => format!("NO {what}: {error:?}"),
    };
    // A message that can't be sent is one of the things that can be refused.
    let _ = api::send_message(&text);
}

fn text_of(data: Vec<u8>) -> String {
    String::from_utf8_lossy(&data).into_owned()
}

fn read_body(url: &str, headers: Vec<(String, String)>) -> Result<String, String> {
    use habitat_sdk::bindings::habitat::host::types::Header;

    let response = api::http_fetch(&HttpRequest {
        method: HttpMethod::Get,
        url: url.to_string(),
        headers: headers
            .into_iter()
            .map(|(name, value)| Header { name, value })
            .collect(),
        body: None,
    })
    .map_err(|error| format!("{error:?}"))?;
    let shown: Vec<String> = response
        .headers
        .iter()
        .map(|header| format!("{}={}", header.name, header.value))
        .collect();
    let mut body = Vec::new();
    while let Some(piece) = response
        .body
        .read(64 * 1024)
        .map_err(|error| format!("{error:?}"))?
    {
        body.extend_from_slice(&piece);
    }
    Ok(format!(
        "status {} headers {shown:?} body {}",
        response.status,
        text_of(body)
    ))
}

fn ask_model(text: &str) -> Result<String, String> {
    let answer = api::model_complete(&ModelRequest {
        model: "default".to_string(),
        messages: vec![ModelMessage {
            role: ModelRole::User,
            text: text.to_string(),
        }],
        max_tokens: 100_000_000,
    })
    .map_err(|error| format!("{error:?}"))?;
    let mut all = String::new();
    while let Some(piece) = answer.next().map_err(|error| format!("{error:?}"))? {
        all.push_str(&piece);
    }
    Ok(all)
}

#[allow(unconditional_recursion, reason = "that's the attack")]
fn recurse(depth: u64) -> u64 {
    let frame = std::hint::black_box([depth; 64]);
    std::hint::black_box(recurse(frame[0] + 1)) + frame[63]
}

/// Fills as much of its home as it can, one file at a time, until it's told no.
fn fill_disk() -> usize {
    let block = vec![7_u8; 64 * 1024];
    let mut written = 0;
    while api::write_file(&format!("fill-{written}"), &block).is_ok() {
        written += 1;
    }
    written
}

impl Guest for Agent {
    fn handle_event(event: Event) -> Result<(), String> {
        let text = match event {
            Event::UserMessage(text) | Event::Scheduled(text) => text,
        };
        let mut words = text.splitn(3, ' ');
        let command = words.next().unwrap_or_default();
        let first = words.next().unwrap_or_default();
        let rest = words.next().unwrap_or_default();
        match command {
            "read" => report(
                &format!("read {first}"),
                api::read_file(first, 0, u32::MAX).map(text_of),
            ),
            "write" => report(
                &format!("write {first}"),
                api::write_file(first, rest.as_bytes()),
            ),
            "remove" => report(&format!("remove {first}"), api::remove_file(first)),
            "list" => report(&format!("list {first}"), api::list_dir(first)),
            "ask-host" => report(
                &format!("ask-host {first}"),
                api::request_access(&AccessTarget::NetworkHost(first.to_string()), rest),
            ),
            "ask-model" => report(
                &format!("ask-model {first}"),
                api::request_access(&AccessTarget::Model(first.to_string()), rest),
            ),
            "fetch" => report(&format!("fetch {first}"), read_body(first, Vec::new())),
            "fetch-with-header" => {
                let (name, value) = rest.split_once('=').unwrap_or((rest, ""));
                report(
                    &format!("fetch-with-header {first}"),
                    read_body(first, vec![(name.to_string(), value.to_string())]),
                );
            }
            "model" => report("model", ask_model(first)),
            "pick" => {
                let file = api::pick_user_file(first).map(|file| {
                    let contents = file.read(0, u32::MAX).map(text_of);
                    format!("name {} contents {contents:?}", file.name())
                });
                report("pick", file);
            }
            "spam-ask" => {
                let count: u32 = first.parse().unwrap_or_default();
                for index in 0..count {
                    let target = AccessTarget::NetworkHost(format!("host{index}.example"));
                    let _ = api::request_access(&target, "please");
                }
                let _ = api::send_message("OK spam-ask finished");
            }
            "ask-same" => {
                let count: u32 = first.parse().unwrap_or_default();
                let target = AccessTarget::NetworkHost(rest.to_string());
                let mut last = None;
                for _ in 0..count {
                    last = Some(api::request_access(&target, "please please please"));
                }
                report("ask-same", last.unwrap_or(Ok(api_grant(target))));
            }
            "spam-messages" => {
                let count: u32 = first.parse().unwrap_or_default();
                let mut sent = 0;
                for _ in 0..count {
                    if api::send_message("spam").is_ok() {
                        sent += 1;
                    }
                }
                let _ = sent;
            }
            "big-write" => {
                let size: usize = first.parse().unwrap_or_default();
                report("big-write", api::write_file("big", &vec![1; size]));
            }
            "big-path" => {
                let size: usize = first.parse().unwrap_or_default();
                report(
                    "big-path",
                    api::read_file(&"x".repeat(size), 0, 1).map(text_of),
                );
            }
            "big-read" => report(
                "big-read",
                api::read_file(first, 0, u32::MAX).map(|data| data.len()),
            ),
            "big-message" => {
                let size: usize = first.parse().unwrap_or_default();
                report("big-message", api::send_message(&"x".repeat(size)));
            }
            "big-reason" => {
                let size: usize = first.parse().unwrap_or_default();
                let target = AccessTarget::NetworkHost(rest.to_string());
                report(
                    "big-reason",
                    api::request_access(&target, &"x".repeat(size)),
                );
            }
            "big-url" => {
                let size: usize = first.parse().unwrap_or_default();
                let url = format!("https://allowed.example/{}", "x".repeat(size));
                report("big-url", read_body(&url, Vec::new()));
            }
            "big-random" => report(
                "big-random",
                api::random_bytes(u32::MAX).map(|data| data.len()),
            ),
            "fail-huge" => {
                let size: usize = first.parse().unwrap_or_default();
                return Err("x".repeat(size));
            }
            "fill-disk" => {
                let written = fill_disk();
                let _ = api::send_message(&format!("OK fill-disk wrote {written} files"));
            }
            "spin" => loop {
                std::hint::black_box(0);
            },
            "spin-on-host" => loop {
                std::hint::black_box(api::now_ms());
            },
            "grow" => {
                let mut all = Vec::new();
                loop {
                    all.push(vec![1_u8; 1024 * 1024]);
                    std::hint::black_box(&all);
                }
            }
            "recurse" => {
                std::hint::black_box(recurse(0));
            }
            "panic" => panic!("told to"),
            other => return Err(format!("unknown attack: {other}")),
        }
        Ok(())
    }
}

/// Only here so that a count of 0 has something to report. It's never been granted.
fn api_grant(target: AccessTarget) -> habitat_sdk::bindings::habitat::host::types::Grant {
    habitat_sdk::bindings::habitat::host::types::Grant {
        target,
        lifetime: habitat_sdk::bindings::habitat::host::types::GrantLifetime::Once,
    }
}

habitat_sdk::bindings::export_agent!(Agent);
