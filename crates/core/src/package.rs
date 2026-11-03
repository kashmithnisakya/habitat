//! Agent packages: one file that holds an agent's manifest and WebAssembly component.
//!
//! Packages are untrusted, so reading one checks everything before anything is unpacked. See
//! `docs/packages.md`.

use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;

use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tar::{Archive, Builder, Entries, EntryType, Header};

use crate::home::AgentStorage;
use crate::manifest::{MAX_MANIFEST_BYTES, Manifest, ManifestError};
use crate::platform;

/// The package format this version of Habitat reads and writes.
pub const FORMAT_VERSION: u32 = 1;
/// The largest `agent.wasm` a package can hold.
pub const MAX_COMPONENT_BYTES: u64 = 128 * 1024 * 1024;

const INDEX_NAME: &str = "package.toml";
const MANIFEST_NAME: &str = "agent.toml";
const COMPONENT_NAME: &str = "agent.wasm";
const PACKAGE_NAME: &str = "package";

const MAX_INDEX_BYTES: u64 = 4096;
const MAX_MANIFEST: u64 = MAX_MANIFEST_BYTES as u64;
// Room for the headers and padding, which also bounds any zeros after the last file.
const MAX_ARCHIVE_BYTES: u64 = MAX_COMPONENT_BYTES + MAX_MANIFEST + MAX_INDEX_BYTES + 1024 * 1024;

// A WebAssembly component, rather than a core module, which has a different version here.
const COMPONENT_HEADER: &[u8] = b"\0asm\x0d\0\x01\0";

/// Identifies a package's exact contents. It's the SHA-256 of `package.toml`, which holds the
/// SHA-256 of every other file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PackageHash([u8; 32]);

impl fmt::Display for PackageHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Why a package was rejected. Messages are safe to print, even for a hostile package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageError {
    /// The package isn't laid out as this format requires.
    Malformed(&'static str),
    /// The package or one of its files is over its limit.
    TooLarge { file: &'static str, limit: u64 },
    /// The package uses a format this version of Habitat can't read.
    UnsupportedFormat,
    /// The manifest was rejected.
    Manifest(ManifestError),
    /// A file doesn't match its recorded hash, so it has changed.
    HashMismatch { file: &'static str },
    /// The package belongs to a different agent than the folder it's being used with.
    WrongAgent,
    /// The agent has no package installed.
    NotInstalled,
    /// The agent already has a package, and upgrades aren't supported yet.
    AlreadyInstalled,
    /// Reading or writing failed. This holds no host paths.
    Io(io::ErrorKind),
}

impl fmt::Display for PackageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(message) => write!(f, "invalid package: {message}"),
            Self::TooLarge { file, limit } => {
                write!(f, "{file} is over the limit of {limit} bytes")
            }
            Self::UnsupportedFormat => write!(
                f,
                "this version of Habitat only reads package format {FORMAT_VERSION}"
            ),
            Self::Manifest(error) => write!(f, "invalid manifest: {error}"),
            Self::HashMismatch { file } => write!(f, "{file} doesn't match its recorded hash"),
            Self::WrongAgent => write!(f, "the package belongs to a different agent"),
            Self::NotInstalled => write!(f, "no package is installed"),
            Self::AlreadyInstalled => write!(f, "a package is already installed"),
            Self::Io(kind) => write!(f, "file system error: {kind}"),
        }
    }
}

impl std::error::Error for PackageError {}

impl From<io::Error> for PackageError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

