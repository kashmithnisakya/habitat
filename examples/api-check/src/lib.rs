//! Uses every call in the agent API, to check that the guest bindings are complete. CI builds this
//! into a component and checks what it imports.

use habitat_sdk::bindings::Guest;
use habitat_sdk::bindings::habitat::host::agent_home as api;
use habitat_sdk::bindings::habitat::host::types::{
    AccessTarget, Event, HomeError, HttpMethod, HttpRequest, LogLevel, ModelMessage, ModelRequest,
    ModelRole,
};

struct Agent;

impl Guest for Agent {
    fn handle_event(event: Event) -> Result<(), String> {
        use_api(event).map_err(|error| error.to_string())
    }
}

fn use_api(event: Event) -> Result<(), HomeError> {
    let text = match event {
        Event::UserMessage(text) | Event::Scheduled(text) => text,
    };

    api::write_file("notes/latest.txt", text.as_bytes())?;
    api::read_file("notes/latest.txt", 0, 4096)?;
    for entry in api::list_dir("notes")? {
        api::log(LogLevel::Info, &format!("{}: {}", entry.name, entry.size));
    }
    api::remove_file("notes/latest.txt")?;

    api::log(LogLevel::Debug, &api::now_ms().to_string());
    api::random_bytes(16)?;

    let target = AccessTarget::NetworkHost("api.github.com".to_string());
    api::request_access(&target, "To look up the project")?;
    let response = api::http_fetch(&HttpRequest {
        method: HttpMethod::Get,
        url: "https://api.github.com/".to_string(),
        headers: Vec::new(),
        body: None,
    })?;
    while let Some(piece) = response.body.read(64 * 1024)? {
        api::log(LogLevel::Debug, &piece.len().to_string());
    }

    let answer = api::model_complete(&ModelRequest {
        model: "default".to_string(),
        messages: vec![ModelMessage {
            role: ModelRole::User,
            text,
        }],
        max_tokens: 100,
    })?;
    while let Some(piece) = answer.next()? {
        api::send_message(&piece)?;
    }
    if let Some(usage) = answer.usage() {
        api::log(LogLevel::Debug, &usage.output_tokens.to_string());
    }

    let file = api::pick_user_file("To summarize it")?;
    api::log(LogLevel::Debug, &file.name());
    file.read(file.size().saturating_sub(16), 16)?;
    Ok(())
}

habitat_sdk::bindings::export_agent!(Agent);
