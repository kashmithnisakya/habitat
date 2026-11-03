//! The host's side of the agent API.
//!
//! Everything here takes untrusted input from the agent. Calls that need the broker are denied
//! until the broker exists.

use std::io::{self, Read};
use std::time::{SystemTime, UNIX_EPOCH};

use habitat_core::backend::{
    AccessRefused, FetchError, FetchMethod, FetchRequest, Lifetime, RunBroker, Target,
};
use habitat_core::home::{EntryKind as HomeEntryKind, Home, HomeError as StoreError};
use wasmtime::component::{Resource, ResourceTable};
use wasmtime::{ResourceLimiter, Result};

use crate::bindings::habitat::host::agent_home;
use crate::bindings::habitat::host::types::{
    self, AccessTarget, DirEntry, EntryKind, Grant, GrantLifetime, Header, HomeError,
    HostByteStream, HostModelStream, HostUserFile, HttpMethod, HttpRequest, HttpResponse, LogLevel,
    ModelRequest, ModelStream, TokenUsage, UserFile,
};

/// The most that one read returns. Agents read larger files in pieces.
const MAX_READ_BYTES: u32 = 1024 * 1024;
/// How many response bodies one run can have open at once, since each can hold a connection.
const MAX_OPEN_BODIES: usize = 16;
/// The most random bytes that one call returns.
const MAX_RANDOM_BYTES: u32 = 1024;

/// How much one element of a table counts for, since tables use the host's memory too.
const TABLE_ELEMENT_BYTES: u64 = 8;
// How many of each one run can create. Each uses some of the host's memory beyond what's counted.
const MAX_INSTANCES: usize = 100;
const MAX_TABLES: usize = 100;
const MAX_MEMORIES: usize = 100;

/// Keeps a run within its memory limit.
///
/// Wasmtime's own `StoreLimits` limits each memory separately, so an agent could get around it by
/// creating many memories. This counts all of a run's memories and tables together.
pub(crate) struct Limiter {
    limit: u64,
    used: u64,
    hit_limit: bool,
}

impl Limiter {
    fn grow(&mut self, by: u64) -> bool {
        match self.used.checked_add(by) {
            Some(used) if used <= self.limit => {
                self.used = used;
                true
            }
            _ => {
                self.hit_limit = true;
                false
            }
        }
    }
}

impl ResourceLimiter for Limiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool> {
        Ok(self.grow(desired.saturating_sub(current) as u64))
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool> {
        let elements = desired.saturating_sub(current) as u64;
        Ok(self.grow(elements.saturating_mul(TABLE_ELEMENT_BYTES)))
    }

    fn instances(&self) -> usize {
        MAX_INSTANCES
    }

    fn tables(&self) -> usize {
        MAX_TABLES
    }

    fn memories(&self) -> usize {
        MAX_MEMORIES
    }
}

/// A response body that an agent is reading, and can stop reading by dropping it.
pub struct OpenBody(Box<dyn Read + Send>);

/// What one run of an agent can reach.
pub(crate) struct HostState {
    home: Home,
    limiter: Limiter,
    broker: Box<dyn RunBroker + Send>,
    table: ResourceTable,
    open_bodies: usize,
}

impl HostState {
    pub(crate) fn new(home: Home, memory_bytes: u64, broker: Box<dyn RunBroker + Send>) -> Self {
        Self {
            home,
            broker,
            table: ResourceTable::new(),
            open_bodies: 0,
            limiter: Limiter {
                limit: memory_bytes,
                used: 0,
                hit_limit: false,
            },
        }
    }

    pub(crate) fn limiter(&mut self) -> &mut dyn ResourceLimiter {
        &mut self.limiter
    }

    /// Whether the agent asked for more memory than its limit allows at any point in this run.
    pub(crate) fn hit_memory_limit(&self) -> bool {
        self.limiter.hit_limit
    }
}

