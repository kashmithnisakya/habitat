//! A small agent that remembers who has visited. Each message is the name of a visitor. See
//! `docs/writing-an-agent.md`, which explains this code.

use habitat_sdk::{Agent, BoxError, Event, state};
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
        visits.last_visitor = name;
        state::save("visits", &visits)?;
        Ok(())
    }
}

habitat_sdk::agent!(Hello);
