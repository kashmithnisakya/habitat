//! Each agent's storage on disk, and the home store that gives an agent its own files.
//!
//! Paths from an agent are untrusted. They're checked here, and then every file operation walks
//! down from an open directory, one name at a time, without following symlinks. Swapping a
//! directory for a symlink during an operation therefore can't lead outside the home.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, Metadata, OpenOptions};

use crate::manifest::is_agent_name;
use crate::platform;

const AGENTS_DIR: &str = "agents";
const PACKAGE_DIR: &str = "package";
const HOME_DIR: &str = "home";
/// Outside the home, so an agent can never see a partly written file.
const TEMP_DIR: &str = "tmp";
const GRANTS_FILE: &str = "grants.db";
const AUDIT_FILE: &str = "audit.db";
const RUNS_DIR: &str = "runs";
/// Outside each agent's folder, because Windows can't move a folder with an open file in it.
const LOCKS_DIR: &str = "locks";
/// Where an agent's folder goes while it's being deleted.
const REMOVED_DIR: &str = "removed";
const PACKAGE_HASH_FILE: &str = "package.sha256";

const MAX_PATH_BYTES: usize = 1024;
const MAX_NAME_BYTES: usize = 255;
const MAX_PATH_DEPTH: usize = 32;
/// The most entries that one directory listing returns. It keeps a listing from using a lot of memory.
const MAX_LIST_ENTRIES: usize = 10_000;
/// Far more than the most that a valid manifest's requests could need saving.
const MAX_GRANTS_BYTES: u64 = 1024 * 1024;
/// Counted for every file and directory, so that empty files can't fill the disk.
const ENTRY_COST: u64 = 4096;

/// Why a home operation failed. Messages never include host paths, so they're safe to show to
/// the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeError {
    /// The agent's name or path breaks the rules. The path may not even have been used.
    InvalidPath(&'static str),
    /// Nothing exists at this path.
    NotFound,
    /// The path leads to something the home store refuses to touch, such as a symlink.
    Denied,
    /// The path leads to a directory or passes through a file.
    NotAFile,
    /// This would take the home over its disk limit.
    DiskLimit { needed: u64, limit: u64 },
    /// A directory has more entries than can be listed at once.
    TooManyEntries { limit: usize },
    /// The host's file system failed.
    Io(io::ErrorKind),
}

impl std::fmt::Display for HomeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPath(reason) => write!(f, "invalid path: {reason}"),
            Self::NotFound => write!(f, "file not found"),
            Self::Denied => write!(f, "access denied"),
            Self::NotAFile => write!(f, "not a file"),
            Self::DiskLimit { needed, limit } => {
                write!(f, "disk limit exceeded: needs {needed} bytes of {limit}")
            }
            Self::TooManyEntries { limit } => {
                write!(f, "too many entries to list: the limit is {limit}")
            }
            Self::Io(kind) => write!(f, "file system error: {kind}"),
        }
    }
}

impl std::error::Error for HomeError {}

impl From<io::Error> for HomeError {
    fn from(error: io::Error) -> Self {
        match (error.kind(), error.raw_os_error()) {
            (io::ErrorKind::NotFound, _) => Self::NotFound,
            (io::ErrorKind::PermissionDenied, _) => Self::Denied,
            _ if platform::is_link_refused(&error) => Self::Denied,
            (io::ErrorKind::NotADirectory | io::ErrorKind::IsADirectory, _) => Self::NotAFile,
            (kind, _) => Self::Io(kind),
        }
    }
}

/// One agent's folder. Only `home/` is ever visible to the agent.
#[derive(Debug)]
pub struct AgentStorage {
    name: String,
    path: PathBuf,
}

impl AgentStorage {
    /// Opens an agent's folder under `root`, creating it if needed. `root` is chosen by the host
    /// and is trusted. Only the current user can open the folder.
    pub fn open(root: &Path, agent_name: &str) -> Result<Self, HomeError> {
        if !is_agent_name(agent_name) {
            return Err(HomeError::InvalidPath("not a valid agent name"));
        }
        let path = root.join(AGENTS_DIR).join(agent_name);
        platform::create_private_dirs(&path)?;
        platform::create_private_dirs(&root.join(LOCKS_DIR))?;
        for dir in [PACKAGE_DIR, HOME_DIR, TEMP_DIR, RUNS_DIR] {
            platform::create_private_dirs(&path.join(dir))?;
        }
        Ok(Self {
            name: agent_name.to_string(),
            path,
        })
    }

    /// Opens an agent's folder under `root`, but only if it's already there.
    pub fn open_existing(root: &Path, agent_name: &str) -> Result<Self, HomeError> {
        if !is_agent_name(agent_name) {
            return Err(HomeError::InvalidPath("not a valid agent name"));
        }
        if !root.join(AGENTS_DIR).join(agent_name).is_dir() {
            return Err(HomeError::NotFound);
        }
        Self::open(root, agent_name)
    }

    /// The names of all the agents with a folder under `root`, sorted. Anything else there is left
    /// out, including anything that couldn't have been made by [`AgentStorage::open`].
    pub(crate) fn names(root: &Path) -> io::Result<Vec<String>> {
        let entries = match fs::read_dir(root.join(AGENTS_DIR)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            other => other?,
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str()
                && is_agent_name(name)
                && entry.path().is_dir()
            {
                names.push(name.to_string());
            }
        }
        names.sort_unstable();
        Ok(names)
    }

    /// Deletes this agent's folder and everything in it, without following any symlinks.
    ///
    /// The folder is moved away first, so the agent is gone in one step. If the host dies part way
    /// through, a later install can't inherit what's left of the old agent's home. What's left is
    /// cleared out the next time an agent is removed.
    pub(crate) fn remove(self) -> io::Result<()> {
        let agents = self.path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        let root = agents.parent().ok_or(io::ErrorKind::InvalidInput)?;
        let removed = root.join(REMOVED_DIR);
        platform::create_private_dirs(&removed)?;
        // Each is deleted by whatever moved it there, so this only matters after a crash. Another
        // process could be deleting one at the same time, so errors here don't matter either.
        for entry in fs::read_dir(&removed)?.flatten() {
            let _ = fs::remove_dir_all(entry.path());
        }

        let mut random = [0; 8];
        getrandom::fill(&mut random).map_err(|_| io::Error::from(io::ErrorKind::Other))?;
        let id: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let target = removed.join(format!("{}-{id}", self.name));
        fs::rename(&self.path, &target)?;
        platform::sync_dir_path(agents)?;
        platform::sync_dir_path(&removed)?;
        fs::remove_dir_all(target)
    }