/// Converts a home store error. Its messages hold no host paths.
fn from_store(error: StoreError) -> HomeError {
    match error {
        StoreError::InvalidPath(reason) => HomeError::InvalidRequest(reason.to_string()),
        StoreError::NotFound => HomeError::NotFound,
        StoreError::Denied => HomeError::Denied(error.to_string()),
        StoreError::NotAFile => HomeError::InvalidRequest(error.to_string()),
        StoreError::DiskLimit { .. } | StoreError::TooManyEntries { .. } => {
            HomeError::LimitExceeded(error.to_string())
        }
        StoreError::Io(_) => HomeError::Internal,
    }
}

fn from_fetch(error: FetchError) -> HomeError {
    match error {
        FetchError::NotGranted => HomeError::NotGranted,
        FetchError::Denied(why) => HomeError::Denied(why),
        FetchError::InvalidRequest(why) => HomeError::InvalidRequest(why),
        FetchError::LimitExceeded(why) => HomeError::LimitExceeded(why),
        FetchError::TimedOut => HomeError::TimedOut,
        FetchError::Unavailable(why) => HomeError::Unavailable(why),
        FetchError::Internal => HomeError::Internal,
    }
}

fn needs_broker<T>() -> Result<T, HomeError> {
    Err(HomeError::Denied("this isn't available yet".to_string()))
}

impl agent_home::Host for HostState {
    fn read_file(
        &mut self,
        path: String,
        offset: u64,
        max_bytes: u32,
    ) -> Result<Vec<u8>, HomeError> {
        self.home
            .read_at(&path, offset, max_bytes.min(MAX_READ_BYTES) as usize)
            .map_err(from_store)
    }

    fn write_file(&mut self, path: String, data: Vec<u8>) -> Result<(), HomeError> {
        self.home.write(&path, &data).map_err(from_store)
    }

    fn remove_file(&mut self, path: String) -> Result<(), HomeError> {
        self.home.remove(&path).map_err(from_store)
    }

    fn list_dir(&mut self, path: String) -> Result<Vec<DirEntry>, HomeError> {
        let entries = self.home.list(&path).map_err(from_store)?;
        Ok(entries
            .into_iter()
            .map(|entry| DirEntry {
                name: entry.name,
                kind: match entry.kind {
                    HomeEntryKind::File => EntryKind::File,
                    HomeEntryKind::Directory => EntryKind::Directory,
                },
                size: entry.size,
            })
            .collect())
    }

