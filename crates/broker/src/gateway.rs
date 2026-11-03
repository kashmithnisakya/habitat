//! Makes network requests for agents, without becoming a way around what the user approved.
//!
//! The rules for what's allowed are in [`crate::broker`], which checks them on every hop. This holds
//! everything else: what counts as a valid URL, what can be sent and received, and the real network.
//! Only HTTPS to the default port is possible, and nothing reaches an address that isn't public,
//! whatever a name resolves to.

use std::fmt;
use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use habitat_core::backend::{FetchMethod, FetchRequest};
use habitat_core::manifest::is_host_name;

use crate::vault::SecretHeader;
use ureq::Agent;
use ureq::http::{self, Uri};
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

/// The most that a response body can be, which is cut off rather than held in memory.
pub const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
/// The most that a request body can be.
pub const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
/// The most redirects that one request can follow.
pub const MAX_REDIRECTS: u32 = 5;
/// The longest that one hop can take, including reading the whole response.
pub const MAX_HOP_TIME: Duration = Duration::from_secs(120);
const MAX_URL_BYTES: usize = 8 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// A URL that's been checked, which is the only kind that goes any further.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedUrl {
    pub host: String,
    /// Starts with a slash, and can have a query at the end.
    pub path: String,
}

impl fmt::Display for CheckedUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "https://{}{}", self.host, self.path)
    }
}

/// Characters that a path and query can use. Everything else has to be percent-encoded by whoever
/// made the URL. This leaves out anything that's been used to confuse one URL parser with another.
fn is_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-._~:/?[]@!$&'()*+,;=%".contains(&byte)
}

/// Checks a URL from an agent. It has to be exactly `https://host[:443][/path][?query]`, with the
/// host in the form that a manifest uses. Anything else is refused, not corrected.
pub fn check_url(url: &str) -> Result<CheckedUrl, &'static str> {
    if url.len() > MAX_URL_BYTES {
        return Err("the URL is too long");
    }
    let rest = url
        .strip_prefix("https://")
        .ok_or("only https:// URLs are allowed")?;
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(split);
    let host = authority.strip_suffix(":443").unwrap_or(authority);
    if !is_host_name(host) {
        return Err("the URL's host isn't valid");
    }
    if !path.bytes().all(is_path_byte) {
        return Err("the URL holds a character that isn't allowed");
    }
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    Ok(CheckedUrl {
        host: host.to_string(),
        path,
    })
}

/// Works out where a redirect leads. It can only go to another valid HTTPS URL, or a path on the
/// same host. The result is checked again like any other request.
pub fn follow(base: &CheckedUrl, location: &str) -> Result<CheckedUrl, &'static str> {
    if location.starts_with("https://") {
        return check_url(location);
    }
    // These lead to another host, or another scheme.
    if location.starts_with("//") || location.contains("://") {
        return Err("the redirect isn't to an https:// URL");
    }
    let path = if location.starts_with('/') {
        location.to_string()
    } else {
        let without_query = base.path.split('?').next().unwrap_or("/");
        let folder = without_query
            .rfind('/')
            .map_or("/", |end| &without_query[..=end]);
        format!("{folder}{location}")
    };
    check_url(&format!("https://{}{path}", base.host))
}

/// These are for the host or the connection to set. An agent setting them could split a request or
/// send it somewhere other than what was checked.
fn is_forbidden_request_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "connection"
            | "content-length"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
            | "keep-alive"
            | "proxy-authorization"
            | "proxy-connection"
            | "expect"
    ) || name.to_ascii_lowercase().starts_with("proxy-")
}

/// Whether a redirect to another host should lose this header, which could be a credential.
fn is_credential_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "cookie" | "x-api-key" | "api-key"
    )
}

/// These are never passed back to an agent. Some could hold something that the host added, or were
/// only meant for whatever the host is talking to.
pub fn is_hidden_response_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "cookie"
            | "set-cookie"
            | "set-cookie2"
            | "x-api-key"
            | "api-key"
            | "connection"
            | "keep-alive"
            | "transfer-encoding"
    )
}

