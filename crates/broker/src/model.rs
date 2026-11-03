//! Lets agents use an AI model without ever holding its API key.
//!
//! The core makes the request and adds the key itself. An agent only sees the text that comes back,
//! as it arrives, and nothing about the provider reaches the agent API. This holds one provider
//! today, behind [`start`], so that adding another only changes this file.

use std::io::{self, BufRead, BufReader, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use habitat_core::backend::{
    FetchError, FetchMethod, FetchRequest, ModelAnswer, ModelRequest, ModelRole, TokenUsage,
};
use serde_json::{Value, json};

use crate::audit::{AuditError, AuditEvent, AuditLog};
use crate::gateway::{
    CheckedUrl, LimitedBody, MAX_RESPONSE_BYTES, Network, NetworkError, check_url,
};
use crate::vault::{Vault, VaultError};

const PROVIDER_HOST: &str = "api.anthropic.com";
const PROVIDER_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2026-10-21";
/// Asks the provider to retry with another model if a safety classifier declines a request.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// What an agent gets if it asks for `default`.
const DEFAULT_MODEL: &str = "claude-opus-5-5";
/// The secret that holds this provider's key. See `docs/permissions.md`.
pub const SECRET_NAME: &str = "anthropic";
const MAX_MESSAGES: usize = 1024;
const MAX_REQUEST_TEXT_BYTES: usize = 8 * 1024 * 1024;
const MAX_LINE_BYTES: u64 = 1024 * 1024;

/// What's shared between one run's broker and the answers it has open.
#[derive(Clone)]
pub(crate) struct RunShared {
    pub(crate) audit: Arc<Mutex<Result<AuditLog, AuditError>>>,
    pub(crate) tokens_left: Arc<AtomicU64>,
}

impl RunShared {
    fn spend(&self, tokens: u64) {
        // One run only makes one call at a time, so nothing can change this in between.
        let left = self.tokens_left.load(Ordering::SeqCst);
        self.tokens_left
            .store(left.saturating_sub(tokens), Ordering::SeqCst);
    }

    fn log(&self, event: &AuditEvent) {
        let mut audit = self.audit.lock().unwrap_or_else(PoisonError::into_inner);
        if let Ok(audit) = audit.as_mut() {
            let _ = audit.append(event);
        }
    }
}

/// The provider's name for a model. Only names it's known to use get through.
fn provider_model(name: &str) -> Option<&str> {
    match name {
        "default" => Some(DEFAULT_MODEL),
        other if other.starts_with("claude-") => Some(other),
        _ => None,
    }
}

fn body_for(model: &str, request: &ModelRequest, max_tokens: u64) -> Result<Vec<u8>, FetchError> {
    let mut system = String::new();
    let mut messages = Vec::new();
    for message in &request.messages {
        match message.role {
            ModelRole::System => {
                if !system.is_empty() {
                    system.push('\n');
                }
                system.push_str(&message.text);
            }
            ModelRole::User => messages.push(json!({"role": "user", "content": message.text})),
            ModelRole::Assistant => {
                messages.push(json!({"role": "assistant", "content": message.text}));
            }
        }
    }
    if messages.is_empty() {
        return Err(FetchError::InvalidRequest(
            "a request has to hold a message from the user".to_string(),
        ));
    }
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": true,
        "messages": messages,
        "fallbacks": "default",
    });
    if !system.is_empty() {
        body["system"] = Value::String(system);
    }
    serde_json::to_vec(&body).map_err(|_| FetchError::Internal)
}

fn check_size(request: &ModelRequest) -> Result<(), FetchError> {
    let total: usize = request
        .messages
        .iter()
        .map(|message| message.text.len())
        .sum();
    if request.messages.len() > MAX_MESSAGES || total > MAX_REQUEST_TEXT_BYTES {
        return Err(FetchError::InvalidRequest(
            "the request is too large".to_string(),
        ));
    }
    Ok(())
}