#[derive(Deserialize)]
struct RawFormat {
    format: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIndex {
    #[serde(rename = "format")]
    _format: i64,
    files: RawFiles,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFiles {
    #[serde(rename = "agent.toml")]
    manifest: String,
    #[serde(rename = "agent.wasm")]
    component: String,
}

/// A package whose contents have all been checked.
#[derive(Debug)]
pub struct Package {
    index: Vec<u8>,
    manifest_text: Vec<u8>,
    manifest: Manifest,
    // Shared, so that a backend can compile it on another thread without a second copy of it.
    component: Arc<Vec<u8>>,
    hash: PackageHash,
}

impl Package {
    /// Makes a package from the text of an `agent.toml` and a WebAssembly component.
    pub fn new(manifest_text: &str, component: Vec<u8>) -> Result<Self, PackageError> {
        if component.len() as u64 > MAX_COMPONENT_BYTES {
            return Err(PackageError::TooLarge {
                file: COMPONENT_NAME,
                limit: MAX_COMPONENT_BYTES,
            });
        }
        let index = format!(
            "format = {FORMAT_VERSION}\n\n[files]\n\"{MANIFEST_NAME}\" = \"{}\"\n\"{COMPONENT_NAME}\" = \"{}\"\n",
            hex_sha256(manifest_text.as_bytes()),
            hex_sha256(&component),
        );
        Self::from_files(
            index.into_bytes(),
            manifest_text.as_bytes().to_vec(),
            component,
        )
    }

    /// Reads and checks a package file.
    pub fn read(reader: impl Read) -> Result<Self, PackageError> {
        let mut archive = Archive::new(reader.take(MAX_ARCHIVE_BYTES));

        // Raw, so that long-name and extended header entries are rejected rather than applied.
        let mut entries = archive.entries().map_err(|_| BAD_ARCHIVE)?.raw(true);
        let index = read_entry(&mut entries, INDEX_NAME, MAX_INDEX_BYTES)?;
        let manifest_text = read_entry(&mut entries, MANIFEST_NAME, MAX_MANIFEST)?;
        let component = read_entry(&mut entries, COMPONENT_NAME, MAX_COMPONENT_BYTES)?;
        if entries.next().is_some() {
            return Err(PackageError::Malformed("it has an extra entry"));
        }

        // Anything after the end of the archive would be invisible to every check here.
        let mut rest = archive.into_inner();
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = rest.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            if buffer[..count].iter().any(|byte| *byte != 0) {
                return Err(PackageError::Malformed("it has data after its last entry"));
            }
        }
        if rest.limit() == 0 {
            return Err(PackageError::TooLarge {
                file: PACKAGE_NAME,
                limit: MAX_ARCHIVE_BYTES,
            });
        }

        Self::from_files(index, manifest_text, component)
    }

    fn from_files(
        index: Vec<u8>,
        manifest_text: Vec<u8>,
        component: Vec<u8>,
    ) -> Result<Self, PackageError> {
        let files = parse_index(&index)?;
        if files.manifest != hex_sha256(&manifest_text) {
            return Err(PackageError::HashMismatch {
                file: MANIFEST_NAME,
            });
        }
        if files.component != hex_sha256(&component) {
            return Err(PackageError::HashMismatch {
                file: COMPONENT_NAME,
            });
        }

        let text = std::str::from_utf8(&manifest_text)
            .map_err(|_| PackageError::Malformed("agent.toml isn't UTF-8"))?;
        let manifest = Manifest::parse(text).map_err(PackageError::Manifest)?;

        // Only a quick check. The runtime fully validates the component when it compiles it.
        if !component.starts_with(COMPONENT_HEADER) {
            return Err(PackageError::Malformed(
                "agent.wasm isn't a WebAssembly component",
            ));
        }

        let hash = PackageHash(Sha256::digest(&index).into());
        Ok(Self {
            index,
            manifest_text,
            manifest,
            component: Arc::new(component),
            hash,
        })
    }

    /// The validated manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The WebAssembly component. It hasn't been fully validated yet.
    pub fn component(&self) -> &[u8] {
        &self.component
    }

    /// The same as `component`, for use after this package may have gone.
    pub fn shared_component(&self) -> Arc<Vec<u8>> {
        Arc::clone(&self.component)
    }

    /// The hash of the package's contents.
    pub fn hash(&self) -> PackageHash {
        self.hash
    }

    /// Writes the package file. The same contents always give the same bytes.
    pub fn write(&self, writer: impl Write) -> Result<(), PackageError> {
        let mut builder = Builder::new(writer);
        for (name, data) in self.files() {
            let mut header = Header::new_ustar();
            header.set_entry_type(EntryType::Regular);
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            builder.append_data(&mut header, name, data)?;
        }
        builder.finish()?;
        Ok(())
    }

    /// Unpacks the package into an agent's folder, leaving its home alone. The agent counts as
    /// installed only once the hash is recorded, which happens last, so an interrupted install
    /// can be run again.
    pub fn install(&self, storage: &AgentStorage) -> Result<(), PackageError> {
        if storage.name() != self.manifest.name() {
            return Err(PackageError::WrongAgent);
        }
        let record = storage.package_hash_path();
        if record.symlink_metadata().is_ok() {
            return Err(PackageError::AlreadyInstalled);
        }

        let folder = storage.package_path();
        fs::remove_dir_all(&folder)?;
        platform::create_private_dir(&folder)?;
        for (name, data) in self.files() {
            platform::write_new_file(&folder.join(name), data, true)?;
        }
        platform::sync_dir_path(&folder)?;

        let temp = record.with_extension("new");
        if temp.symlink_metadata().is_ok() {
            fs::remove_file(&temp)?;
        }
        platform::write_new_file(&temp, format!("{}\n", self.hash).as_bytes(), false)?;
        fs::rename(&temp, &record)?;
        if let Some(parent) = record.parent() {
            platform::sync_dir_path(parent)?;
        }
        Ok(())
    }

    /// Reads an agent's installed package, and checks that it hasn't changed since it was
    /// installed. Run the copy this returns, so that a later change can't go unnoticed.
    pub fn load_installed(storage: &AgentStorage) -> Result<Self, PackageError> {
        let record = match read_installed(&storage.package_hash_path(), 65) {
            Err(PackageError::Io(io::ErrorKind::NotFound)) => {
                return Err(PackageError::NotInstalled);
            }
            other => other?,
        };

        let folder = storage.package_path();
        let mut names = 0;
        for entry in fs::read_dir(&folder)? {
            let entry = entry?;
            let name = entry.file_name();
            if ![INDEX_NAME, MANIFEST_NAME, COMPONENT_NAME]
                .iter()
                .any(|known| name == *known)
            {
                return Err(PackageError::Malformed(
                    "the package folder has extra files",
                ));
            }
            names += 1;
        }
        if names != 3 {
            return Err(PackageError::Malformed(
                "the package folder is missing files",
            ));
        }

        let package = Self::from_files(
            read_installed(&folder.join(INDEX_NAME), MAX_INDEX_BYTES)?,
            read_installed(&folder.join(MANIFEST_NAME), MAX_MANIFEST)?,
            read_installed(&folder.join(COMPONENT_NAME), MAX_COMPONENT_BYTES)?,
        )?;
        if record != format!("{}\n", package.hash).as_bytes() {
            return Err(PackageError::HashMismatch { file: INDEX_NAME });
        }
        if storage.name() != package.manifest.name() {
            return Err(PackageError::WrongAgent);
        }
        Ok(package)
    }

    /// Reads the version of an agent's installed package, to show to someone. This only reads the
    /// manifest, so unlike [`Package::load_installed`], it doesn't check that nothing has changed.
    pub fn installed_version(storage: &AgentStorage) -> Result<Version, PackageError> {
        let path = storage.package_path().join(MANIFEST_NAME);
        let text = read_installed(&path, MAX_MANIFEST)?;
        let text = std::str::from_utf8(&text)
            .map_err(|_| PackageError::Malformed("agent.toml isn't UTF-8"))?;
        let manifest = Manifest::parse(text).map_err(PackageError::Manifest)?;
        Ok(manifest.version().clone())
    }

    fn files(&self) -> [(&'static str, &[u8]); 3] {
        [
            (INDEX_NAME, &self.index),
            (MANIFEST_NAME, &self.manifest_text),
            (COMPONENT_NAME, self.component.as_slice()),
        ]
    }
}

const BAD_ARCHIVE: PackageError = PackageError::Malformed("it isn't a valid archive");
const BAD_INDEX: PackageError = PackageError::Malformed("package.toml isn't valid");

/// Reads the next entry, which must be a regular file with exactly this name. Nothing from the
/// entry's header is used except its size.
fn read_entry<R: Read>(
    entries: &mut Entries<'_, R>,
    name: &'static str,
    limit: u64,
) -> Result<Vec<u8>, PackageError> {
    let mut entry = entries
        .next()
        .ok_or(PackageError::Malformed("it's missing a file"))?
        .map_err(|_| BAD_ARCHIVE)?;
    if entry.header().entry_type() != EntryType::Regular {
        return Err(PackageError::Malformed("it has an entry that isn't a file"));
    }
    if entry.path_bytes().as_ref() != name.as_bytes() {
        return Err(PackageError::Malformed("it has an unexpected entry"));
    }
    let size = entry.header().size().map_err(|_| BAD_ARCHIVE)?;
    if size > limit {
        return Err(PackageError::TooLarge { file: name, limit });
    }

    let mut data = Vec::new();
    entry.read_to_end(&mut data).map_err(|_| BAD_ARCHIVE)?;
    if data.len() as u64 != size {
        return Err(PackageError::Malformed("it's cut short"));
    }
    Ok(data)
}

fn parse_index(index: &[u8]) -> Result<RawFiles, PackageError> {
    let text = std::str::from_utf8(index).map_err(|_| BAD_INDEX)?;
    // The format comes first, because a later format may have fields that aren't known here.
    let format: RawFormat = toml::from_str(text).map_err(|_| BAD_INDEX)?;
    if format.format != i64::from(FORMAT_VERSION) {
        return Err(PackageError::UnsupportedFormat);
    }
    let raw: RawIndex = toml::from_str(text).map_err(|_| BAD_INDEX)?;
    Ok(raw.files)
}

fn hex_sha256(data: &[u8]) -> String {
    PackageHash(Sha256::digest(data).into()).to_string()
}

/// Reads a file from an agent's folder, refusing anything that isn't a regular file.
fn read_installed(path: &Path, limit: u64) -> Result<Vec<u8>, PackageError> {
    const NOT_A_FILE: PackageError =
        PackageError::Malformed("an installed file isn't a regular file");

    let file = platform::open_no_follow(path).map_err(|error| {
        if platform::is_link_refused(&error) {
            NOT_A_FILE
        } else {
            error.into()
        }
    })?;
    if !file.metadata()?.is_file() {
        return Err(NOT_A_FILE);
    }
    let mut data = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut data)?;
    if data.len() as u64 > limit {
        return Err(PackageError::Malformed("an installed file is too large"));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::ffi::CString;
    use std::io::Cursor;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::sync::mpsc;
    #[cfg(unix)]
    use std::thread;
    #[cfg(unix)]
    use std::time::Duration;

    use super::*;
    use crate::home::HomeError;
    use crate::platform::test_support::symlink;

    const EXAMPLE: &str = include_str!("../../../examples/research-helper/agent.toml");
    const OTHER_AGENT: &str = "[agent]\nname = \"other-agent\"\nversion = \"1.0.0\"\n\n[limits]\nmemory_mb = 1\nrun_seconds = 1\ndisk_mb = 1\n";

    fn component() -> Vec<u8> {
        let mut bytes = COMPONENT_HEADER.to_vec();
        bytes.extend_from_slice(b"pretend this is agent code");
        bytes
    }

    fn package() -> Package {
        Package::new(EXAMPLE, component()).unwrap()
    }

    fn archive() -> Vec<u8> {
        let mut bytes = Vec::new();
        package().write(&mut bytes).unwrap();
        bytes
    }

    fn index_for(manifest: &[u8], component: &[u8]) -> Vec<u8> {
        format!(
            "format = 1\n\n[files]\n\"agent.toml\" = \"{}\"\n\"agent.wasm\" = \"{}\"\n",
            hex_sha256(manifest),
            hex_sha256(component),
        )
        .into_bytes()
    }

    /// Writes an entry with exactly these bytes as its name and type, including ones that the tar
    /// library refuses to write through its normal functions.
    fn raw_entry(out: &mut Vec<u8>, name: &[u8], kind: EntryType, data: &[u8]) {
        let mut header = Header::new_ustar();
        header.as_old_mut().name[..name.len()].copy_from_slice(name);
        header.set_entry_type(kind);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(data);
        out.resize(out.len().next_multiple_of(512), 0);
    }

    /// Lays out an archive with these entries, then the usual end marker.
    fn raw_archive(entries: &[(&[u8], EntryType, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, kind, data) in entries {
            raw_entry(&mut out, name, *kind, data);
        }
        out.resize(out.len() + 1024, 0);
        out
    }

    fn valid_entries<'a>(
        index: &'a [u8],
        manifest: &'a [u8],
        component: &'a [u8],
    ) -> Vec<(&'a [u8], EntryType, &'a [u8])> {
        vec![
            (b"package.toml", EntryType::Regular, index),
            (b"agent.toml", EntryType::Regular, manifest),
            (b"agent.wasm", EntryType::Regular, component),
        ]
    }

    fn read(bytes: &[u8]) -> Result<Package, PackageError> {
        Package::read(Cursor::new(bytes))
    }

    /// Offset of a file's contents in a package made by `Package::write`.
    fn find(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
            .unwrap()
    }

    struct Fixture {
        root: tempfile::TempDir,
        storage: AgentStorage,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let storage = AgentStorage::open(root.path(), "research-helper").unwrap();
            Self { root, storage }
        }

        fn installed() -> Self {
            let fixture = Self::new();
            package().install(&fixture.storage).unwrap();
            fixture
        }

        /// Makes the files writable, as anything tampering with them would.
        fn unlock(&self, name: &str) {
            let path = self.storage.package_path().join(name);
            #[cfg(unix)]
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
            #[cfg(windows)]
            {
                let mut permissions = fs::metadata(&path).unwrap().permissions();
                #[allow(clippy::permissions_set_readonly_false, reason = "only for this test")]
                permissions.set_readonly(false);
                fs::set_permissions(path, permissions).unwrap();
            }
        }
    }

    #[test]
    fn a_package_can_be_built_read_and_unpacked() {
        let original = package();
        let read_back = read(&archive()).unwrap();
        assert_eq!(read_back.hash(), original.hash());
        assert_eq!(read_back.manifest().name(), "research-helper");
        assert_eq!(read_back.component(), component());
        assert_eq!(*read_back.shared_component(), component());

        let fixture = Fixture::new();
        read_back.install(&fixture.storage).unwrap();
        let folder = fixture.storage.package_path();
        assert_eq!(fs::read(folder.join("agent.wasm")).unwrap(), component());
        assert_eq!(
            fs::read_to_string(folder.join("agent.toml")).unwrap(),
            EXAMPLE
        );

        let loaded = Package::load_installed(&fixture.storage).unwrap();
        assert_eq!(loaded.hash(), original.hash());
        assert_eq!(loaded.component(), component());
    }

    #[test]
    fn the_same_contents_give_the_same_bytes_and_hash() {
        assert_eq!(archive(), archive());
        assert_eq!(package().hash(), package().hash());
    }

    #[test]
    fn the_hash_covers_every_file() {
        let mut other_code = component();
        other_code.push(0);
        let changed_code = Package::new(EXAMPLE, other_code).unwrap();
        assert_ne!(changed_code.hash(), package().hash());

        let changed_manifest = format!("{EXAMPLE}\n# changed\n");
        let changed_manifest = Package::new(&changed_manifest, component()).unwrap();
        assert_ne!(changed_manifest.hash(), package().hash());
    }

    #[test]
    fn a_package_holds_exactly_three_files() {
        let bytes = archive();
        let mut archive = Archive::new(Cursor::new(&bytes));
        let names: Vec<_> = archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path_bytes().into_owned())
            .collect();
        assert_eq!(
            names,
            [&b"package.toml"[..], b"agent.toml", b"agent.wasm"].map(|name| name.to_vec())
        );
    }

