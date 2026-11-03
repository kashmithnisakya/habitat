//! The smallest useful agent: it answers each message, and keeps a count in its home between runs.

use habitat_sdk::bindings::Guest;
use habitat_sdk::bindings::habitat::host::agent_home as api;
use habitat_sdk::bindings::habitat::host::types::{Event, HomeError};

const COUNT_FILE: &str = "count";
const MAX_COUNT_BYTES: u32 = 32;

struct Agent;

impl Guest for Agent {
    fn handle_event(event: Event) -> Result<(), String> {
        let Event::UserMessage(text) = event else {
            return Ok(());
        };
        let count = read_count()? + 1;
        api::write_file(COUNT_FILE, count.to_string().as_bytes())
            .map_err(|error| error.to_string())?;
        let reply = format!("You said: {text}\nThat's message {count} since I was installed.");
        api::send_message(&reply).map_err(|error| error.to_string())
    }
}

fn read_count() -> Result<u64, String> {
    match api::read_file(COUNT_FILE, 0, MAX_COUNT_BYTES) {
        Ok(data) => String::from_utf8_lossy(&data)
            .parse()
            .map_err(|_| "the saved count is unreadable".to_string()),
        Err(HomeError::NotFound) => Ok(0),
        Err(error) => Err(error.to_string()),
    }
}

habitat_sdk::bindings::export_agent!(Agent);