/// Turns a status from the provider into something an agent can act on. What the provider said
/// isn't passed on, because the agent doesn't need it, and it could mention the key.
fn error_for_status(status: u16) -> FetchError {
    match status {
        400 | 404 | 413 | 422 => {
            FetchError::InvalidRequest("the model provider refused the request".to_string())
        }
        401 | 403 => FetchError::Denied(
            "the model provider refused the API key, which the user has to fix".to_string(),
        ),
        429 => FetchError::Unavailable(
            "the model provider is rate limiting, so try again later".to_string(),
        ),
        _ => FetchError::Unavailable("the model provider failed, so try again later".to_string()),
    }
}

/// Starts a request. The caller has already checked that the agent can use this model, and how many
/// tokens it has left.
pub(crate) fn start(
    network: &dyn Network,
    vault: Option<&Vault>,
    request: &ModelRequest,
    shared: RunShared,
    wait: Duration,
) -> Result<Box<dyn ModelAnswer + Send>, FetchError> {
    check_size(request)?;
    let model = provider_model(&request.model).ok_or_else(|| {
        FetchError::Denied("this host has no provider for that model".to_string())
    })?;
    let left = shared.tokens_left.load(Ordering::SeqCst);
    // It can't ask for more than it has left. What it has read so far counts too, afterwards.
    let max_tokens = u64::from(request.max_tokens).min(left);
    if max_tokens == 0 {
        return Err(FetchError::InvalidRequest(
            "a request has to allow at least one token".to_string(),
        ));
    }
    let body = body_for(model, request, max_tokens)?;

    let no_key = || {
        FetchError::Unavailable(
            "this host has no API key for models, so the user has to add one".to_string(),
        )
    };
    let vault = vault.ok_or_else(no_key)?;
    let key = vault
        .header_for(SECRET_NAME, PROVIDER_HOST)
        .map_err(|error| match error {
            VaultError::NotFound | VaultError::KeystoreUnavailable => no_key(),
            _ => FetchError::Internal,
        })?;

    let url: CheckedUrl = check_url(PROVIDER_URL).map_err(|_| FetchError::Internal)?;
    let outgoing = FetchRequest {
        method: FetchMethod::Post,
        url: PROVIDER_URL.to_string(),
        headers: vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("anthropic-version".to_string(), API_VERSION.to_string()),
            ("anthropic-beta".to_string(), FALLBACK_BETA.to_string()),
            ("accept".to_string(), "text/event-stream".to_string()),
        ],
        body: Some(body),
    };
    let incoming = network
        .send(&url, &outgoing, std::slice::from_ref(&key), wait)
        .map_err(|error| match error {
            NetworkError::TimedOut => FetchError::TimedOut,
            NetworkError::Blocked | NetworkError::Failed => {
                FetchError::Unavailable("the model provider couldn't be reached".to_string())
            }
        })?;
    if incoming.status != 200 {
        return Err(error_for_status(incoming.status));
    }
    Ok(Box::new(Answer {
        body: BufReader::new(LimitedBody {
            inner: incoming.body,
            left: MAX_RESPONSE_BYTES,
        }),
        shared,
        model: request.model.clone(),
        usage: TokenUsage::default(),
        finished: false,
        logged: false,
    }))
}

/// One answer as it arrives. It stops at the end of the stream, or when it's dropped, and either way
/// what was used gets recorded.
struct Answer<R: Read + Send> {
    body: BufReader<LimitedBody<R>>,
    shared: RunShared,
    model: String,
    usage: TokenUsage,
    finished: bool,
    logged: bool,
}