    #[test]
    fn changing_a_file_inside_a_package_is_detected() {
        let mut bytes = archive();
        let at = find(&bytes, b"pretend this is agent code");
        bytes[at] ^= 1;
        assert_eq!(
            read(&bytes).unwrap_err(),
            PackageError::HashMismatch { file: "agent.wasm" }
        );

        let mut bytes = archive();
        let at = find(&bytes, b"research-helper");
        bytes[at] ^= 1;
        assert_eq!(
            read(&bytes).unwrap_err(),
            PackageError::HashMismatch { file: "agent.toml" }
        );
    }

    #[test]
    fn changing_a_recorded_hash_is_detected() {
        let wasm = component();
        let mut index = index_for(EXAMPLE.as_bytes(), &wasm);
        let at = find(&index, b"agent.wasm") + "agent.wasm\" = \"".len();
        index[at] = if index[at] == b'0' { b'1' } else { b'0' };
        let bytes = raw_archive(&valid_entries(&index, EXAMPLE.as_bytes(), &wasm));
        assert_eq!(
            read(&bytes).unwrap_err(),
            PackageError::HashMismatch { file: "agent.wasm" }
        );
    }

    #[test]
    fn unsafe_entry_names_are_rejected() {
        let wasm = component();
        let index = index_for(EXAMPLE.as_bytes(), &wasm);
        for name in [
            &b"../agent.wasm"[..],
            b"../../../../etc/passwd",
            b"/agent.wasm",
            b"/etc/passwd",
            b"./agent.wasm",
            b"a/../agent.wasm",
            b"agent.wasm/",
            b"home/agent.wasm",
            b"agent.wasm ",
            b"AGENT.WASM",
            b"agent.wasm\xff",
        ] {
            let mut entries = valid_entries(&index, EXAMPLE.as_bytes(), &wasm);
            entries[2].0 = name;
            assert_eq!(
                read(&raw_archive(&entries)).unwrap_err(),
                PackageError::Malformed("it has an unexpected entry"),
                "{}",
                String::from_utf8_lossy(name)
            );
        }
    }