    /// The name of the agent this folder belongs to.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Opens the agent's home, with the given limit on the space it can use.
    pub fn home(&self, disk_limit: u64) -> Result<Home, HomeError> {
        let open = |name| Dir::open_ambient_dir(self.path.join(name), ambient_authority());
        Home::new(open(HOME_DIR)?, open(TEMP_DIR)?, disk_limit)
    }

    /// The read-only agent code and manifest.
    pub fn package_path(&self) -> PathBuf {
        self.path.join(PACKAGE_DIR)
    }

    /// The record of the installed package's hash, which only the core can write to. It's kept
    /// outside the package folder, so that a change to the package can't also change the record.
    pub fn package_hash_path(&self) -> PathBuf {
        self.path.join(PACKAGE_HASH_FILE)
    }

    /// The folder of records of the agent's runs, which only the core can write to.
    pub(crate) fn runs_dir(&self) -> io::Result<Dir> {
        Dir::open_ambient_dir(self.path.join(RUNS_DIR), ambient_authority())
    }

    /// The file that's locked for as long as the agent is running. It stays when the agent is
    /// removed, so that the lock means the same thing for a later agent with the same name.
    pub(crate) fn run_lock_path(&self) -> PathBuf {
        self.locks_dir().join(format!("{}.lock", self.name))
    }

    fn locks_dir(&self) -> PathBuf {
        let agents = self.path.parent().unwrap_or(&self.path);
        let root = agents.parent().unwrap_or(agents);
        root.join(LOCKS_DIR)
    }

    /// The agent's permissions, which only the core can write to.
    pub fn grants_path(&self) -> PathBuf {
        self.path.join(GRANTS_FILE)
    }

    /// Reads the agent's saved permissions, if there are any. A link is refused.
    pub fn read_grants(&self) -> io::Result<Option<Vec<u8>>> {
        let file = match platform::open_no_follow(&self.grants_path()) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            other => other?,
        };
        if !file.metadata()?.is_file() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut data = Vec::new();
        file.take(MAX_GRANTS_BYTES + 1).read_to_end(&mut data)?;
        if data.len() as u64 > MAX_GRANTS_BYTES {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(Some(data))
    }

    /// Changes the agent's saved permissions, one change at a time, even across processes. `change`
    /// gets what's saved, and returns what to save instead. A crash leaves one or the other.
    pub fn update_grants<E: From<io::Error>>(
        &self,
        change: impl FnOnce(Option<Vec<u8>>) -> Result<Vec<u8>, E>,
    ) -> Result<(), E> {
        let lock = platform::open_or_create_no_follow(
            &self.locks_dir().join(format!("{}.grants.lock", self.name)),
        )?;
        if !lock.metadata()?.is_file() {
            return Err(io::Error::from(io::ErrorKind::InvalidData).into());
        }
        // Only ever held for a moment, and the system lets go of it if this process dies.
        lock.lock()?;

        let data = change(self.read_grants()?)?;
        if data.len() as u64 > MAX_GRANTS_BYTES {
            return Err(io::Error::from(io::ErrorKind::InvalidData).into());
        }
        let path = self.grants_path();
        let temp = path.with_extension("new");
        // Left by a crash, and it was never in use.
        if temp.symlink_metadata().is_ok() {
            fs::remove_file(&temp)?;
        }
        platform::write_new_file(&temp, &data, false)?;
        fs::rename(&temp, &path)?;
        platform::sync_dir_path(&self.path)?;
        Ok(())
    }

    /// The agent's tamper-evident log, which only the core can write to.
    pub fn audit_path(&self) -> PathBuf {
        self.path.join(AUDIT_FILE)
    }
}

/// What kind of thing a [`HomeEntry`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

/// One item in a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeEntry {
    pub name: String,
    pub kind: EntryKind,
    /// The file's size in bytes, which is always 0 for a directory.
    pub size: u64,
}

/// An agent's files. Agents can only name things inside the home.
///
/// Only one `Home` should be open for an agent at a time, because each keeps its own count of the
/// space used. The supervisor is responsible for that.
#[derive(Debug)]
pub struct Home {
    root: Dir,
    temp: Dir,
    disk_limit: u64,
    used: u64,
    next_temp: u64,
}

impl Home {
    fn new(root: Dir, temp: Dir, disk_limit: u64) -> Result<Self, HomeError> {
        // Anything here was left by a crash, and was never part of the agent's files.
        for entry in temp.entries()? {
            temp.remove_file(entry?.file_name())?;
        }
        let used = count_used(&root)?;
        Ok(Self {
            root,
            temp,
            disk_limit,
            used,
            next_temp: 0,
        })
    }

    /// The space the home is using, including a fixed amount for each file and directory.
    pub fn used_bytes(&self) -> u64 {
        self.used
    }

    /// Reads a whole file.
    pub fn read(&self, path: &str) -> Result<Vec<u8>, HomeError> {
        let (mut file, metadata) = self.open_file(path)?;
        if metadata.len() > self.disk_limit {
            return Err(HomeError::DiskLimit {
                needed: metadata.len(),
                limit: self.disk_limit,
            });
        }

        // Bounded, in case something outside Habitat makes the file grow during this read.
        let mut data = Vec::with_capacity(metadata.len() as usize);
        Read::take(&mut file, self.disk_limit.saturating_add(1)).read_to_end(&mut data)?;
        if data.len() as u64 > self.disk_limit {
            return Err(HomeError::DiskLimit {
                needed: data.len() as u64,
                limit: self.disk_limit,
            });
        }
        Ok(data)
    }

