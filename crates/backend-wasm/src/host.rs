//! The host's side of the agent API.
//!
//! Everything here takes untrusted input from the agent. Calls that need the broker are denied
//! until the broker exists.

use std::time::{SystemTime, UNIX_EPOCH};

use habitat_core::home::{EntryKind as HomeEntryKind, Home, HomeError as StoreError};
use wasmtime::component::Resource;
use wasmtime::{ResourceLimiter, Result};

use crate::bindings::habitat::host::agent_home;
use crate::bindings::habitat::host::types::{
    self, AccessTarget, ByteStream, DirEntry, EntryKind, Grant, HomeError, HostByteStream,
    HostModelStream, HostUserFile, HttpRequest, HttpResponse, LogLevel, ModelRequest, ModelStream,
    TokenUsage, UserFile,
};

/// The most that one read returns. Agents read larger files in pieces.
const MAX_READ_BYTES: u32 = 1024 * 1024;
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

/// What one run of an agent can reach.
pub(crate) struct HostState {
    home: Home,
    limiter: Limiter,
}

impl HostState {
    pub(crate) fn new(home: Home, memory_bytes: u64) -> Self {
        Self {
            home,
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

    fn http_fetch(&mut self, _request: HttpRequest) -> Result<HttpResponse, HomeError> {
        needs_broker()
    }

    fn request_access(
        &mut self,
        _target: AccessTarget,
        _reason: String,
    ) -> Result<Grant, HomeError> {
        needs_broker()
    }

    fn pick_user_file(&mut self, _reason: String) -> Result<Resource<UserFile>, HomeError> {
        needs_broker()
    }
}

impl types::Host for HostState {}

// Nothing can create these resources yet, so an agent can never hold one to call these with.

impl HostByteStream for HostState {
    fn read(
        &mut self,
        _stream: Resource<ByteStream>,
        _max_bytes: u32,
    ) -> Result<Option<Vec<u8>>, HomeError> {
        needs_broker()
    }

    fn drop(&mut self, _stream: Resource<ByteStream>) -> Result<()> {
        Ok(())
    }
}

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