    #[test]
    fn a_traversal_name_in_the_ustar_prefix_is_rejected() {
        let wasm = component();
        let index = index_for(EXAMPLE.as_bytes(), &wasm);
        let mut out = Vec::new();
        raw_entry(&mut out, b"package.toml", EntryType::Regular, &index);
        raw_entry(
            &mut out,
            b"agent.toml",
            EntryType::Regular,
            EXAMPLE.as_bytes(),
        );

        let mut header = Header::new_ustar();
        header.as_old_mut().name[..10].copy_from_slice(b"agent.wasm");
        header.as_ustar_mut().unwrap().prefix[..5].copy_from_slice(b"../..");
        header.set_entry_type(EntryType::Regular);
        header.set_size(wasm.len() as u64);
        header.set_cksum();
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(&wasm);
        out.resize(out.len().next_multiple_of(512) + 1024, 0);

        assert_eq!(
            read(&out).unwrap_err(),
            PackageError::Malformed("it has an unexpected entry")
        );
    }

    #[test]
    fn entries_that_are_not_files_are_rejected() {
        let wasm = component();
        let index = index_for(EXAMPLE.as_bytes(), &wasm);
        for kind in [
            EntryType::Symlink,
            EntryType::Link,
            EntryType::Directory,
            EntryType::Char,
            EntryType::Block,
            EntryType::Fifo,
            EntryType::Continuous,
            EntryType::GNUSparse,
        ] {
            let mut entries = valid_entries(&index, EXAMPLE.as_bytes(), &wasm);
            entries[2].1 = kind;
            assert_eq!(
                read(&raw_archive(&entries)).unwrap_err(),
                PackageError::Malformed("it has an entry that isn't a file"),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn long_name_and_extended_header_entries_are_rejected() {
        let wasm = component();
        let index = index_for(EXAMPLE.as_bytes(), &wasm);
        // These change the name of the entry after them, which could hide a traversal.
        for (kind, data) in [
            (EntryType::GNULongName, &b"../../agent.wasm\0"[..]),
            (EntryType::XHeader, b"26 path=../../agent.wasm\n"),
            (EntryType::XGlobalHeader, b"26 path=../../agent.wasm\n"),
        ] {
            let mut entries = valid_entries(&index, EXAMPLE.as_bytes(), &wasm);
            entries.insert(2, (b"././@LongLink", kind, data));
            assert_eq!(
                read(&raw_archive(&entries)).unwrap_err(),
                PackageError::Malformed("it has an entry that isn't a file"),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn missing_repeated_extra_and_reordered_files_are_rejected() {
        let wasm = component();
        let index = index_for(EXAMPLE.as_bytes(), &wasm);
        let manifest = EXAMPLE.as_bytes();
        let valid = valid_entries(&index, manifest, &wasm);

        for count in 0..3 {
            assert_eq!(
                read(&raw_archive(&valid[..count])).unwrap_err(),
                PackageError::Malformed("it's missing a file"),
            );
        }

        let mut reordered = valid.clone();
        reordered.swap(1, 2);
        assert_eq!(
            read(&raw_archive(&reordered)).unwrap_err(),
            PackageError::Malformed("it has an unexpected entry")
        );

        let mut repeated = valid.clone();
        repeated.push(valid[2]);
        assert_eq!(
            read(&raw_archive(&repeated)).unwrap_err(),
            PackageError::Malformed("it has an extra entry")
        );

        let mut extra = valid.clone();
        extra.push((b"secret.sh", EntryType::Regular, b"x"));
        assert_eq!(
            read(&raw_archive(&extra)).unwrap_err(),
            PackageError::Malformed("it has an extra entry")
        );
    }

    /// Counts what's read, to show that oversized files are rejected before they're read.
    struct Counting<R> {
        inner: R,
        count: std::rc::Rc<std::cell::Cell<u64>>,
    }

    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = self.inner.read(buffer)?;
            self.count.set(self.count.get() + count as u64);
            Ok(count)
        }
    }

    #[test]
    fn oversized_files_are_rejected_without_reading_them() {
        let manifest = EXAMPLE.as_bytes();
        let wasm = component();
        let index = index_for(manifest, &wasm);

        let header_only = |prefix: Vec<u8>, name: &[u8], size: u64| {
            let mut out = prefix;
            let before = out.len() as u64;
            let mut header = Header::new_ustar();
            header.as_old_mut().name[..name.len()].copy_from_slice(name);
            header.set_entry_type(EntryType::Regular);
            header.set_size(size);
            header.set_cksum();
            out.extend_from_slice(header.as_bytes());

            let count = std::rc::Rc::new(std::cell::Cell::new(0));
            // The claimed body really is there, as endless data.
            let reader = Counting {
                inner: Cursor::new(out).chain(io::repeat(b'x')),
                count: count.clone(),
            };
            let error = Package::read(reader).unwrap_err();
            assert!(count.get() < before + 64 * 1024 + 512, "{}", count.get());
            error
        };

        assert_eq!(
            header_only(Vec::new(), b"package.toml", MAX_INDEX_BYTES + 1),
            PackageError::TooLarge {
                file: "package.toml",
                limit: MAX_INDEX_BYTES
            }
        );

        let mut prefix = Vec::new();
        raw_entry(&mut prefix, b"package.toml", EntryType::Regular, &index);
        assert_eq!(
            header_only(prefix.clone(), b"agent.toml", MAX_MANIFEST + 1),
            PackageError::TooLarge {
                file: "agent.toml",
                limit: MAX_MANIFEST
            }
        );

        raw_entry(&mut prefix, b"agent.toml", EntryType::Regular, manifest);
        for size in [MAX_COMPONENT_BYTES + 1, u64::MAX / 2] {
            assert_eq!(
                header_only(prefix.clone(), b"agent.wasm", size),
                PackageError::TooLarge {
                    file: "agent.wasm",
                    limit: MAX_COMPONENT_BYTES
                }
            );
        }
    }

    #[test]
    fn files_at_the_limit_are_accepted() {
        let wasm = component();
        let mut manifest = EXAMPLE.to_string();
        manifest.push_str(&"#".repeat(MAX_MANIFEST_BYTES - manifest.len()));
        assert!(Package::new(&manifest, wasm.clone()).is_ok());

        let mut index = index_for(manifest.as_bytes(), &wasm);
        index.resize(MAX_INDEX_BYTES as usize, b'#');
        let bytes = raw_archive(&valid_entries(&index, manifest.as_bytes(), &wasm));
        assert!(read(&bytes).is_ok());
    }

    #[test]
    fn an_oversized_component_cant_be_packaged() {
        let code = vec![0; MAX_COMPONENT_BYTES as usize + 1];
        assert_eq!(
            Package::new(EXAMPLE, code).unwrap_err(),
            PackageError::TooLarge {
                file: "agent.wasm",
                limit: MAX_COMPONENT_BYTES
            }
        );
    }

    #[test]
    fn data_after_the_last_entry_is_rejected() {
        let mut bytes = archive();
        bytes.extend_from_slice(b"hidden");
        assert_eq!(
            read(&bytes).unwrap_err(),
            PackageError::Malformed("it has data after its last entry")
        );

        // Hidden behind enough zeros to look like more padding.
        let mut bytes = archive();
        bytes.resize(bytes.len() + 200 * 1024, 0);
        bytes.push(1);
        assert_eq!(
            read(&bytes).unwrap_err(),
            PackageError::Malformed("it has data after its last entry")
        );

        // Some tools pad archives with zeros, which is harmless.
        let mut bytes = archive();
        bytes.resize(bytes.len() + 10 * 1024, 0);
        assert!(read(&bytes).is_ok());
    }

    #[test]
    fn endless_zeros_after_the_last_entry_are_rejected() {
        let result = Package::read(Cursor::new(archive()).chain(io::repeat(0)));
        assert_eq!(
            result.unwrap_err(),
            PackageError::TooLarge {
                file: "package",
                limit: MAX_ARCHIVE_BYTES
            }
        );
    }

    #[test]
    fn cut_short_or_corrupted_archives_are_rejected() {
        let bytes = archive();
        let end_of_files = find(&bytes, b"pretend this is agent code") + 26;
        for length in (0..end_of_files).step_by(97).chain([end_of_files - 1]) {
            assert!(read(&bytes[..length]).is_err(), "{length}");
        }

        for at in [0, 100, 124, 148, 156, 257] {
            let mut corrupted = bytes.clone();
            corrupted[at] ^= 0xff;
            assert!(read(&corrupted).is_err(), "{at}");
        }

        assert!(read(b"").is_err());
        assert!(read(&[0xff; 4096]).is_err());
    }

    #[test]
    fn a_file_shorter_than_its_header_claims_is_rejected() {
        // The hashes match what's really there, so only the size gives this away.
        let manifest = EXAMPLE.as_bytes();
        let wasm = component();
        let index = index_for(manifest, &wasm);
        let mut out = Vec::new();
        raw_entry(&mut out, b"package.toml", EntryType::Regular, &index);
        raw_entry(&mut out, b"agent.toml", EntryType::Regular, manifest);

        let mut header = Header::new_ustar();
        header.as_old_mut().name[..10].copy_from_slice(b"agent.wasm");
        header.set_entry_type(EntryType::Regular);
        header.set_size(wasm.len() as u64 + 1);
        header.set_cksum();
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(&wasm);

        assert_eq!(
            read(&out).unwrap_err(),
            PackageError::Malformed("it's cut short")
        );
    }

    #[test]
    fn invalid_indexes_are_rejected() {
        let manifest = EXAMPLE.as_bytes();
        let wasm = component();
        let good = String::from_utf8(index_for(manifest, &wasm)).unwrap();

        let upper = good
            .to_uppercase()
            .replace("FORMAT", "format")
            .replace("[FILES]", "[files]");
        let upper = upper
            .replace("AGENT.TOML", "agent.toml")
            .replace("AGENT.WASM", "agent.wasm");
        let cases: [(Vec<u8>, PackageError); 9] = [
            (b"\xff\xfe".to_vec(), BAD_INDEX),
            (b"not toml".to_vec(), BAD_INDEX),
            (good.replace("format = 1\n", "").into_bytes(), BAD_INDEX),
            (format!("{good}surprise = true\n").into_bytes(), BAD_INDEX),
            (
                good.replace("format = 1\n", "format = 1\nsurprise = true\n")
                    .into_bytes(),
                BAD_INDEX,
            ),
            (
                format!("{good}\"secret.sh\" = \"00\"\n").into_bytes(),
                BAD_INDEX,
            ),
            (
                good.replace("format = 1", "format = 2").into_bytes(),
                PackageError::UnsupportedFormat,
            ),
            (
                good.replace("format = 1", "format = 0").into_bytes(),
                PackageError::UnsupportedFormat,
            ),
            (
                upper.into_bytes(),
                PackageError::HashMismatch { file: "agent.toml" },
            ),
        ];
        for (index, expected) in cases {
            let bytes = raw_archive(&valid_entries(&index, manifest, &wasm));
            assert_eq!(
                read(&bytes).unwrap_err(),
                expected,
                "{}",
                String::from_utf8_lossy(&index)
            );
        }
    }

    #[test]
    fn a_later_format_is_reported_as_unsupported_even_with_new_fields() {
        let manifest = EXAMPLE.as_bytes();
        let wasm = component();
        let index = b"format = 2\n\n[signature]\nkey = \"abc\"\n".to_vec();
        let bytes = raw_archive(&valid_entries(&index, manifest, &wasm));
        assert_eq!(read(&bytes).unwrap_err(), PackageError::UnsupportedFormat);
    }

    #[test]
    fn invalid_manifests_are_rejected() {
        let wasm = component();
        let error = Package::new("[agent]\nname = \"BAD\"\n", wasm.clone()).unwrap_err();
        assert!(matches!(error, PackageError::Manifest(_)), "{error:?}");

        let manifest = b"\xff\xfe";
        let index = index_for(manifest, &wasm);
        assert_eq!(
            read(&raw_archive(&valid_entries(&index, manifest, &wasm))).unwrap_err(),
            PackageError::Malformed("agent.toml isn't UTF-8")
        );
    }

    #[test]
    fn only_webassembly_components_are_accepted() {
        for code in [
            &b""[..],
            b"\0asm",
            b"\0asm\x01\0\0\0",
            b"\x7fELF\x02\x01\x01\0",
            b"\xcf\xfa\xed\xfe\x0c\0\0\x01",
        ] {
            assert_eq!(
                Package::new(EXAMPLE, code.to_vec()).unwrap_err(),
                PackageError::Malformed("agent.wasm isn't a WebAssembly component"),
            );
        }
    }

    #[test]
    fn a_package_only_installs_into_its_own_agents_folder() {
        let fixture = Fixture::new();
        let other = Package::new(OTHER_AGENT, component()).unwrap();
        assert_eq!(
            other.install(&fixture.storage),
            Err(PackageError::WrongAgent)
        );
        assert_eq!(
            Package::load_installed(&fixture.storage).unwrap_err(),
            PackageError::NotInstalled
        );
    }

    #[test]
    fn a_package_moved_to_another_agents_folder_is_rejected() {
        let fixture = Fixture::installed();
        let other = AgentStorage::open(fixture.root.path(), "other-agent").unwrap();
        fs::remove_dir_all(other.package_path()).unwrap();
        fs::rename(fixture.storage.package_path(), other.package_path()).unwrap();
        fs::rename(
            fixture.storage.package_hash_path(),
            other.package_hash_path(),
        )
        .unwrap();
        assert_eq!(
            Package::load_installed(&other).unwrap_err(),
            PackageError::WrongAgent
        );
    }

    #[test]
    fn installing_twice_is_refused_and_keeps_the_first_package() {
        let fixture = Fixture::installed();
        let hash = package().hash();

        let mut other_code = component();
        other_code.push(0);
        let other = Package::new(EXAMPLE, other_code).unwrap();
        assert_eq!(
            other.install(&fixture.storage),
            Err(PackageError::AlreadyInstalled)
        );
        assert_eq!(
            Package::load_installed(&fixture.storage).unwrap().hash(),
            hash
        );
    }

    #[test]
    fn an_interrupted_install_counts_as_not_installed_and_can_be_repeated() {
        let fixture = Fixture::new();
        // The install stopped at some point before recording the hash.
        let folder = fixture.storage.package_path();
        fs::write(folder.join("package.toml"), b"partial").unwrap();
        fs::write(folder.join("left-behind"), b"partial").unwrap();
        fs::write(
            fixture.storage.package_hash_path().with_extension("new"),
            b"partial",
        )
        .unwrap();
        assert_eq!(
            Package::load_installed(&fixture.storage).unwrap_err(),
            PackageError::NotInstalled
        );

        package().install(&fixture.storage).unwrap();
        assert_eq!(
            Package::load_installed(&fixture.storage).unwrap().hash(),
            package().hash()
        );
        assert!(!folder.join("left-behind").exists());
    }

    #[test]
    fn installing_leaves_the_agents_home_alone() {
        let fixture = Fixture::new();
        let mut home = fixture.storage.home(1024 * 1024).unwrap();
        home.write("notes.txt", b"mine").unwrap();
        package().install(&fixture.storage).unwrap();
        assert_eq!(home.read("notes.txt").unwrap(), b"mine");
        // The package isn't visible from inside the home.
        assert_eq!(home.read("agent.wasm"), Err(HomeError::NotFound));
    }

    #[cfg(unix)]
    #[test]
    fn installed_files_are_private_and_read_only() {
        let fixture = Fixture::installed();
        let folder = fixture.storage.package_path();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&folder), 0o700);
        for name in ["package.toml", "agent.toml", "agent.wasm"] {
            assert_eq!(mode(&folder.join(name)), 0o400, "{name}");
        }
        assert_eq!(mode(&fixture.storage.package_hash_path()), 0o600);
    }

    #[test]
    fn changing_an_installed_file_is_detected() {
        for (name, expected) in [("agent.wasm", "agent.wasm"), ("agent.toml", "agent.toml")] {
            let fixture = Fixture::installed();
            fixture.unlock(name);
            let path = fixture.storage.package_path().join(name);
            let mut data = fs::read(&path).unwrap();
            data.push(b'\n');
            fs::write(&path, data).unwrap();
            assert_eq!(
                Package::load_installed(&fixture.storage).unwrap_err(),
                PackageError::HashMismatch { file: expected },
            );
        }
    }

    #[test]
    fn replacing_a_whole_installed_package_is_detected() {
        // Everything in the package folder is consistent, but it isn't what was installed.
        let fixture = Fixture::installed();
        let folder = fixture.storage.package_path();
        let mut other_code = component();
        other_code.push(0);
        let other = Package::new(EXAMPLE, other_code).unwrap();
        for (name, data) in other.files() {
            fixture.unlock(name);
            fs::write(folder.join(name), data).unwrap();
        }
        assert_eq!(
            Package::load_installed(&fixture.storage).unwrap_err(),
            PackageError::HashMismatch {
                file: "package.toml"
            }
        );
    }

    #[test]
    fn a_changed_or_missing_record_is_detected() {
        let fixture = Fixture::installed();
        let record = fixture.storage.package_hash_path();
        for bad in ["", "abc\n", "not a hash"] {
            fs::write(&record, bad).unwrap();
            assert_eq!(
                Package::load_installed(&fixture.storage).unwrap_err(),
                PackageError::HashMismatch {
                    file: "package.toml"
                }
            );
        }
        fs::remove_file(&record).unwrap();
        assert_eq!(
            Package::load_installed(&fixture.storage).unwrap_err(),
            PackageError::NotInstalled
        );
    }

    #[test]
    fn extra_or_missing_installed_files_are_detected() {
        let fixture = Fixture::installed();
        let folder = fixture.storage.package_path();
        fs::write(folder.join("extra.wasm"), b"x").unwrap();
        assert_eq!(
            Package::load_installed(&fixture.storage).unwrap_err(),
            PackageError::Malformed("the package folder has extra files")
        );

        let fixture = Fixture::installed();
        let folder = fixture.storage.package_path();
        fs::remove_file(folder.join("agent.wasm")).unwrap();
        assert_eq!(
            Package::load_installed(&fixture.storage).unwrap_err(),
            PackageError::Malformed("the package folder is missing files")
        );
    }

    #[test]
    fn installed_files_replaced_by_symlinks_are_denied() {
        let fixture = Fixture::installed();
        let outside = fixture.root.path().join("outside.wasm");
        fs::write(&outside, component()).unwrap();
        let path = fixture.storage.package_path().join("agent.wasm");
        fs::remove_file(&path).unwrap();
        symlink(&outside, &path).unwrap();
        assert_eq!(
            Package::load_installed(&fixture.storage).unwrap_err(),
            PackageError::Malformed("an installed file isn't a regular file")
        );
    }

    #[test]
    fn a_symlinked_record_is_denied_and_blocks_installing() {
        let fixture = Fixture::new();
        let outside = fixture.root.path().join("outside");
        fs::write(&outside, format!("{}\n", package().hash())).unwrap();
        symlink(&outside, fixture.storage.package_hash_path()).unwrap();
        assert_eq!(
            Package::load_installed(&fixture.storage).unwrap_err(),
            PackageError::Malformed("an installed file isn't a regular file")
        );
        assert_eq!(
            package().install(&fixture.storage),
            Err(PackageError::AlreadyInstalled)
        );
    }

    #[test]
    fn oversized_installed_files_are_rejected() {
        let fixture = Fixture::installed();
        fixture.unlock("package.toml");
        let path = fixture.storage.package_path().join("package.toml");
        fs::write(path, vec![b'#'; MAX_INDEX_BYTES as usize + 1]).unwrap();
        assert_eq!(
            Package::load_installed(&fixture.storage).unwrap_err(),
            PackageError::Malformed("an installed file is too large")
        );
    }

    #[cfg(unix)]
    #[test]
    #[cfg_attr(
        target_os = "android",
        ignore = "Android's security policy stops this test from creating a named pipe"
    )]
    fn a_named_pipe_in_the_package_folder_is_denied_without_hanging() {
        let fixture = Fixture::installed();
        let path = fixture.storage.package_path().join("agent.wasm");
        fs::remove_file(&path).unwrap();
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path is a valid C string that lives for the whole call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = Package::load_installed(&fixture.storage).map(|package| package.hash());
            sender.send(result).unwrap();
        });
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(10)),
            Ok(Err(PackageError::Malformed(
                "an installed file isn't a regular file"
            )))
        );
    }

    #[test]
    fn errors_do_not_include_host_paths_or_package_text() {
        let fixture = Fixture::new();
        let root = fixture.root.path().to_string_lossy().into_owned();

        let wasm = component();
        let index = index_for(EXAMPLE.as_bytes(), &wasm);
        let mut entries = valid_entries(&index, EXAMPLE.as_bytes(), &wasm);
        entries[2].0 = b"\x1b[31mevil";

        let errors = [
            Package::load_installed(&fixture.storage).unwrap_err(),
            read(&raw_archive(&entries)).unwrap_err(),
            read(b"\x1b[31mevil").unwrap_err(),
        ];
        for error in errors {
            let message = error.to_string();
            assert!(!message.contains(&root), "{message}");
            assert!(!message.contains("evil"), "{message}");
            assert!(message.chars().all(|c| c.is_ascii() && !c.is_control()));
        }
    }
}