    fn now_ms(&mut self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64)
    }

    fn random_bytes(&mut self, count: u32) -> Result<Vec<u8>, HomeError> {
        if count > MAX_RANDOM_BYTES {
            return Err(HomeError::LimitExceeded(format!(
                "the limit is {MAX_RANDOM_BYTES} bytes"
            )));
        }
        let mut bytes = vec![0; count as usize];
        getrandom::fill(&mut bytes).map_err(|_| HomeError::Internal)?;
        Ok(bytes)
    }

    fn send_message(&mut self, _text: String) -> Result<(), HomeError> {
        needs_broker()
    }

    fn log(&mut self, _level: LogLevel, _message: String) {}

    fn model_complete(
        &mut self,
        _request: ModelRequest,
    ) -> Result<Resource<ModelStream>, HomeError> {
        needs_broker()
    }

    fn http_fetch(&mut self, request: HttpRequest) -> Result<HttpResponse, HomeError> {
        if self.open_bodies >= MAX_OPEN_BODIES {
            return Err(HomeError::LimitExceeded(format!(
                "the limit is {MAX_OPEN_BODIES} response bodies at once"
            )));
        }
        let response = self
            .broker
            .http_fetch(FetchRequest {
                method: match request.method {
                    HttpMethod::Get => FetchMethod::Get,
                    HttpMethod::Head => FetchMethod::Head,
                    HttpMethod::Post => FetchMethod::Post,
                    HttpMethod::Put => FetchMethod::Put,
                    HttpMethod::Patch => FetchMethod::Patch,
                    HttpMethod::Delete => FetchMethod::Delete,
                },
                url: request.url,
                headers: request
                    .headers
                    .into_iter()
                    .map(|header| (header.name, header.value))
                    .collect(),
                body: request.body,
            })
            .map_err(from_fetch)?;
        let body = self
            .table
            .push(OpenBody(response.body))
            .map_err(|_| HomeError::Internal)?;
        self.open_bodies += 1;
        Ok(HttpResponse {
            status: response.status,
            headers: response
                .headers
                .into_iter()
                .map(|(name, value)| Header { name, value })
                .collect(),
            body,
        })
    }

    fn request_access(&mut self, target: AccessTarget, reason: String) -> Result<Grant, HomeError> {
        let asked_for = match &target {
            AccessTarget::NetworkHost(host) => Target::NetworkHost(host.clone()),
            AccessTarget::Model(model) => Target::Model(model.clone()),
        };
        match self.broker.request_access(&asked_for, &reason) {
            Ok(lifetime) => Ok(Grant {
                target,
                lifetime: match lifetime {
                    Lifetime::Once => GrantLifetime::Once,
                    Lifetime::ThisRun => GrantLifetime::ThisRun,
                    Lifetime::UntilRevoked => GrantLifetime::UntilRevoked,
                },
            }),
            Err(AccessRefused::Denied(why)) => Err(HomeError::Denied(why)),
            Err(AccessRefused::LimitExceeded(why)) => Err(HomeError::LimitExceeded(why)),
            Err(AccessRefused::Internal) => Err(HomeError::Internal),
        }
    }

    fn pick_user_file(&mut self, _reason: String) -> Result<Resource<UserFile>, HomeError> {
        needs_broker()
    }
}

impl types::Host for HostState {}

impl HostByteStream for HostState {
    fn read(
        &mut self,
        stream: Resource<OpenBody>,
        max_bytes: u32,
    ) -> Result<Option<Vec<u8>>, HomeError> {
        let body = self
            .table
            .get_mut(&stream)
            .map_err(|_| HomeError::Internal)?;
        let mut data = vec![0; max_bytes.min(MAX_READ_BYTES) as usize];
        // Asking for nothing can't tell us whether it's the end, so that's what it gets.
        if data.is_empty() {
            return Ok(Some(data));
        }
        let count = body.0.read(&mut data).map_err(|error| match error.kind() {
            io::ErrorKind::FileTooLarge => {
                HomeError::LimitExceeded("the response is too large".to_string())
            }
            io::ErrorKind::TimedOut => HomeError::TimedOut,
            _ => HomeError::Unavailable("reading the response failed".to_string()),
        })?;
        if count == 0 {
            return Ok(None);
        }
        data.truncate(count);
        Ok(Some(data))
    }

    fn drop(&mut self, stream: Resource<OpenBody>) -> Result<()> {
        // Closing it cancels whatever's left of the download.
        self.table.delete(stream)?;
        self.open_bodies = self.open_bodies.saturating_sub(1);
        Ok(())
    }
}

// Nothing can create these resources yet, so an agent can never hold one to call these with.

impl HostModelStream for HostState {
    fn next(&mut self, _stream: Resource<ModelStream>) -> Result<Option<String>, HomeError> {
        needs_broker()
    }

    fn usage(&mut self, _stream: Resource<ModelStream>) -> Option<TokenUsage> {
        None
    }

    fn drop(&mut self, _stream: Resource<ModelStream>) -> Result<()> {
        Ok(())
    }
}

impl HostUserFile for HostState {
    fn name(&mut self, _file: Resource<UserFile>) -> String {
        String::new()
    }

    fn size(&mut self, _file: Resource<UserFile>) -> u64 {
        0
    }

    fn read(
        &mut self,
        _file: Resource<UserFile>,
        _offset: u64,
        _max_bytes: u32,
    ) -> Result<Vec<u8>, HomeError> {
        needs_broker()
    }

    fn drop(&mut self, _file: Resource<UserFile>) -> Result<()> {
        Ok(())
    }
}