    /// Reads up to `max_bytes` from `offset`. Returns nothing at or past the end of the file.
    pub fn read_at(&self, path: &str, offset: u64, max_bytes: usize) -> Result<Vec<u8>, HomeError> {
        let (mut file, metadata) = self.open_file(path)?;
        if offset >= metadata.len() {
            return Ok(Vec::new());
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut data = Vec::new();
        Read::take(&mut file, max_bytes as u64).read_to_end(&mut data)?;
        Ok(data)
    }

    fn open_file(&self, path: &str) -> Result<(File, Metadata), HomeError> {
        let (parents, name) = parse_path(path)?;
        let dir = self.open_parents(&parents)?;

        // Non-blocking, so that opening a named pipe can't hang the host.
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        platform::dont_block(&mut options);
        let file = dir.open_with(name, &options)?;
        let metadata = file.metadata()?;
        if metadata.is_dir() {
            return Err(HomeError::NotAFile);
        }
        if !metadata.is_file() {
            return Err(HomeError::Denied);
        }
        Ok((file, metadata))
    }

    /// Lists a directory, sorted by name. An empty path means the home itself. Anything that isn't
    /// a regular file or directory is left out, since agents can't create those or use them.
    pub fn list(&self, path: &str) -> Result<Vec<HomeEntry>, HomeError> {
        let dir = if path.is_empty() {
            self.root.try_clone()?
        } else {
            let (parents, name) = parse_path(path)?;
            open_child(&self.open_parents(&parents)?, name)?
        };

        let mut entries = Vec::new();
        for entry in dir.entries()? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let kind = if file_type.is_file() {
                EntryKind::File
            } else if file_type.is_dir() {
                EntryKind::Directory
            } else {
                continue;
            };
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if entries.len() == MAX_LIST_ENTRIES {
                return Err(HomeError::TooManyEntries {
                    limit: MAX_LIST_ENTRIES,
                });
            }
            let size = match kind {
                EntryKind::File => entry.metadata()?.len(),
                EntryKind::Directory => 0,
            };
            entries.push(HomeEntry { name, kind, size });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Replaces a whole file, creating it and its parent directories if needed. If the host
    /// crashes, the file holds either its old contents or its new contents, never a mix.
    pub fn write(&mut self, path: &str, data: &[u8]) -> Result<(), HomeError> {
        let (parents, name) = parse_path(path)?;

        let mut dir = self.root.try_clone()?;
        let mut existing_parents = 0;
        for parent in &parents {
            match open_child(&dir, parent) {
                Ok(next) => dir = next,
                Err(HomeError::NotFound) => break,
                Err(error) => return Err(error),
            }
            existing_parents += 1;
        }
        let missing_parents = &parents[existing_parents..];

        let old_cost = if missing_parents.is_empty() {
            match dir.symlink_metadata(name) {
                Ok(metadata) => file_cost(&metadata)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
                Err(error) => return Err(error.into()),
            }
        } else {
            0
        };
        let new_cost = data.len() as u64 + ENTRY_COST + missing_parents.len() as u64 * ENTRY_COST;
        let needed = (self.used - old_cost.min(self.used)).saturating_add(new_cost);
        if needed > self.disk_limit {
            return Err(HomeError::DiskLimit {
                needed,
                limit: self.disk_limit,
            });
        }

        let temp_name = self.next_temp.to_string();
        self.next_temp += 1;
        let result = self.write_and_move(&temp_name, data, dir, missing_parents, name);
        if result.is_err() {
            // The temporary file may already have moved, so a failure here doesn't matter.
            let _ = self.temp.remove_file(&temp_name);
            // Some parent directories may exist now, so count again rather than guess.
            self.used = count_used(&self.root)?;
            return result;
        }
        self.used = needed;
        Ok(())
    }

    fn write_and_move(
        &self,
        temp_name: &str,
        data: &[u8],
        mut dir: Dir,
        missing_parents: &[&str],
        name: &str,
    ) -> Result<(), HomeError> {
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let mut file = self.temp.open_with(temp_name, &options)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);

        for parent in missing_parents {
            dir.create_dir(parent)?;
            sync_dir(&dir)?;
            dir = open_child(&dir, parent)?;
        }
        self.temp.rename(temp_name, &dir, name)?;
        sync_dir(&dir)?;
        Ok(())
    }

    /// Removes a file, and any parent directories that it leaves empty.
    pub fn remove(&mut self, path: &str) -> Result<(), HomeError> {
        let (parents, name) = parse_path(path)?;

        let mut dirs = vec![self.root.try_clone()?];
        for parent in &parents {
            let next = open_child(&dirs[dirs.len() - 1], parent)?;
            dirs.push(next);
        }
        let dir = dirs.pop().ok_or(HomeError::Io(io::ErrorKind::Other))?;
        let cost = file_cost(&dir.symlink_metadata(name)?)?;
        dir.remove_file(name)?;
        sync_dir(&dir)?;
        self.used = self.used.saturating_sub(cost);

        for (parent_dir, parent_name) in dirs.iter().zip(&parents).rev() {
            // Stops at the first directory that still has something in it.
            if parent_dir.remove_dir(parent_name).is_err() {
                break;
            }
            sync_dir(parent_dir)?;
            self.used = self.used.saturating_sub(ENTRY_COST);
        }
        Ok(())
    }

    fn open_parents(&self, parents: &[&str]) -> Result<Dir, HomeError> {
        let mut dir = self.root.try_clone()?;
        for parent in parents {
            dir = open_child(&dir, parent)?;
        }
        Ok(dir)
    }
}

/// Opens a directory inside `dir`, one level down, without following a symlink.
fn open_child(dir: &Dir, name: &str) -> Result<Dir, HomeError> {
    dir.open_dir_nofollow(name).map_err(|error| {
        let is_file = dir
            .symlink_metadata(name)
            .is_ok_and(|metadata| metadata.is_file());
        match HomeError::from(error) {
            // Refusing a symlink or special file here looks the same as passing through a file.
            HomeError::NotAFile if !is_file => HomeError::Denied,
            other => other,
        }
    })
}

/// What an existing file counts for, or an error if the path isn't a regular file.
fn file_cost(metadata: &Metadata) -> Result<u64, HomeError> {
    if metadata.is_dir() {
        return Err(HomeError::NotAFile);
    }
    if !metadata.is_file() {
        return Err(HomeError::Denied);
    }
    Ok(metadata.len().saturating_add(ENTRY_COST))
}

/// Saves a directory's entries to disk, as far as the system allows.
fn sync_dir(dir: &Dir) -> Result<(), HomeError> {
    platform::sync_dir(dir)?;
    Ok(())
}

/// Adds up the space used by a home, without following symlinks.
fn count_used(root: &Dir) -> Result<u64, HomeError> {
    let mut used: u64 = 0;
    let mut pending = vec![root.try_clone()?];
    while let Some(dir) = pending.pop() {
        for entry in dir.entries()? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            used = used.saturating_add(ENTRY_COST);
            if file_type.is_dir() {
                pending.push(dir.open_dir_nofollow(entry.file_name())?);
            } else if file_type.is_file() {
                used = used.saturating_add(entry.metadata()?.len());
            }
        }
    }
    Ok(used)
}