impl<R: Read + Send> Answer<R> {
    /// Reads one line, without holding any more than the limit in memory.
    fn read_line(&mut self) -> Result<Option<String>, FetchError> {
        let mut line = Vec::new();
        let count = (&mut self.body)
            .take(MAX_LINE_BYTES + 1)
            .read_until(b'\n', &mut line)
            .map_err(|error| match error.kind() {
                io::ErrorKind::FileTooLarge => {
                    FetchError::LimitExceeded("the model's answer is too large".to_string())
                }
                io::ErrorKind::TimedOut => FetchError::TimedOut,
                _ => FetchError::Unavailable("reading the model's answer failed".to_string()),
            })?;
        if count == 0 {
            return Ok(None);
        }
        if count as u64 > MAX_LINE_BYTES {
            return Err(FetchError::LimitExceeded(
                "the model's answer is too large".to_string(),
            ));
        }
        String::from_utf8(line)
            .map(Some)
            .map_err(|_| FetchError::Unavailable("the model's answer wasn't valid".to_string()))
    }

    fn add_usage(&mut self, input: Option<u64>, output: Option<u64>) {
        if let Some(input) = input {
            let new = saturating_u32(input);
            self.shared
                .spend(u64::from(new.saturating_sub(self.usage.input_tokens)));
            self.usage.input_tokens = new;
        }
        // This is a running total, so only what's new counts.
        if let Some(output) = output {
            let new = saturating_u32(output);
            self.shared
                .spend(u64::from(new.saturating_sub(self.usage.output_tokens)));
            self.usage.output_tokens = new;
        }
    }

    fn log_usage(&mut self) {
        if !self.logged {
            self.logged = true;
            self.shared.log(&AuditEvent::ModelUsed {
                model: self.model.clone(),
                input_tokens: self.usage.input_tokens,
                output_tokens: self.usage.output_tokens,
            });
        }
    }

    fn fail<T>(&mut self, error: FetchError) -> Result<T, FetchError> {
        self.finished = true;
        self.log_usage();
        Err(error)
    }
}

fn saturating_u32(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn number(value: &Value, pointer: &str) -> Option<u64> {
    value.pointer(pointer).and_then(Value::as_u64)
}

impl<R: Read + Send> ModelAnswer for Answer<R> {
    fn next(&mut self) -> Result<Option<String>, FetchError> {
        if self.finished {
            return Ok(None);
        }
        loop {
            let line = match self.read_line() {
                Ok(Some(line)) => line,
                // The stream has to say that it's finished, or it was cut off.
                Ok(None) => {
                    return self.fail(FetchError::Unavailable(
                        "the model's answer ended early".to_string(),
                    ));
                }
                Err(error) => return self.fail(error),
            };
            let Some(data) = line.trim_end().strip_prefix("data:") else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<Value>(data.trim_start()) else {
                return self.fail(FetchError::Unavailable(
                    "the model's answer wasn't valid".to_string(),
                ));
            };
            match event.get("type").and_then(Value::as_str) {
                Some("message_start") => {
                    self.add_usage(
                        number(&event, "/message/usage/input_tokens"),
                        number(&event, "/message/usage/output_tokens"),
                    );
                }
                Some("content_block_delta") => {
                    if event.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta")
                        && let Some(text) = event.pointer("/delta/text").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        return Ok(Some(text.to_string()));
                    }
                }
                Some("message_delta") => {
                    self.add_usage(
                        number(&event, "/usage/input_tokens"),
                        number(&event, "/usage/output_tokens"),
                    );
                    if event.pointer("/delta/stop_reason").and_then(Value::as_str)
                        == Some("refusal")
                    {
                        return self.fail(FetchError::Denied(
                            "the model declined to answer".to_string(),
                        ));
                    }
                }
                Some("message_stop") => {
                    self.finished = true;
                    self.log_usage();
                    return Ok(None);
                }
                Some("error") => {
                    return self.fail(FetchError::Unavailable(
                        "the model provider failed, so try again later".to_string(),
                    ));
                }
                // Anything else, such as thinking or a ping, isn't for the agent.
                _ => {}
            }
        }
    }

    fn usage(&self) -> Option<TokenUsage> {
        self.finished.then_some(self.usage)
    }
}

impl<R: Read + Send> Drop for Answer<R> {
    fn drop(&mut self) {
        self.log_usage();
    }
}