/// Checks a request's headers and body. The text of the error holds nothing from the agent.
pub fn check_request(request: &FetchRequest) -> Result<(), &'static str> {
    if request
        .body
        .as_ref()
        .is_some_and(|body| body.len() > MAX_REQUEST_BYTES)
    {
        return Err("the request body is too large");
    }
    if request.headers.len() > MAX_HEADERS {
        return Err("the request has too many headers");
    }
    let mut total = 0;
    for (name, value) in &request.headers {
        total += name.len() + value.len();
        if is_forbidden_request_header(name) {
            return Err("the request has a header that only the host can set");
        }
        if http::HeaderName::from_bytes(name.as_bytes()).is_err()
            || http::HeaderValue::from_str(value).is_err()
        {
            return Err("the request has a header that isn't valid");
        }
    }
    if total > MAX_HEADER_BYTES {
        return Err("the request's headers are too large");
    }
    Ok(())
}

/// Takes off anything that shouldn't reach an agent, and limits how much there is.
pub fn clean_response_headers(headers: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut total = 0;
    headers
        .into_iter()
        .filter(|(name, _)| !is_hidden_response_header(name))
        .take(MAX_HEADERS)
        .take_while(|(name, value)| {
            total += name.len() + value.len();
            total <= MAX_HEADER_BYTES
        })
        .collect()
}

/// The request for the next hop after a redirect, as browsers do it. Going to another host loses
/// anything that could be a credential.
pub fn redirected(mut request: FetchRequest, status: u16, same_host: bool) -> FetchRequest {
    let becomes_get = (301..=303).contains(&status) && request.method != FetchMethod::Head;
    if becomes_get {
        request.method = FetchMethod::Get;
        request.body = None;
        request
            .headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
    }
    if !same_host {
        request
            .headers
            .retain(|(name, _)| !is_credential_header(name));
    }
    request
}

/// Whether an address belongs to the public internet. This refuses anything that's the user's own
/// device or network, anything reserved, and anything with another address embedded in it.
pub fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    let reserved = a == 0                               // "this network"
        || a == 10                                      // private
        || a == 127                                     // this device
        || (a == 100 && (64..128).contains(&b))         // shared, as in carrier-grade NAT
        || (a == 169 && b == 254)                       // link-local, including cloud metadata
        || (a == 172 && (16..32).contains(&b))          // private
        || (a == 192 && b == 0 && c == 0)               // protocol assignments
        || (a == 192 && b == 0 && c == 2)               // documentation
        || (a == 192 && b == 88 && c == 99)             // old 6to4 relay
        || (a == 192 && b == 168)                       // private
        || (a == 198 && (b == 18 || b == 19))           // benchmarking
        || (a == 198 && b == 51 && c == 100)            // documentation
        || (a == 203 && b == 0 && c == 113)             // documentation
        || a >= 224; // multicast, reserved and broadcast
    !reserved
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    let embedded_v4 = |high: u16, low: u16| {
        let [a, b] = high.to_be_bytes();
        let [c, d] = low.to_be_bytes();
        Ipv4Addr::new(a, b, c, d)
    };
    // Addresses that are really IPv4 ones, which count as whatever they hold.
    if segments[..5] == [0, 0, 0, 0, 0] && segments[5] == 0xffff {
        return is_public_v4(embedded_v4(segments[6], segments[7]));
    }
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return is_public_v4(embedded_v4(segments[6], segments[7]));
    }
    let first = segments[0];
    let reserved = segments[..6] == [0, 0, 0, 0, 0, 0]  // ::, ::1, and old IPv4-compatible ones
        || first == 0x0064 && segments[1] == 0xff9b     // the rest of the translation prefix
        || first == 0x0100                              // discard
        || (first & 0xffc0) == 0xfe80                   // link-local
        || (first & 0xffc0) == 0xfec0                   // old site-local
        || (first & 0xfe00) == 0xfc00                   // private
        || (first & 0xff00) == 0xff00                   // multicast
        || first == 0x2002                              // 6to4, which embeds an IPv4 address
        || (first == 0x2001 && segments[1] == 0)        // Teredo, which does too
        || (first == 0x2001 && segments[1] == 0x0db8)   // documentation
        || (first == 0x2001 && segments[1] < 0x0200); // other protocol assignments
    !reserved
}

/// Why the network couldn't complete a request. This holds nothing from the outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkError {
    TimedOut,
    /// The name resolves to something that isn't public, or to nothing at all.
    Blocked,
    /// Anything else, such as a failed connection or a bad certificate.
    Failed,
}

/// A response on its way in, with its body not read yet.
pub struct Incoming {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn Read + Send>,
}