/// Splits an agent's path into its parent directories and file name.
fn parse_path(path: &str) -> Result<(Vec<&str>, &str), HomeError> {
    if path.is_empty() {
        return Err(HomeError::InvalidPath("path is empty"));
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(HomeError::InvalidPath("path is too long"));
    }
    let mut parts: Vec<&str> = path.split('/').collect();
    if parts.len() > MAX_PATH_DEPTH {
        return Err(HomeError::InvalidPath("path is too deep"));
    }
    for part in &parts {
        check_path_part(part)?;
    }
    let name = parts.pop().ok_or(HomeError::InvalidPath("path is empty"))?;
    Ok((parts, name))
}

/// Whether Windows would treat a name as a device instead of a file, with or without an extension.
/// This applies on every system, so that an agent's files mean the same thing everywhere.
fn is_device_name(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part).trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    match upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    {
        // Windows also counts these superscript digits.
        Some(rest) => matches!(
            rest,
            "0" | "1"
                | "2"
                | "3"
                | "4"
                | "5"
                | "6"
                | "7"
                | "8"
                | "9"
                | "\u{b9}"
                | "\u{b2}"
                | "\u{b3}"
        ),
        None => false,
    }
}

/// Allows letters and digits from any language, and a few common symbols. This rules out control
/// characters, invisible characters, right-to-left overrides, and lookalikes of `/` and `.`,
/// some of which file systems have ignored or changed in the past.
fn check_path_part(part: &str) -> Result<(), HomeError> {
    if part.is_empty() {
        return Err(HomeError::InvalidPath(
            "path must be relative, with no empty parts",
        ));
    }
    if part.len() > MAX_NAME_BYTES {
        return Err(HomeError::InvalidPath("a name in the path is too long"));
    }
    if part.ends_with('.') || part.starts_with(' ') || part.ends_with(' ') {
        return Err(HomeError::InvalidPath(
            "a name can't end with '.', which also rules out '.' and '..', or start or end with a space",
        ));
    }
    if is_device_name(part) {
        return Err(HomeError::InvalidPath(
            "a name can't be one that Windows keeps for a device, such as NUL",
        ));
    }
    let allowed =
        |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ' | '+' | '=' | '@' | ',');
    if !part.chars().all(allowed) {
        return Err(HomeError::InvalidPath(
            "path contains a character that isn't allowed",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::ffi::CString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    #[cfg(unix)]
    use std::sync::mpsc;
    use std::time::Duration;
    use std::{env, fs, thread};

    use tempfile::TempDir;

    use super::*;
    use crate::platform::test_support::symlink;

    const LIMIT: u64 = 1024 * 1024;
    const CRASH_DIR_VAR: &str = "HABITAT_TEST_CRASH_DIR";

    struct Fixture {
        root: TempDir,
        storage: AgentStorage,
        /// A folder outside the agent's storage, which must never be reached.
        outside: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = TempDir::new().unwrap();
            let storage = AgentStorage::open(root.path(), "test-agent").unwrap();
            let outside = root.path().join("outside");
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("secret"), "outside secret").unwrap();
            Self {
                root,
                storage,
                outside,
            }
        }

        fn home(&self) -> Home {
            self.storage.home(LIMIT).unwrap()
        }

        fn home_path(&self) -> PathBuf {
            self.storage.path.join(HOME_DIR)
        }

        fn temp_path(&self) -> PathBuf {
            self.storage.path.join(TEMP_DIR)
        }
    }

    #[test]
    fn reading_part_of_a_file() {
        let fixture = Fixture::new();
        let mut home = fixture.home();
        home.write("dir/file", b"0123456789").unwrap();

        assert_eq!(home.read_at("dir/file", 0, 4).unwrap(), b"0123");
        assert_eq!(home.read_at("dir/file", 4, 4).unwrap(), b"4567");
        assert_eq!(home.read_at("dir/file", 8, 4).unwrap(), b"89");
        assert_eq!(home.read_at("dir/file", 0, 0).unwrap(), b"");
        assert_eq!(home.read_at("dir/file", 9, 100).unwrap(), b"9");
        for past_the_end in [10, 11, u64::MAX] {
            assert_eq!(home.read_at("dir/file", past_the_end, 4).unwrap(), b"");
        }
    }

    #[test]
    fn reading_part_of_a_file_follows_the_same_rules() {
        let fixture = Fixture::new();
        let mut home = fixture.home();
        home.write("dir/file", b"x").unwrap();
        symlink(
            fixture.outside.join("secret"),
            fixture.home_path().join("link"),
        )
        .unwrap();

        assert_eq!(home.read_at("missing", 0, 4), Err(HomeError::NotFound));
        assert_eq!(home.read_at("dir", 0, 4), Err(HomeError::NotAFile));
        assert_eq!(home.read_at("link", 0, 4), Err(HomeError::Denied));
        assert!(matches!(
            home.read_at("../outside/secret", 0, 4),
            Err(HomeError::InvalidPath(_))
        ));
    }

    #[test]
    fn listing_directories() {
        let fixture = Fixture::new();
        let mut home = fixture.home();
        assert_eq!(home.list("").unwrap(), []);

        home.write("b.txt", b"12345").unwrap();
        home.write("a/nested/deep.txt", b"xy").unwrap();
        home.write("a/top.txt", b"x").unwrap();

        let file = |name: &str, size| HomeEntry {
            name: name.to_string(),
            kind: EntryKind::File,
            size,
        };
        let directory = |name: &str| HomeEntry {
            name: name.to_string(),
            kind: EntryKind::Directory,
            size: 0,
        };
        assert_eq!(home.list("").unwrap(), [directory("a"), file("b.txt", 5)]);
        assert_eq!(
            home.list("a").unwrap(),
            [directory("nested"), file("top.txt", 1)]
        );
        assert_eq!(home.list("a/nested").unwrap(), [file("deep.txt", 2)]);
    }

    #[test]
    fn listing_stays_inside_the_home() {
        let fixture = Fixture::new();
        let mut home = fixture.home();
        home.write("file", b"x").unwrap();
        symlink(&fixture.outside, fixture.home_path().join("link")).unwrap();
        home.write("real/inner.txt", b"x").unwrap();
        symlink("real", fixture.home_path().join("inside-link")).unwrap();

        assert_eq!(home.list("inside-link"), Err(HomeError::Denied));
        assert_eq!(home.list("missing"), Err(HomeError::NotFound));
        assert_eq!(home.list("file"), Err(HomeError::NotAFile));
        assert_eq!(home.list("link"), Err(HomeError::Denied));
        assert_eq!(home.list("link/deeper"), Err(HomeError::Denied));
        for path in ["..", "../outside", ".", "/", "/etc"] {
            assert!(
                matches!(home.list(path), Err(HomeError::InvalidPath(_))),
                "{path}"
            );
        }

        // A symlink doesn't appear in a listing, so it can't reveal what it points to.
        let names: Vec<_> = home.list("").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["file", "real"]);
    }

    #[test]
    fn listing_stops_at_its_limit() {
        let fixture = Fixture::new();
        let home = fixture.home();
        let path = fixture.home_path();
        for number in 0..MAX_LIST_ENTRIES {
            fs::write(path.join(number.to_string()), b"").unwrap();
        }
        assert_eq!(home.list("").unwrap().len(), MAX_LIST_ENTRIES);

        fs::write(path.join("one-too-many"), b"").unwrap();
        assert_eq!(
            home.list(""),
            Err(HomeError::TooManyEntries {
                limit: MAX_LIST_ENTRIES
            })
        );
    }

    #[test]
    fn write_then_read() {
        let fixture = Fixture::new();
        let mut home = fixture.home();
        home.write("notes/2026/today.txt", b"hello").unwrap();
        assert_eq!(home.read("notes/2026/today.txt").unwrap(), b"hello");
        home.write("notes/2026/today.txt", b"changed").unwrap();
        assert_eq!(home.read("notes/2026/today.txt").unwrap(), b"changed");
    }

    #[test]
    fn files_survive_reopening() {
        let fixture = Fixture::new();
        fixture.home().write("state.json", b"{}").unwrap();
        assert_eq!(fixture.home().read("state.json").unwrap(), b"{}");
    }

    #[test]
    fn empty_files_work() {
        let fixture = Fixture::new();
        let mut home = fixture.home();
        home.write("empty", b"").unwrap();
        assert_eq!(home.read("empty").unwrap(), b"");
    }

    #[test]
    fn missing_files_are_not_found() {
        let fixture = Fixture::new();
        let mut home = fixture.home();
        assert_eq!(home.read("missing"), Err(HomeError::NotFound));
        assert_eq!(home.read("missing/file"), Err(HomeError::NotFound));
        assert_eq!(home.remove("missing"), Err(HomeError::NotFound));
    }

    #[test]
    fn bad_paths_are_rejected() {
        let long_name = "a".repeat(MAX_NAME_BYTES + 1);
        let long_path = vec!["a".repeat(100); 11].join("/");
        let deep_path = vec!["a"; MAX_PATH_DEPTH + 1].join("/");
        for path in [
            "",
            "/",
            "/etc/passwd",
            "//etc/passwd",
            "..",
            "../secret",
            "a/../../secret",
            "a/..",
            ".",
            "./a",
            "a/.",
            "...",
            "a//b",
            "a/",
            "a\\b",
            "..\\secret",
            "C:\\secret",
            "a:b",
            "a\0b",
            "a\nb",
            "a\tb",
            "~/secret",
            "$HOME/secret",
            "a*",
            "a.",
            " a",
            "a ",
            &long_name,
            &long_path,
            &deep_path,
        ] {
            let fixture = Fixture::new();
            let mut home = fixture.home();
            assert!(
                matches!(home.read(path), Err(HomeError::InvalidPath(_))),
                "read {path:?}"
            );
            assert!(
                matches!(home.write(path, b"x"), Err(HomeError::InvalidPath(_))),
                "write {path:?}"
            );
            assert!(
                matches!(home.remove(path), Err(HomeError::InvalidPath(_))),
                "remove {path:?}"
            );
            assert_eq!(fs::read_dir(fixture.home_path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn names_that_mean_something_else_on_windows_are_rejected_everywhere() {
        let fixture = Fixture::new();
        let mut home = fixture.home();
        for path in [
            "NUL",
            "nul",
            "Nul.txt",
            "nul.tar.gz",
            "NUL .txt",
            "CON",
            "con.log",
            "PRN",
            "AUX",
            "COM0",
            "COM1",
            "com9.txt",
            "LPT1",
            "lpt9.dat",
            "COM\u{b9}",
            "lpt\u{b2}.txt",
            "COM\u{b3}",
            "folder/NUL",
            "NUL/file",
            "a/con/b",
            // Alternate data streams, which hide data inside a file.
            "file.txt:hidden",
            "file.txt::$DATA",
            // Short names, trailing dots and spaces, and other ways to name the same file.
            "PROGRA~1",
            "name.",
            "name ",
            "name...",
            "folder./file",
            // A drive, a device path and a backslash.
            "C:",
            "C:/Windows",
            "//./NUL",
            "a\\b",
            "..\\outside",
        ] {
            assert!(
                matches!(home.write(path, b"x"), Err(HomeError::InvalidPath(_))),
                "{path:?}"
            );
            assert!(
                matches!(home.read(path), Err(HomeError::InvalidPath(_))),
                "{path:?}"
            );
            assert!(
                matches!(home.remove(path), Err(HomeError::InvalidPath(_))),
                "{path:?}"
            );
        }
        assert_eq!(home.list("").unwrap(), []);
    }

    #[test]
    fn names_that_only_look_like_devices_are_fine() {
        let fixture = Fixture::new();
        let mut home = fixture.home();
        for path in [
            "console",
            "nullable.txt",
            "COM",
            "COM10",
            "LPT",
            "auxiliary",
            "my.con",
        ] {
            home.write(path, b"x").unwrap();
            assert_eq!(home.read(path).unwrap(), b"x", "{path}");
        }
    }

    #[cfg(windows)]
    fn make_junction(link: &Path, target: &Path) {
        let output = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    /// Junctions are a Windows kind of link that any user can make, without special rights.
    #[cfg(windows)]
    #[test]
    fn junctions_are_denied() {
        let fixture = Fixture::new();
        fs::write(fixture.outside.join("secret"), "secret").unwrap();
        make_junction(&fixture.home_path().join("link"), &fixture.outside);

        let mut home = fixture.home();
        assert!(home.read("link/secret").is_err());
        assert!(home.read_at("link/secret", 0, 16).is_err());
        assert!(home.write("link/new", b"x").is_err());
        assert!(home.remove("link/secret").is_err());
        assert!(home.list("link").is_err());
        assert!(
            home.list("")
                .unwrap()
                .iter()
                .all(|entry| entry.name != "link")
        );
        assert_eq!(
            fs::read_to_string(fixture.outside.join("secret")).unwrap(),
            "secret"
        );
        assert!(!fixture.outside.join("new").exists());
    }

    /// Only the link goes, and whatever it led to stays.
    #[cfg(windows)]
    #[test]
    fn counting_space_doesnt_follow_junctions() {
        let fixture = Fixture::new();
        fs::write(fixture.outside.join("big"), vec![0; 1024 * 1024]).unwrap();
        make_junction(&fixture.home_path().join("link"), &fixture.outside);
        assert!(fixture.home().used_bytes() < 1024 * 1024);
    }

    #[test]
    fn tricky_unicode_is_rejected() {
        let fixture = Fixture::new();
        let mut home = fixture.home();
        for path in [
            "a\u{0301}",          // A combining accent, which file systems may normalize.
            "\u{202e}txt.exe",    // A right-to-left override.
            "a\u{200d}b",         // A zero-width joiner.
            ".\u{200c}.",         // Some file systems have ignored this, leaving "..".
            "\u{feff}a",          // A byte order mark.
            "\u{ff0e}\u{ff0e}/a", // Full-width dots.
            "\u{2024}\u{2024}/a", // One dot leaders.
            "a\u{2215}b",         // A division slash.
            "a\u{ff0f}b",         // A full-width slash.
            "a\u{ad}b",           // A soft hyphen.
            "\u{1f600}",          // Not harmful, but not a letter or digit.
        ] {
            assert!(
                matches!(home.write(path, b"x"), Err(HomeError::InvalidPath(_))),
                "{path:?}"
            );
        }
    }

    #[test]
    fn ordinary_names_are_allowed() {
        let fixture = Fixture::new();
        let mut home = fixture.home();
        let longest = "a".repeat(MAX_NAME_BYTES);
        for path in [
            "a",
            ".config/settings.toml",
            "file.tar.gz",
            "my notes.txt",
            "a-b_c+d=e@f,g",
            "caf\u{e9}.txt",
            "\u{65e5}\u{672c}\u{8a9e}/\u{30e1}\u{30e2}.txt",
            "\u{0627}\u{0644}\u{0639}\u{0631}\u{0628}\u{064a}\u{0629}.txt",
            &longest,
        ] {
            home.write(path, b"x")
                .unwrap_or_else(|e| panic!("{path:?}: {e}"));
            assert_eq!(home.read(path).unwrap(), b"x");
        }
        let deepest = vec!["deep"; MAX_PATH_DEPTH].join("/");
        home.write(&deepest, b"x").unwrap();
    }

    #[test]
    fn symlinks_to_files_are_denied() {
        let fixture = Fixture::new();
        let link = fixture.home_path().join("link");
        symlink(fixture.outside.join("secret"), &link).unwrap();

        let mut home = fixture.home();
        assert_eq!(home.read("link"), Err(HomeError::Denied));
        assert_eq!(home.write("link", b"overwritten"), Err(HomeError::Denied));
        assert_eq!(home.remove("link"), Err(HomeError::Denied));
        assert_eq!(
            fs::read_to_string(fixture.outside.join("secret")).unwrap(),
            "outside secret"
        );
    }

    #[test]
    fn symlinked_directories_are_denied() {
        let fixture = Fixture::new();
        symlink(&fixture.outside, fixture.home_path().join("link")).unwrap();

        let mut home = fixture.home();
        assert_eq!(home.read("link/secret"), Err(HomeError::Denied));
        assert_eq!(home.write("link/new", b"x"), Err(HomeError::Denied));
        assert_eq!(home.remove("link/secret"), Err(HomeError::Denied));
        assert!(!fixture.outside.join("new").exists());
        assert!(fixture.outside.join("secret").exists());
    }

    #[test]
    fn relative_symlinks_and_symlinks_inside_the_home_are_denied() {
        let fixture = Fixture::new();
        let mut home = fixture.home();
        home.write("real", b"x").unwrap();
        symlink("real", fixture.home_path().join("inside")).unwrap();
        symlink("../grants.db", fixture.home_path().join("up")).unwrap();
        symlink("up", fixture.home_path().join("chain")).unwrap();
        fs::write(fixture.storage.grants_path(), "grants").unwrap();

        for path in ["inside", "up", "chain"] {
            assert_eq!(home.read(path), Err(HomeError::Denied), "{path}");
            assert_eq!(home.write(path, b"y"), Err(HomeError::Denied), "{path}");
        }
        assert_eq!(
            fs::read_to_string(fixture.storage.grants_path()).unwrap(),
            "grants"
        );
    }

    #[test]
    fn symlinked_directories_inside_the_home_are_denied() {
        let fixture = Fixture::new();
        let mut home = fixture.home();
        home.write("real/file", b"x").unwrap();
        symlink("real", fixture.home_path().join("link")).unwrap();

        assert_eq!(home.read("link/file"), Err(HomeError::Denied));
        assert_eq!(home.write("link/new", b"y"), Err(HomeError::Denied));
        assert_eq!(home.remove("link/file"), Err(HomeError::Denied));
        assert_eq!(home.read("real/file").unwrap(), b"x");
        assert_eq!(home.read("real/new"), Err(HomeError::NotFound));
    }

    #[test]
    fn swapping_a_directory_for_a_symlink_never_leaves_the_home() {
        let fixture = Fixture::new();
        let mut home = fixture.home();
        home.write("swap/file", b"inside").unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let swap = fixture.home_path().join("swap");
        let parked = fixture.home_path().join("parked");
        let link = fixture.home_path().join("link");
        symlink(&fixture.outside, &link).unwrap();
        fs::write(fixture.outside.join("file"), "outside secret").unwrap();

        // Repeatedly replaces the directory with a symlink that leads outside the home. Writes can
        // recreate the directory at any moment, so a failed rename here is expected.
        let swapper = {
            let stop = stop.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = fs::rename(&swap, &parked);
                    let _ = fs::rename(&link, &swap);
                    let _ = fs::rename(&swap, &link);
                    let _ = fs::rename(&parked, &swap);
                }
            })
        };

        for round in 0..2000 {
            if let Ok(data) = home.read("swap/file") {
                assert_ne!(data, b"outside secret", "read outside on round {round}");
            }
            let _ = home.write("swap/planted", b"x");
            let _ = home.remove("swap/secret");
        }
        stop.store(true, Ordering::Relaxed);
        swapper.join().unwrap();

        assert!(!fixture.outside.join("planted").exists());
        assert_eq!(
            fs::read_to_string(fixture.outside.join("secret")).unwrap(),
            "outside secret"
        );
    }

    #[cfg(unix)]
    #[test]
    #[cfg_attr(
        target_os = "android",
        ignore = "Android's security policy stops this test from creating a named pipe"
    )]
    fn named_pipes_are_denied_without_hanging() {
        let fixture = Fixture::new();
        let pipe = fixture.home_path().join("pipe");
        let c_path = CString::new(pipe.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path is a valid C string that lives for the whole call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        let mut home = fixture.home();
        assert_eq!(home.write("pipe", b"x"), Err(HomeError::Denied));
        assert_eq!(home.remove("pipe"), Err(HomeError::Denied));

        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || sender.send(home.read("pipe")).unwrap());
        let result = receiver.recv_timeout(Duration::from_secs(10));
        assert_eq!(result, Ok(Err(HomeError::Denied)));
    }

    #[test]
    fn directories_and_files_are_not_mixed_up() {
        let fixture = Fixture::new();
        let mut home = fixture.home();
        home.write("dir/file", b"x").unwrap();
        assert_eq!(home.read("dir"), Err(HomeError::NotAFile));
        assert_eq!(home.write("dir", b"y"), Err(HomeError::NotAFile));
        assert_eq!(home.remove("dir"), Err(HomeError::NotAFile));
        assert_eq!(home.read("dir/file/under"), Err(HomeError::NotAFile));
        assert_eq!(home.write("dir/file/under", b"y"), Err(HomeError::NotAFile));
        assert_eq!(home.read("dir/file").unwrap(), b"x");
    }

    #[test]
    fn other_storage_is_outside_the_home() {
        let fixture = Fixture::new();
        fs::write(fixture.storage.grants_path(), "grants").unwrap();
        fs::write(fixture.storage.audit_path(), "audit").unwrap();
        fs::write(fixture.storage.package_path().join("agent.toml"), "x").unwrap();

        let mut home = fixture.home();
        for path in [
            "grants.db",
            "audit.db",
            "package/agent.toml",
            "tmp/0",
            "../grants.db",
            "../audit.db",
            "../package/agent.toml",
            "../tmp/0",
        ] {
            assert!(home.read(path).is_err(), "{path}");
        }
        home.write("grants.db", b"mine").unwrap();
        home.write("package/agent.toml", b"mine").unwrap();
        assert_eq!(
            fs::read_to_string(fixture.storage.grants_path()).unwrap(),
            "grants"
        );
        assert_eq!(
            fs::read_to_string(fixture.storage.package_path().join("agent.toml")).unwrap(),
            "x"
        );
    }

    #[test]
    fn only_real_agent_folders_count_as_agents() {
        let root = TempDir::new().unwrap();
        assert_eq!(
            AgentStorage::names(root.path()).unwrap(),
            Vec::<String>::new()
        );
        AgentStorage::open(root.path(), "zebra").unwrap();
        AgentStorage::open(root.path(), "ant").unwrap();
        let agents = root.path().join(AGENTS_DIR);
        fs::write(agents.join("a-file"), "x").unwrap();
        fs::create_dir(agents.join("Not-A-Name")).unwrap();
        fs::create_dir(agents.join(".hidden")).unwrap();
        assert_eq!(AgentStorage::names(root.path()).unwrap(), ["ant", "zebra"]);
    }

    #[test]
    fn bad_agent_names_are_rejected() {
        let root = TempDir::new().unwrap();
        for name in ["", "..", "../other", "a/b", "/etc", "Agent", "a\0b"] {
            assert!(
                matches!(
                    AgentStorage::open(root.path(), name),
                    Err(HomeError::InvalidPath(_))
                ),
                "{name:?}"
            );
        }
        assert!(!root.path().join(AGENTS_DIR).exists());
    }

    #[test]
    fn agents_cannot_see_each_other() {
        let root = TempDir::new().unwrap();
        let mut first = AgentStorage::open(root.path(), "first")
            .unwrap()
            .home(LIMIT)
            .unwrap();
        let second = AgentStorage::open(root.path(), "second")
            .unwrap()
            .home(LIMIT)
            .unwrap();
        first.write("file", b"first").unwrap();
        assert_eq!(second.read("file"), Err(HomeError::NotFound));
        assert!(matches!(
            second.read("../../first/home/file"),
            Err(HomeError::InvalidPath(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn only_the_current_user_can_open_storage() {
        let fixture = Fixture::new();
        let mode = fs::metadata(&fixture.storage.path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn errors_do_not_include_host_paths() {
        let fixture = Fixture::new();
        symlink(&fixture.outside, fixture.home_path().join("link")).unwrap();
        let mut home = fixture.home();
        let root = fixture.root.path().to_string_lossy().into_owned();
        for error in [
            home.read("missing").unwrap_err(),
            home.read("link/secret").unwrap_err(),
            home.read("../x").unwrap_err(),
            home.write(&"a".repeat(MAX_PATH_BYTES + 1), b"x")
                .unwrap_err(),
            home.write("big", &vec![0; LIMIT as usize + 1]).unwrap_err(),
        ] {
            let text = format!("{error} {error:?}");
            assert!(!text.contains(&root), "{text}");
        }
    }

    #[test]
    fn writes_stop_at_the_disk_limit() {
        let fixture = Fixture::new();
        let limit = 3 * ENTRY_COST;
        let mut home = fixture.storage.home(limit).unwrap();
        let exact = (limit - ENTRY_COST) as usize;
        home.write("file", &vec![1; exact]).unwrap();
        assert_eq!(home.used_bytes(), limit);

        assert!(matches!(
            home.write("other", b""),
            Err(HomeError::DiskLimit { .. })
        ));
        assert!(matches!(
            home.write("file", &vec![2; exact + 1]),
            Err(HomeError::DiskLimit { .. })
        ));
        assert_eq!(home.read("file").unwrap(), vec![1; exact]);
        assert!(!fixture.home_path().join("other").exists());
        assert_eq!(fs::read_dir(fixture.temp_path()).unwrap().count(), 0);
    }

    #[test]
    fn overwriting_and_removing_give_space_back() {
        let fixture = Fixture::new();
        let limit = 3 * ENTRY_COST;
        let mut home = fixture.storage.home(limit).unwrap();
        let exact = (limit - ENTRY_COST) as usize;
        home.write("file", &vec![1; exact]).unwrap();

        home.write("file", &vec![2; exact]).unwrap();
        home.write("file", b"small").unwrap();
        assert_eq!(home.used_bytes(), ENTRY_COST + 5);

        home.remove("file").unwrap();
        assert_eq!(home.used_bytes(), 0);
        home.write("new", &vec![3; exact]).unwrap();
    }

    #[test]
    fn empty_files_and_directories_use_up_the_limit() {
        let fixture = Fixture::new();
        let mut home = fixture.storage.home(10 * ENTRY_COST).unwrap();
        let mut created = 0;
        while home.write(&format!("f{created}"), b"").is_ok() {
            created += 1;
            assert!(created <= 10);
        }
        assert_eq!(created, 10);

        let mut home = Fixture::new().storage.home(2 * ENTRY_COST).unwrap();
        assert!(matches!(
            home.write("a/b/file", b""),
            Err(HomeError::DiskLimit { .. })
        ));
        assert_eq!(home.used_bytes(), 0);
    }

    #[test]
    fn a_failed_write_creates_no_directories() {
        let fixture = Fixture::new();
        let mut home = fixture.storage.home(ENTRY_COST).unwrap();
        assert!(home.write("a/b/file", b"x").is_err());
        assert!(!fixture.home_path().join("a").exists());
    }

    #[test]
    fn removing_a_file_removes_empty_parents_only() {
        let fixture = Fixture::new();
        let mut home = fixture.home();
        home.write("a/b/one", b"1").unwrap();
        home.write("a/two", b"2").unwrap();
        home.remove("a/b/one").unwrap();
        assert!(!fixture.home_path().join("a/b").exists());
        assert_eq!(home.read("a/two").unwrap(), b"2");
        home.remove("a/two").unwrap();
        assert!(!fixture.home_path().join("a").exists());
        assert!(fixture.home_path().exists());
        assert_eq!(home.used_bytes(), 0);
    }

    #[test]
    fn reopening_counts_the_same_space() {
        let fixture = Fixture::new();
        let mut home = fixture.home();
        home.write("a/b/one", &[1; 100]).unwrap();
        home.write("a/two", &[2; 200]).unwrap();
        home.write("three", &[3; 300]).unwrap();
        home.write("three", &[3; 30]).unwrap();
        home.remove("a/two").unwrap();
        assert_eq!(fixture.home().used_bytes(), home.used_bytes());
    }

    #[test]
    fn symlinks_are_not_followed_when_counting_space() {
        let fixture = Fixture::new();
        fs::write(fixture.outside.join("huge"), vec![0; LIMIT as usize * 2]).unwrap();
        symlink(&fixture.outside, fixture.home_path().join("dir")).unwrap();
        symlink(
            fixture.outside.join("huge"),
            fixture.home_path().join("file"),
        )
        .unwrap();
        assert_eq!(fixture.home().used_bytes(), 2 * ENTRY_COST);

        fs::write(fixture.home_path().join("real"), [0; 100]).unwrap();
        symlink("real", fixture.home_path().join("inside")).unwrap();
        assert_eq!(fixture.home().used_bytes(), 4 * ENTRY_COST + 100);
    }

    #[test]
    fn oversized_files_are_not_read_into_memory() {
        let fixture = Fixture::new();
        fs::write(
            fixture.home_path().join("big"),
            vec![0; 2 * ENTRY_COST as usize],
        )
        .unwrap();
        let home = fixture.storage.home(ENTRY_COST).unwrap();
        assert!(matches!(home.read("big"), Err(HomeError::DiskLimit { .. })));
    }

    #[test]
    fn leftover_temporary_files_are_removed_on_open() {
        let fixture = Fixture::new();
        fs::write(fixture.temp_path().join("0"), "partial").unwrap();
        let mut home = fixture.home();
        assert_eq!(home.used_bytes(), 0);
        assert_eq!(fs::read_dir(fixture.temp_path()).unwrap().count(), 0);
        home.write("file", b"x").unwrap();
    }

    /// The contents written in turn by the crash test. Every byte matches, and each size is
    /// different, so a partial or mixed file can always be spotted.
    fn crash_contents(round: usize) -> Vec<u8> {
        let (byte, size) = [(1, 300_000), (2, 200_000), (3, 100_000)][round % 3];
        vec![byte; size]
    }

    #[test]
    #[ignore = "run by killing_a_write_leaves_the_old_or_new_file"]
    fn crash_test_writer() {
        let dir = env::var(CRASH_DIR_VAR).expect("only for the crash test");
        let mut home = AgentStorage::open(Path::new(&dir), "crash")
            .unwrap()
            .home(LIMIT)
            .unwrap();
        let mut round = 0;
        loop {
            home.write("file", &crash_contents(round)).unwrap();
            round += 1;
        }
    }

    #[test]
    fn killing_a_write_leaves_the_old_or_new_file() {
        let root = TempDir::new().unwrap();
        let storage = AgentStorage::open(root.path(), "crash").unwrap();
        let valid: Vec<Vec<u8>> = (0..3).map(crash_contents).collect();
        let file = storage.path.join(HOME_DIR).join("file");

        for round in 0..15 {
            let mut child = Command::new(env::current_exe().unwrap())
                .args(["--exact", "home::tests::crash_test_writer", "--ignored"])
                .env(CRASH_DIR_VAR, root.path())
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            // Waits for a write to land, then lets it run for a varied time before killing it.
            while round == 0 && !file.exists() {
                thread::sleep(Duration::from_millis(1));
            }
            thread::sleep(Duration::from_millis(20 + round * 7));
            child.kill().unwrap();
            child.wait().unwrap();

            let home = storage.home(LIMIT).unwrap();
            let data = home.read("file").unwrap();
            assert!(
                valid.contains(&data),
                "partial file of {} bytes",
                data.len()
            );
            assert_eq!(home.used_bytes(), data.len() as u64 + ENTRY_COST);
            assert_eq!(
                fs::read_dir(storage.path.join(TEMP_DIR)).unwrap().count(),
                0
            );
        }
    }
}
