//! The agent's own files, in its home. Paths are relative to the home, and can't lead out of it.

use crate::bindings::habitat::host::agent_home as api;
use crate::{DirEntry, Error};

/// How much to ask for at a time. The host can return less, and does for anything over 1 MB.
const READ_SIZE: u32 = 1024 * 1024;

/// Reads a whole file, which has to fit in the agent's memory.
pub fn read(path: &str) -> Result<Vec<u8>, Error> {
    let mut data = Vec::new();
    loop {
        let piece = api::read_file(path, data.len() as u64, READ_SIZE)?;
        if piece.is_empty() {
            return Ok(data);
        }
        data.extend_from_slice(&piece);
    }
}

/// Reads a whole file as text.
pub fn read_to_string(path: &str) -> Result<String, Error> {
    String::from_utf8(read(path)?).map_err(|_| Error::NotText)
}

/// Replaces a whole file, creating it and its parent folders if needed. If the agent is stopped or
/// the host crashes, the file holds its old contents or its new ones, never a mix.
pub fn write(path: &str, data: &[u8]) -> Result<(), Error> {
    Ok(api::write_file(path, data)?)
}

/// Removes a file, and any parent folders that this leaves empty.
pub fn remove(path: &str) -> Result<(), Error> {
    Ok(api::remove_file(path)?)
}

/// Lists a folder, and an empty path means the home itself.
pub fn list(path: &str) -> Result<Vec<DirEntry>, Error> {
    Ok(api::list_dir(path)?)
}