/// Something that can make one HTTPS request, with no redirects. Tests swap in a fake one.
pub trait Network: Send + Sync {
    /// `secrets` are headers that the core is adding, which are kept apart from the request, so that
    /// printing a request can't print them.
    fn send(
        &self,
        url: &CheckedUrl,
        request: &FetchRequest,
        secrets: &[SecretHeader],
        wait: Duration,
    ) -> Result<Incoming, NetworkError>;
}

/// Stops reading at a limit, with an error, so that a response can't be mistaken for a whole one.
pub struct LimitedBody<R = Box<dyn Read + Send>> {
    pub inner: R,
    pub left: u64,
}

impl<R: Read> Read for LimitedBody<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        // One more than the limit, to tell a body of exactly the limit from one that's over it.
        let most = (buffer.len() as u64).min(self.left.saturating_add(1)) as usize;
        let count = self.inner.read(&mut buffer[..most])?;
        if count as u64 > self.left {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        self.left -= count as u64;
        Ok(count)
    }
}

/// Looks up a name. Tests swap in a fake one.
pub trait Lookup: fmt::Debug + Send + Sync + 'static {
    fn lookup(&self, host: &str) -> io::Result<Vec<IpAddr>>;
}

/// The system's own name lookup.
#[derive(Debug, Default)]
pub struct SystemLookup;

impl Lookup for SystemLookup {
    fn lookup(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        Ok((host, 443)
            .to_socket_addrs()?
            .map(|address| address.ip())
            .collect())
    }
}

/// Only gives the client addresses that are public. If a name has any that aren't, none are used,
/// because a name that mixes the two is up to something. The client connects to exactly what this
/// returns, so a name can't change its answer between this check and the connection.
#[derive(Debug)]
struct PublicOnly<L>(L);

impl<L: Lookup> Resolver for PublicOnly<L> {
    fn resolve(
        &self,
        uri: &Uri,
        _config: &ureq::config::Config,
        _timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let host = uri.host().ok_or(ureq::Error::HostNotFound)?;
        let addresses = self.0.lookup(host).map_err(|_| ureq::Error::HostNotFound)?;
        if addresses.is_empty() || !addresses.iter().all(|address| is_public(*address)) {
            return Err(ureq::Error::HostNotFound);
        }
        let mut resolved = self.empty();
        for address in addresses {
            // It has room for a few, which is plenty, and the rest don't matter.
            if resolved.try_push(SocketAddr::new(address, 443)).is_err() {
                break;
            }
        }
        Ok(resolved)
    }
}

/// The real network.
pub struct RealNetwork {
    agent: Agent,
}

impl RealNetwork {
    pub fn new() -> Self {
        Self::with_lookup(SystemLookup)
    }

    fn with_lookup(lookup: impl Lookup) -> Self {
        let config = Agent::config_builder()
            .https_only(true)
            // Redirects are followed by the broker, which checks each one. A proxy from the
            // environment could see or change everything, so it's turned off.
            .max_redirects(0)
            .proxy(None)
            .http_status_as_error(false)
            .max_response_header_size(64 * 1024)
            .build();
        Self {
            agent: Agent::with_parts(config, DefaultConnector::default(), PublicOnly(lookup)),
        }
    }
}

impl Default for RealNetwork {
    fn default() -> Self {
        Self::new()
    }
}

fn to_network_error(error: ureq::Error) -> NetworkError {
    match error {
        ureq::Error::Timeout(_) => NetworkError::TimedOut,
        ureq::Error::HostNotFound => NetworkError::Blocked,
        _ => NetworkError::Failed,
    }
}

