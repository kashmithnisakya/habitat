//! An agent that uses the SDK as each message tells it to, so that the SDK's tests can drive it. A
//! command fails if it doesn't get the expected result.

use habitat_sdk::{Agent, BoxError, Error, Event, HomeError, LogLevel, files, state};

struct Check;

impl Agent for Check {
    fn handle(event: Event) -> Result<(), BoxError> {
        let (text, scheduled) = match event {
            Event::UserMessage(text) => (text, false),
            Event::Scheduled(text) => (text, true),
        };
        if scheduled {
            return Err("this only handles messages".into());
        }
        let mut words = text.splitn(3, ' ');
        let command = words.next().unwrap_or_default();
        let first = words.next().unwrap_or_default();
        let rest = words.next().unwrap_or_default();
        match command {
            "save" => state::save(first, rest)?,
            "expect" => {
                let saved: Option<String> = state::load(first)?;
                if saved.as_deref() != Some(rest) {
                    return Err(format!("{first} holds {saved:?}").into());
                }
            }
            "expect-nothing" => {
                let saved: Option<String> = state::load(first)?;
                if let Some(saved) = saved {
                    return Err(format!("{first} holds {saved}").into());
                }
            }
            "save-big" => state::save(first, &"x".repeat(rest.parse()?))?,
            "clear" => state::clear(first)?,
            "corrupt" => files::write(&format!("state/{first}.json"), b"{not json")?,
            "load-as-number" => {
                let _: Option<u64> = state::load(first)?;
            }
            "big-file" => big_file(first)?,
            "file-text" => {
                files::write(first, rest.as_bytes())?;
                if files::read_to_string(first)? != rest {
                    return Err("the text changed".into());
                }
            }
            "not-text" => {
                files::write(first, &[0xff, 0xfe])?;
                match files::read_to_string(first) {
                    Err(Error::NotText) => {}
                    other => return Err(format!("reading it gave {other:?}").into()),
                }
            }
            "missing-file" => match files::read(first) {
                Err(Error::Host(HomeError::NotFound)) => {}
                other => return Err(format!("reading it gave {other:?}").into()),
            },
            "list" => {
                files::write("listed/a.txt", b"12")?;
                let entries = files::list("listed")?;
                if entries.len() != 1 || entries[0].name != "a.txt" || entries[0].size != 2 {
                    return Err(format!("listed {entries:?}").into());
                }
                files::remove("listed/a.txt")?;
                if files::list("")?.iter().any(|entry| entry.name == "listed") {
                    return Err("the empty folder is still there".into());
                }
            }
            "clock-and-random" => {
                // 2020-01-01, so a clock that always returns 0 fails.
                if habitat_sdk::now_ms() < 1_577_836_800_000 {
                    return Err("the clock is wrong".into());
                }
                let first = habitat_sdk::random_bytes(32)?;
                let second = habitat_sdk::random_bytes(32)?;
                if first.len() != 32 || first == second {
                    return Err("the random bytes aren't random".into());
                }
                habitat_sdk::log(LogLevel::Info, "logging doesn't fail");
            }
            "fail" => return Err(rest.to_string().into()),
            other => return Err(format!("unknown command: {other}").into()),
        }
        Ok(())
    }
}

/// Saves a file larger than one read can return, and checks that it comes back whole.
fn big_file(size: &str) -> Result<(), BoxError> {
    let size: usize = size.parse()?;
    let data: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();
    files::write("big", &data)?;
    if files::read("big")? != data {
        return Err("the file changed".into());
    }
    Ok(())
}

habitat_sdk::agent!(Check);
