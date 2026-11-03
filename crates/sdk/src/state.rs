//! Saving and loading values between runs, as JSON files in `state/` in the agent's home.
//!
//! **Each save is all or nothing, but only for that one value.** If the agent is stopped or the
//! host crashes, a value holds what it held before or what was saved, never a mix. Two separate
//! saves can still be split, with the first saved and the second not. So keep things that have to
//! match each other in one value.

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::files;
use crate::{Error, HomeError};

const MAX_NAME_BYTES: usize = 64;

/// The file that holds a value, or an error if its name breaks the rules. Only allowing these
/// characters means that a name can't lead to another folder or be refused by the host.
fn path(name: &str) -> Result<String, Error> {
    let allowed = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_';
    if name.is_empty() || name.len() > MAX_NAME_BYTES || !name.bytes().all(allowed) {
        return Err(Error::InvalidStateName);
    }
    Ok(format!("state/{name}.json"))
}

/// Saves a value under a name, replacing whatever was saved under it before.
pub fn save<T: Serialize + ?Sized>(name: &str, value: &T) -> Result<(), Error> {
    let path = path(name)?;
    let data = serde_json::to_vec(value).map_err(|error| Error::InvalidState(error.to_string()))?;
    files::write(&path, &data)
}

/// Loads the value saved under a name, or nothing if there isn't one. It's an error if the value
/// is there and can't be read, such as after changing the type, so that an agent doesn't start
/// over and overwrite it without meaning to.
pub fn load<T: DeserializeOwned>(name: &str) -> Result<Option<T>, Error> {
    let data = match files::read(&path(name)?) {
        Ok(data) => data,
        Err(Error::Host(HomeError::NotFound)) => return Ok(None),
        Err(error) => return Err(error),
    };
    serde_json::from_slice(&data)
        .map(Some)
        .map_err(|error| Error::InvalidState(error.to_string()))
}

/// Removes the value saved under a name, if there is one.
pub fn clear(name: &str) -> Result<(), Error> {
    match files::remove(&path(name)?) {
        Err(Error::Host(HomeError::NotFound)) => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn good_names_become_files_in_the_state_folder() {
        assert_eq!(path("visits").unwrap(), "state/visits.json");
        assert_eq!(path("A-b_9").unwrap(), "state/A-b_9.json");
        let longest = "x".repeat(MAX_NAME_BYTES);
        assert_eq!(path(&longest).unwrap(), format!("state/{longest}.json"));
    }

    #[test]
    fn other_names_are_refused() {
        let too_long = "x".repeat(MAX_NAME_BYTES + 1);
        for name in [
            "",
            ".",
            "..",
            "../secret",
            "a/b",
            "a\\b",
            "a.b",
            "a b",
            "a\0b",
            "é",
            too_long.as_str(),
        ] {
            assert!(
                matches!(path(name), Err(Error::InvalidStateName)),
                "{name:?}"
            );
        }
    }
}
