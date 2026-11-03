//! An agent that does what each message tells it to with the files in its home, so that the
//! backend's tests can drive it. A command fails if it doesn't get the expected result.

use habitat_sdk::bindings::Guest;
use habitat_sdk::bindings::habitat::host::agent_home as api;
use habitat_sdk::bindings::habitat::host::types::{EntryKind, Event, HomeError};

struct Agent;

impl Guest for Agent {
    fn handle_event(event: Event) -> Result<(), String> {
        let (text, scheduled) = match event {
            Event::UserMessage(text) => (text, false),
            Event::Scheduled(name) => (name, true),
        };
        let mut words = text.splitn(3, ' ');
        let command = words.next().unwrap_or_default();
        let first = words.next().unwrap_or_default();
        let rest = words.next().unwrap_or_default();
        match command {
            "write" => api::write_file(first, rest.as_bytes()).map_err(|error| error.to_string()),
            "expect-user-message" if !scheduled => Ok(()),
            "expect-scheduled" if scheduled => Ok(()),
            "expect" => expect(first, rest),
            "expect-big" => expect_big(first, rest),
            "expect-list" => expect_list(first, rest),
            "remove" => api::remove_file(first).map_err(|error| error.to_string()),
            "expect-error" => expect_error(first, rest),
            "expect-denied-outside" => expect_denied_outside(),
            "check-clock-and-random" => check_clock_and_random(),
            "remember" => {
                // SAFETY: agents are single threaded, and nothing else uses this.
                unsafe { MEMORY += 1 };
                Ok(())
            }
            "expect-forgotten" => {
                // SAFETY: as above.
                match unsafe { MEMORY } {
                    0 => Ok(()),
                    other => Err(format!("still remembers {other}")),
                }
            }
            "fail" => Err(rest.to_string()),
            "panic" => panic!("told to"),
            other => Err(format!("unknown command: {other}")),
        }
    }
}

static mut MEMORY: u32 = 0;

fn read_all(path: &str) -> Result<Vec<u8>, HomeError> {
    let mut data = Vec::new();
    loop {
        let piece = api::read_file(path, data.len() as u64, 1024 * 1024)?;
        if piece.is_empty() {
            return Ok(data);
        }
        data.extend_from_slice(&piece);
    }
}

fn expect(path: &str, expected: &str) -> Result<(), String> {
    let data = read_all(path).map_err(|error| error.to_string())?;
    if data != expected.as_bytes() {
        return Err(format!("{path} holds {}", String::from_utf8_lossy(&data)));
    }
    Ok(())
}

/// Writes a file larger than one read returns, then reads it back with that same maximum.
fn expect_big(path: &str, size: &str) -> Result<(), String> {
    let size: usize = size.parse().map_err(|_| "bad size".to_string())?;
    let data: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();
    api::write_file(path, &data).map_err(|error| error.to_string())?;

    // The host returns less than this for each call, so this has to take several.
    let first = api::read_file(path, 0, u32::MAX).map_err(|error| error.to_string())?;
    if first.len() >= size {
        return Err(format!("one read returned {} bytes", first.len()));
    }
    if read_all(path).map_err(|error| error.to_string())? != data {
        return Err("the file changed".to_string());
    }
    Ok(())
}

/// Checks a listing against something like `dir/ file:5`.
fn expect_list(path: &str, expected: &str) -> Result<(), String> {
    let path = if path == "." { "" } else { path };
    let entries = api::list_dir(path).map_err(|error| error.to_string())?;
    let listed: Vec<String> = entries
        .iter()
        .map(|entry| match entry.kind {
            EntryKind::Directory => format!("{}/", entry.name),
            EntryKind::File => format!("{}:{}", entry.name, entry.size),
        })
        .collect();
    if listed.join(" ") != expected {
        return Err(format!("listed {}", listed.join(" ")));
    }
    Ok(())
}

/// Reads a path that must fail, and checks the kind of error.
fn expect_error(path: &str, kind: &str) -> Result<(), String> {
    let error = match api::read_file(path, 0, 16) {
        Ok(_) => return Err(format!("{path} could be read")),
        Err(error) => error,
    };
    let actual = match error {
        HomeError::NotFound => "not-found",
        HomeError::Denied(_) => "denied",
        HomeError::InvalidRequest(_) => "invalid-request",
        HomeError::LimitExceeded(_) => "limit-exceeded",
        _ => "other",
    };
    if actual != kind {
        return Err(format!("{path} gave {actual}"));
    }
    Ok(())
}

/// Everything that needs the broker must be denied until the broker exists.
fn expect_denied_outside() -> Result<(), String> {
    use habitat_sdk::bindings::habitat::host::types::{
        AccessTarget, HttpMethod, HttpRequest, ModelRequest,
    };

    let denied = |error: HomeError, call: &str| match error {
        HomeError::Denied(_) => Ok(()),
        other => Err(format!("{call} gave {other}")),
    };
    let http = api::http_fetch(&HttpRequest {
        method: HttpMethod::Get,
        url: "https://example.com/".to_string(),
        headers: Vec::new(),
        body: None,
    });
    denied(http.err().ok_or("http-fetch worked")?, "http-fetch")?;
    let model = api::model_complete(&ModelRequest {
        model: "default".to_string(),
        messages: Vec::new(),
        max_tokens: 1,
    });
    denied(
        model.err().ok_or("model-complete worked")?,
        "model-complete",
    )?;
    let target = AccessTarget::NetworkHost("example.com".to_string());
    let access = api::request_access(&target, "testing");
    denied(
        access.err().ok_or("request-access worked")?,
        "request-access",
    )?;
    let file = api::pick_user_file("testing");
    denied(file.err().ok_or("pick-user-file worked")?, "pick-user-file")?;
    let message = api::send_message("hello");
    denied(message.err().ok_or("send-message worked")?, "send-message")
}

fn check_clock_and_random() -> Result<(), String> {
    // 2020-01-01, so a clock that always returns 0 fails.
    if api::now_ms() < 1_577_836_800_000 {
        return Err("the clock is wrong".to_string());
    }
    let first = api::random_bytes(32).map_err(|error| error.to_string())?;
    let second = api::random_bytes(32).map_err(|error| error.to_string())?;
    if first.len() != 32 || first == second {
        return Err("random bytes aren't random".to_string());
    }
    if api::random_bytes(1024)
        .map_err(|error| error.to_string())?
        .len()
        != 1024
    {
        return Err("couldn't get the largest amount of random bytes".to_string());
    }
    match api::random_bytes(1025) {
        Err(HomeError::LimitExceeded(_)) => Ok(()),
        _ => Err("asking for too many random bytes wasn't refused".to_string()),
    }
}

habitat_sdk::bindings::export_agent!(Agent);
