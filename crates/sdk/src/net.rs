//! Reaching the network. Everything goes through the host, which decides what's allowed, and makes
//! the request itself. An agent never holds a connection.

use crate::Error;
use crate::bindings::habitat::host::agent_home as api;
use crate::bindings::habitat::host::types::{AccessTarget, HttpMethod, HttpRequest};

/// How much to ask for at a time. The host can return less.
const READ_SIZE: u32 = 256 * 1024;

/// A whole response.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Asks the user to let this agent reach a host. The host has to be listed in the manifest. The
/// reason is shown to the user, marked as this agent's own claim.
pub fn ask_access(host: &str, reason: &str) -> Result<(), Error> {
    api::request_access(&AccessTarget::NetworkHost(host.to_string()), reason)?;
    Ok(())
}

/// Gets a URL, reading the whole response into memory. It has to be `https://`. If this hasn't been
/// allowed, this fails with `HomeError::NotGranted`, and [`ask_access`] is how to ask.
pub fn get(url: &str) -> Result<Response, Error> {
    let response = api::http_fetch(&HttpRequest {
        method: HttpMethod::Get,
        url: url.to_string(),
        headers: Vec::new(),
        body: None,
    })?;
    let mut body = Vec::new();
    while let Some(piece) = response.body.read(READ_SIZE)? {
        body.extend_from_slice(&piece);
    }
    Ok(Response {
        status: response.status,
        headers: response
            .headers
            .into_iter()
            .map(|header| (header.name, header.value))
            .collect(),
        body,
    })
}