impl Network for RealNetwork {
    fn send(
        &self,
        url: &CheckedUrl,
        request: &FetchRequest,
        secrets: &[SecretHeader],
        wait: Duration,
    ) -> Result<Incoming, NetworkError> {
        let method = match request.method {
            FetchMethod::Get => http::Method::GET,
            FetchMethod::Head => http::Method::HEAD,
            FetchMethod::Post => http::Method::POST,
            FetchMethod::Put => http::Method::PUT,
            FetchMethod::Patch => http::Method::PATCH,
            FetchMethod::Delete => http::Method::DELETE,
        };
        let mut builder = http::Request::builder().method(method).uri(url.to_string());
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        for secret in secrets {
            let mut value = http::HeaderValue::from_bytes(secret.value.expose())
                .map_err(|_| NetworkError::Failed)?;
            // This stops `http` from showing it, if anything ever prints the request.
            value.set_sensitive(true);
            builder = builder.header(&secret.name, value);
        }
        let empty = Vec::new();
        let outgoing = builder
            .body(request.body.as_ref().unwrap_or(&empty).as_slice())
            .map_err(|_| NetworkError::Failed)?;
        let agent = self
            .agent
            .configure_request(outgoing)
            .timeout_global(Some(wait))
            .build();
        let response = self.agent.run(agent).map_err(to_network_error)?;
        let (parts, body) = response.into_parts();
        let headers = parts
            .headers
            .iter()
            .filter_map(|(name, value)| Some((name.to_string(), value.to_str().ok()?.to_string())))
            .collect();
        Ok(Incoming {
            status: parts.status.as_u16(),
            headers,
            body: Box::new(
                body.into_with_config()
                    .limit(MAX_RESPONSE_BYTES.saturating_add(1))
                    .reader(),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::process::{Command, Stdio};

    use super::*;

    fn url(text: &str) -> CheckedUrl {
        check_url(text).unwrap()
    }

    #[test]
    fn good_urls_are_read_as_they_are() {
        for (text, host, path) in [
            ("https://api.github.com", "api.github.com", "/"),
            ("https://api.github.com/", "api.github.com", "/"),
            ("https://api.github.com:443/x", "api.github.com", "/x"),
            (
                "https://a.example/x/y?q=1&r=%20",
                "a.example",
                "/x/y?q=1&r=%20",
            ),
            ("https://a.example?q=1", "a.example", "/?q=1"),
        ] {
            let checked = check_url(text).unwrap();
            assert_eq!(checked.host, host, "{text}");
            assert_eq!(checked.path, path, "{text}");
        }
    }

    #[test]
    fn anything_else_is_refused() {
        let long = format!("https://a.example/{}", "x".repeat(MAX_URL_BYTES));
        for text in [
            "",
            "a.example",
            "http://a.example/",
            "HTTPS://a.example/",
            "ftp://a.example/",
            "file:///etc/passwd",
            "//a.example/",
            " https://a.example/",
            "https://",
            "https:///path",
            "https://A.EXAMPLE/",
            "https://a.example:8443/",
            "https://a.example:80/",
            "https://a.example:/",
            "https://a.example:443:443/",
            "https://user@a.example/",
            "https://user:pass@a.example/",
            "https://a.example@evil.example/",
            "https://evil.example\\@a.example/",
            "https://evil.example#@a.example/",
            "https://a.example/#fragment",
            "https://127.0.0.1/",
            "https://[::1]/",
            "https://0x7f.0.0.1/",
            "https://2130706433/",
            "https://localhost/",
            "https://a.example./",
            "https://a.example/ path",
            "https://a.example/\r\nHost: evil.example",
            "https://a.example/\\..\\",
            "https://a.example/é",
            "https://а.example/",
            long.as_str(),
        ] {
            assert!(check_url(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn redirects_are_worked_out_and_checked() {
        let base = url("https://a.example/one/two?q=1");
        for (location, expected) in [
            ("https://b.example/x", "https://b.example/x"),
            ("/x", "https://a.example/x"),
            ("x", "https://a.example/one/x"),
            ("x?y=1", "https://a.example/one/x?y=1"),
        ] {
            assert_eq!(follow(&base, location).unwrap().to_string(), expected);
        }
        for location in [
            "http://a.example/",
            "//evil.example/",
            "file:///etc/passwd",
            "gopher://a.example/",
            "https://127.0.0.1/",
            "https://user@evil.example/",
            "/a b",
            "/x\r\nHost: evil.example",
            "",
        ] {
            let result = follow(&base, location);
            // An empty one stays where it is, which is at least still somewhere checked.
            if location.is_empty() {
                assert_eq!(result.unwrap().host, "a.example");
            } else {
                assert!(result.is_err(), "{location:?}");
            }
        }
    }

    #[test]
    fn private_and_reserved_addresses_arent_public() {
        for text in [
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.1",
            "10.255.255.255",
            "100.64.0.1",
            "100.127.255.255",
            "127.0.0.1",
            "127.255.255.255",
            "169.254.169.254",
            "169.254.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.0.1",
            "192.168.255.255",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "239.255.255.255",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::a00:1",
            "64:ff9b:1::1",
            "100::1",
            "fe80::1",
            "febf::1",
            "fec0::1",
            "fc00::1",
            "fd12:3456::1",
            "ff02::1",
            "2001:db8::1",
            "2001::1",
            "2001:1::1",
            "2002:7f00:1::1",
            "2002:a00:1::1",
        ] {
            let address: IpAddr = text.parse().unwrap();
            assert!(!is_public(address), "{text}");
        }
    }

    #[test]
    fn ordinary_addresses_are_public() {
        for text in [
            "1.1.1.1",
            "8.8.8.8",
            "9.255.255.255",
            "11.0.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "126.255.255.255",
            "128.0.0.1",
            "169.253.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "192.0.1.1",
            "192.167.0.1",
            "192.169.0.1",
            "198.17.0.1",
            "198.20.0.1",
            "223.255.255.255",
            "2606:4700:4700::1111",
            "2a00:1450:4001::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            let address: IpAddr = text.parse().unwrap();
            assert!(is_public(address), "{text}");
        }
    }

    fn get(headers: &[(&str, &str)]) -> FetchRequest {
        FetchRequest {
            method: FetchMethod::Get,
            url: String::new(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            body: None,
        }
    }

    #[test]
    fn headers_that_could_change_where_a_request_goes_are_refused() {
        for name in [
            "Host",
            "host",
            "Connection",
            "Content-Length",
            "Transfer-Encoding",
            "TE",
            "Upgrade",
            "Proxy-Authorization",
            "Proxy-Anything",
            "Expect",
        ] {
            assert!(check_request(&get(&[(name, "x")])).is_err(), "{name}");
        }
        for (name, value) in [
            ("Bad Name", "x"),
            ("Bad:Name", "x"),
            ("", "x"),
            ("X-Split", "a\r\nHost: evil.example"),
            ("X-Split", "a\nb"),
            ("X-Null", "a\0b"),
        ] {
            assert!(
                check_request(&get(&[(name, value)])).is_err(),
                "{name:?} {value:?}"
            );
        }
        assert!(check_request(&get(&[("Accept", "text/plain"), ("X-Custom", "1")])).is_ok());
    }

    #[test]
    fn requests_are_limited_in_size() {
        let many: Vec<(String, String)> = (0..=MAX_HEADERS)
            .map(|index| (format!("X-{index}"), "x".to_string()))
            .collect();
        let mut request = get(&[]);
        request.headers = many;
        assert!(check_request(&request).is_err());

        let mut request = get(&[("X-Big", &"x".repeat(MAX_HEADER_BYTES + 1))]);
        assert!(check_request(&request).is_err());

        request.headers.clear();
        request.body = Some(vec![0; MAX_REQUEST_BYTES]);
        assert!(check_request(&request).is_ok());
        request.body = Some(vec![0; MAX_REQUEST_BYTES + 1]);
        assert!(check_request(&request).is_err());
    }

    #[test]
    fn sensitive_response_headers_dont_reach_the_agent() {
        let headers: Vec<(String, String)> = [
            ("Content-Type", "text/plain"),
            ("Set-Cookie", "session=secret"),
            ("set-cookie2", "secret"),
            ("Authorization", "Bearer secret"),
            ("x-api-key", "secret"),
            ("X-Custom", "fine"),
        ]
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
        let clean = clean_response_headers(headers);
        assert_eq!(
            clean,
            [
                ("Content-Type".to_string(), "text/plain".to_string()),
                ("X-Custom".to_string(), "fine".to_string()),
            ]
        );
    }

    #[test]
    fn a_flood_of_response_headers_is_limited() {
        let many: Vec<(String, String)> = (0..1000)
            .map(|index| (format!("X-{index}"), "x".repeat(1000)))
            .collect();
        let clean = clean_response_headers(many);
        assert!(clean.len() <= MAX_HEADERS);
        let total: usize = clean
            .iter()
            .map(|(name, value)| name.len() + value.len())
            .sum();
        assert!(total <= MAX_HEADER_BYTES);
    }

    #[test]
    fn a_redirect_to_another_host_loses_credentials() {
        let request = get(&[
            ("Authorization", "secret"),
            ("Cookie", "secret"),
            ("X-Api-Key", "secret"),
            ("Accept", "*/*"),
        ]);
        let kept = redirected(request.clone(), 307, true);
        assert_eq!(kept.headers.len(), 4);
        let lost = redirected(request, 307, false);
        assert_eq!(lost.headers, [("Accept".to_string(), "*/*".to_string())]);
    }

    #[test]
    fn a_redirect_changes_the_method_as_browsers_do() {
        let mut post = get(&[("Content-Type", "text/plain"), ("Accept", "*/*")]);
        post.method = FetchMethod::Post;
        post.body = Some(b"data".to_vec());
        for status in [301, 302, 303] {
            let next = redirected(post.clone(), status, true);
            assert_eq!(next.method, FetchMethod::Get, "{status}");
            assert_eq!(next.body, None, "{status}");
            assert_eq!(next.headers, [("Accept".to_string(), "*/*".to_string())]);
        }
        for status in [307, 308] {
            let next = redirected(post.clone(), status, true);
            assert_eq!(next.method, FetchMethod::Post, "{status}");
            assert_eq!(next.body.as_deref(), Some(&b"data"[..]));
        }
        let mut head = get(&[]);
        head.method = FetchMethod::Head;
        assert_eq!(redirected(head, 302, true).method, FetchMethod::Head);
    }

    #[test]
    fn a_body_is_cut_off_at_its_limit_with_an_error() {
        let read_all = |size: usize, limit: u64| {
            let mut body = LimitedBody {
                inner: Box::new(io::Cursor::new(vec![7; size])),
                left: limit,
            };
            let mut data = Vec::new();
            body.read_to_end(&mut data).map(|_| data.len())
        };
        assert_eq!(read_all(0, 10).unwrap(), 0);
        assert_eq!(read_all(9, 10).unwrap(), 9);
        assert_eq!(read_all(10, 10).unwrap(), 10);
        assert_eq!(
            read_all(11, 10).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
        assert_eq!(
            read_all(1_000_000, 10).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
    }

    #[derive(Debug)]
    struct Answers(Vec<IpAddr>);

    impl Lookup for Answers {
        fn lookup(&self, _: &str) -> io::Result<Vec<IpAddr>> {
            Ok(self.0.clone())
        }
    }

    /// Runs a real request through the real client, for a name that resolves to these addresses,
    /// and checks that nothing ever connects to a listener on this device.
    fn fetch_resolving_to(addresses: Vec<IpAddr>) -> Result<Incoming, NetworkError> {
        let network = RealNetwork::with_lookup(Answers(addresses));
        network.send(
            &url("https://rebinding.example/"),
            &get(&[]),
            &[],
            Duration::from_secs(5),
        )
    }

    #[test]
    fn a_name_that_resolves_to_a_private_address_is_blocked_before_any_connection() {
        // A real listener on 443 needs special rights, so this checks the path before connecting:
        // the error is the resolver's own, and the client never gets an address.
        for addresses in [
            vec!["127.0.0.1".parse().unwrap()],
            vec!["::1".parse().unwrap()],
            vec!["10.0.0.1".parse().unwrap()],
            vec!["169.254.169.254".parse().unwrap()],
            vec!["::ffff:127.0.0.1".parse().unwrap()],
            vec!["8.8.8.8".parse().unwrap(), "127.0.0.1".parse().unwrap()],
            vec![],
        ] {
            let result = fetch_resolving_to(addresses.clone());
            assert!(
                matches!(result, Err(NetworkError::Blocked)),
                "{addresses:?} gave {:?}",
                result.err()
            );
        }
    }

    const PROXY_VAR: &str = "HABITAT_TEST_PROXY_CHECK";

    /// Runs in a process of its own with a proxy set, as changing the environment isn't safe in a
    /// test that shares a process with others.
    #[test]
    #[ignore = "run by the_real_client_doesnt_use_a_proxy_from_the_environment"]
    fn proxy_check() {
        assert!(
            env::var(PROXY_VAR).is_ok(),
            "only for the test that runs this"
        );
        let network = RealNetwork::new();
        assert!(network.agent.config().proxy().is_none());
    }

    #[test]
    fn the_real_client_doesnt_use_a_proxy_from_the_environment() {
        for variable in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
            let status = Command::new(env::current_exe().unwrap())
                .args(["--exact", "gateway::tests::proxy_check", "--ignored"])
                .env(PROXY_VAR, "1")
                .env(variable, "http://127.0.0.1:9")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "{variable}");
        }
    }

    #[test]
    fn the_real_client_only_does_https_and_follows_no_redirects() {
        let network = RealNetwork::new();
        assert_eq!(network.agent.config().max_redirects(), 0);
        assert!(network.agent.config().https_only());
    }
}
