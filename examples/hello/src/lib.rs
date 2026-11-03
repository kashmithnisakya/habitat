//! A small agent that remembers who has visited. Each message is the name of a visitor. See
//! `docs/writing-an-agent.md`, which explains this code.

use habitat_sdk::{Agent, BoxError, Event, send_message, state};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
struct Visits {
    count: u64,
    last_visitor: String,
}

struct Hello;

impl Agent for Hello {
    fn handle(event: Event) -> Result<(), BoxError> {
        let Event::UserMessage(name) = event else {
            return Err("this agent only handles messages".into());
        };
        if name.trim().is_empty() {
            return Err("tell me who's visiting".into());
        }

        // The agent starts from nothing on each run, so this is the only thing it remembers.
        let mut visits: Visits = state::load("visits")?.unwrap_or_default();
        visits.count += 1;
        let last_visitor = std::mem::replace(&mut visits.last_visitor, name.clone());
        state::save("visits", &visits)?;

        if last_visitor.is_empty() {
            send_message(&format!("Hello, {name}! You're my first visitor."))?;
        } else {
            send_message(&format!(
                "Hello, {name}! You're visitor number {}. The last one was {last_visitor}.",
                visits.count
            ))?;
        }
        Ok(())
    }
}

habitat_sdk::agent!(Hello);
