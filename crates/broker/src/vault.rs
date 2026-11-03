//! Keeps API keys in the operating system's keystore, and out of every agent's reach.
//!
//! No part of the agent API can get at this. Only the core can turn a secret into a header for a
//! request that it's making itself, and only for a host that the secret is bound to. A secret's
//! value is in its own type, which can't be printed, so it can't end up in a log or error by
//! accident. What's kept in a file is only its name and where it can be sent.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use habitat_core::manifest::is_host_name;
use habitat_core::platform;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const VAULT_FILE: &str = "secrets.toml";
const LOCK_FILE: &str = "secrets.lock";
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_NAME_BYTES: usize = 64;
const MAX_HOSTS: usize = 64;
const MAX_VALUE_BYTES: usize = 16 * 1024;
const MAX_PREFIX_BYTES: usize = 64;

/// A secret's value. It can't be printed, and is wiped from memory when it's dropped. Only code in
/// this crate can see what's in it.
pub struct SecretValue(Zeroizing<Vec<u8>>);

impl SecretValue {
    pub fn new(value: Vec<u8>) -> Self {
        Self(Zeroizing::new(value))
    }

    pub(crate) fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[hidden]")
    }
}

/// A header for the core to add to a request that it's making. This can't be printed either.
#[derive(Debug)]
pub struct SecretHeader {
    pub(crate) name: String,
    pub(crate) value: SecretValue,
}

#[cfg(test)]
impl SecretHeader {
    pub(crate) fn name_for_tests(&self) -> &str {
        &self.name
    }

    pub(crate) fn value_for_tests(&self) -> &[u8] {
        self.value.expose()
    }
}

/// Why something in the vault didn't work. This holds no values and no host paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultError {
    InvalidName,
    InvalidHost,
    /// A header that it isn't safe to send a secret in.
    InvalidHeader,
    InvalidValue,
    NotFound,
    AlreadyExists,
    /// This secret can't be sent to that host.
    HostNotAllowed,
    /// The record of what's in the vault isn't valid.
    Corrupt,
    /// The system's keystore isn't there, or refused.
    KeystoreUnavailable,
    Io(io::ErrorKind),
}

impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => write!(
                f,
                "a secret's name can only use lowercase letters, digits and '-', up to 64 of them"
            ),
            Self::InvalidHost => write!(f, "that isn't a valid host name"),
            Self::InvalidHeader => write!(f, "that isn't a valid header to send a secret in"),
            Self::InvalidValue => write!(
                f,
                "a secret has to be between 1 byte and 16 KB, and can't hold line breaks"
            ),
            Self::NotFound => write!(f, "there's no secret with that name"),
            Self::AlreadyExists => write!(f, "there's already a secret with that name"),
            Self::HostNotAllowed => write!(f, "that secret can't be sent to that host"),
            Self::Corrupt => write!(f, "the record of secrets isn't valid"),
            Self::KeystoreUnavailable => {
                write!(f, "the system's keystore isn't available, or it refused")
            }
            Self::Io(kind) => write!(f, "file system error: {kind}"),
        }
    }
}

impl std::error::Error for VaultError {}

impl From<io::Error> for VaultError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

/// Something that can hold secrets' values. The real one is the system's keystore. Nothing in here
/// can fall back to a file, so without a keystore, there are no secrets.
pub trait SecretStore: Send + Sync {
    fn put(&self, name: &str, value: &SecretValue) -> Result<(), VaultError>;
    fn get(&self, name: &str) -> Result<SecretValue, VaultError>;
    fn delete(&self, name: &str) -> Result<(), VaultError>;
}

/// The keychain on a Mac, Credential Manager on Windows and the Secret Service on Linux.
#[cfg(not(target_os = "android"))]
#[derive(Debug, Default)]
pub struct OsKeystore;

#[cfg(not(target_os = "android"))]
impl OsKeystore {
    const SERVICE: &'static str = "habitat";

    fn entry(name: &str) -> Result<keyring::v1::Entry, VaultError> {
        keyring::v1::Entry::new(Self::SERVICE, name).map_err(|_| VaultError::KeystoreUnavailable)
    }
}

#[cfg(not(target_os = "android"))]
impl SecretStore for OsKeystore {
    fn put(&self, name: &str, value: &SecretValue) -> Result<(), VaultError> {
        Self::entry(name)?
            .set_secret(value.expose())
            .map_err(|_| VaultError::KeystoreUnavailable)
    }

    fn get(&self, name: &str) -> Result<SecretValue, VaultError> {
        Self::entry(name)?
            .get_secret()
            .map(SecretValue::new)
            .map_err(|_| VaultError::KeystoreUnavailable)
    }

    fn delete(&self, name: &str) -> Result<(), VaultError> {
        Self::entry(name)?
            .delete_credential()
            .map_err(|_| VaultError::KeystoreUnavailable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedSecret {
    hosts: BTreeSet<String>,
    header: String,
    #[serde(default)]
    prefix: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedVault {
    #[serde(default)]
    secrets: BTreeMap<String, SavedSecret>,
}

/// What's known about a secret, without its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretInfo {
    pub name: String,
    pub hosts: Vec<String>,
    pub header: String,
}

/// What to add to the vault.
#[derive(Debug)]
pub struct NewSecret<'a> {
    pub name: &'a str,
    /// The only hosts this can ever be sent to.
    pub hosts: &'a [String],
    /// The header to send it in, such as `x-api-key`.
    pub header: &'a str,
    /// What goes before it in the header, such as `Bearer `.
    pub prefix: &'a str,
    pub value: SecretValue,
}

fn is_secret_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// A secret can only go in a header meant for credentials, so that it can't be set up to go in one
/// that a server would echo back, or log.
fn is_credential_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "x-api-key" | "api-key"
    )
}

fn is_header_safe(bytes: &[u8]) -> bool {
    bytes.iter().all(|byte| (0x20..0x7f).contains(byte))
}

/// All the secrets on this host. They aren't per agent, because it's the user who has an API key.
#[derive(Clone)]
pub struct Vault {
    file: PathBuf,
    lock: PathBuf,
    store: Arc<dyn SecretStore>,
}

impl Vault {
    /// Opens the vault for the agents under `root`, which is chosen by the host and is trusted.
    pub fn new(root: &Path, store: impl SecretStore + 'static) -> Self {
        Self {
            file: root.join(VAULT_FILE),
            lock: root.join(LOCK_FILE),
            store: Arc::new(store),
        }
    }

    fn read(&self) -> Result<SavedVault, VaultError> {
        let file = match platform::open_no_follow(&self.file) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(SavedVault::default());
            }
            other => other?,
        };
        if !file.metadata()?.is_file() {
            return Err(VaultError::Corrupt);
        }
        let mut text = String::new();
        file.take(MAX_FILE_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(|_| VaultError::Corrupt)?;
        if text.len() as u64 > MAX_FILE_BYTES {
            return Err(VaultError::Corrupt);
        }
        let saved: SavedVault = toml::from_str(&text).map_err(|_| VaultError::Corrupt)?;
        let valid = saved.secrets.iter().all(|(name, secret)| {
            is_secret_name(name)
                && is_credential_header(&secret.header)
                && is_header_safe(secret.prefix.as_bytes())
                && secret.hosts.iter().all(|host| is_host_name(host))
        });
        if valid {
            Ok(saved)
        } else {
            Err(VaultError::Corrupt)
        }
    }

    fn write(&self, saved: &SavedVault) -> Result<(), VaultError> {
        let text = toml::to_string(saved).map_err(|_| VaultError::Corrupt)?;
        let temp = self.file.with_extension("new");
        if temp.symlink_metadata().is_ok() {
            fs::remove_file(&temp)?;
        }
        platform::write_new_file(&temp, text.as_bytes(), false)?;
        fs::rename(&temp, &self.file)?;
        Ok(())
    }

    /// Makes one change at a time, even across processes.
    fn locked<T>(&self, change: impl FnOnce() -> Result<T, VaultError>) -> Result<T, VaultError> {
        if let Some(parent) = self.file.parent() {
            platform::create_private_dirs(parent)?;
        }
        let lock = platform::open_or_create_no_follow(&self.lock)?;
        if !lock.metadata()?.is_file() {
            return Err(VaultError::Corrupt);
        }
        lock.lock()?;
        change()
    }

    /// Adds a secret. The value goes in the keystore, and only the rest goes in a file.
    pub fn add(&self, secret: NewSecret) -> Result<(), VaultError> {
        if !is_secret_name(secret.name) {
            return Err(VaultError::InvalidName);
        }
        if secret.hosts.is_empty()
            || secret.hosts.len() > MAX_HOSTS
            || !secret.hosts.iter().all(|host| is_host_name(host))
        {
            return Err(VaultError::InvalidHost);
        }
        if !is_credential_header(secret.header)
            || secret.prefix.len() > MAX_PREFIX_BYTES
            || !is_header_safe(secret.prefix.as_bytes())
        {
            return Err(VaultError::InvalidHeader);
        }
        let value = secret.value.expose();
        if value.is_empty() || value.len() > MAX_VALUE_BYTES || !is_header_safe(value) {
            return Err(VaultError::InvalidValue);
        }
        self.locked(|| {
            let mut saved = self.read()?;
            if saved.secrets.contains_key(secret.name) {
                return Err(VaultError::AlreadyExists);
            }
            self.store.put(secret.name, &secret.value)?;
            saved.secrets.insert(
                secret.name.to_string(),
                SavedSecret {
                    hosts: secret.hosts.iter().cloned().collect(),
                    header: secret.header.to_ascii_lowercase(),
                    prefix: secret.prefix.to_string(),
                },
            );
            if let Err(error) = self.write(&saved) {
                // A value with no record of it could never be used or removed.
                let _ = self.store.delete(secret.name);
                return Err(error);
            }
            Ok(())
        })
    }

    /// Lists what's in the vault, without any values.
    pub fn list(&self) -> Result<Vec<SecretInfo>, VaultError> {
        Ok(self
            .read()?
            .secrets
            .into_iter()
            .map(|(name, secret)| SecretInfo {
                name,
                hosts: secret.hosts.into_iter().collect(),
                header: secret.header,
            })
            .collect())
    }

    /// Removes a secret, with its value.
    pub fn remove(&self, name: &str) -> Result<(), VaultError> {
        if !is_secret_name(name) {
            return Err(VaultError::InvalidName);
        }
        self.locked(|| {
            let mut saved = self.read()?;
            if saved.secrets.remove(name).is_none() {
                return Err(VaultError::NotFound);
            }
            // The record goes first. If this stops here, the value can't be used by anything, and
            // removing it failing is reported, rather than leaving something that seems to work.
            self.write(&saved)?;
            self.store.delete(name)
        })
    }

    /// Makes the header for a request to a host, if this secret can go there. This is the only way
    /// that a value leaves here, and it isn't reachable from the agent API, as `pub(crate)` and the
    /// test below make sure of.
    pub(crate) fn header_for(&self, name: &str, host: &str) -> Result<SecretHeader, VaultError> {
        if !is_secret_name(name) {
            return Err(VaultError::InvalidName);
        }
        let saved = self.read()?;
        let secret = saved.secrets.get(name).ok_or(VaultError::NotFound)?;
        // An exact match, so `api.example.com` doesn't cover `api.example.com.evil.example`.
        if !secret.hosts.contains(host) {
            return Err(VaultError::HostNotAllowed);
        }
        let value = self.store.get(name)?;
        let mut header = Zeroizing::new(secret.prefix.clone().into_bytes());
        header.extend_from_slice(value.expose());
        Ok(SecretHeader {
            name: secret.header.clone(),
            value: SecretValue::new(header.to_vec()),
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use tempfile::TempDir;

    use super::*;

    pub(crate) const SECRET: &str = "sk-ant-THE-REAL-SECRET-VALUE";

    /// A keystore for tests, which can be made to fail.
    #[derive(Clone, Default)]
    pub(crate) struct FakeStore {
        pub(crate) values: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        pub(crate) unavailable: bool,
    }

    impl SecretStore for FakeStore {
        fn put(&self, name: &str, value: &SecretValue) -> Result<(), VaultError> {
            if self.unavailable {
                return Err(VaultError::KeystoreUnavailable);
            }
            self.values
                .lock()
                .unwrap()
                .insert(name.to_string(), value.expose().to_vec());
            Ok(())
        }

        fn get(&self, name: &str) -> Result<SecretValue, VaultError> {
            if self.unavailable {
                return Err(VaultError::KeystoreUnavailable);
            }
            self.values
                .lock()
                .unwrap()
                .get(name)
                .map(|value| SecretValue::new(value.clone()))
                .ok_or(VaultError::KeystoreUnavailable)
        }

        fn delete(&self, name: &str) -> Result<(), VaultError> {
            self.values.lock().unwrap().remove(name);
            Ok(())
        }
    }

    struct Fixture {
        root: TempDir,
        store: FakeStore,
        vault: Vault,
    }

    impl Fixture {
        fn new() -> Self {
            let root = TempDir::new().unwrap();
            let store = FakeStore::default();
            let vault = Vault::new(root.path(), store.clone());
            Self { root, store, vault }
        }

        fn add(&self, name: &str, hosts: &[&str]) -> Result<(), VaultError> {
            let hosts: Vec<String> = hosts.iter().map(ToString::to_string).collect();
            self.vault.add(NewSecret {
                name,
                hosts: &hosts,
                header: "x-api-key",
                prefix: "",
                value: SecretValue::new(SECRET.as_bytes().to_vec()),
            })
        }
    }

    #[test]
    fn a_secret_can_be_sent_to_its_host_and_no_other() {
        let fixture = Fixture::new();
        fixture.add("key", &["api.example.com"]).unwrap();
        let header = fixture.vault.header_for("key", "api.example.com").unwrap();
        assert_eq!(header.name, "x-api-key");
        assert_eq!(header.value.expose(), SECRET.as_bytes());
        for host in [
            "example.com",
            "other.example.com",
            "api.example.com.evil.example",
            "xapi.example.com",
            "API.EXAMPLE.COM",
            "",
        ] {
            assert_eq!(
                fixture.vault.header_for("key", host).unwrap_err(),
                VaultError::HostNotAllowed,
                "{host}"
            );
        }
    }

    #[test]
    fn a_prefix_goes_before_the_value() {
        let fixture = Fixture::new();
        let hosts = vec!["api.example.com".to_string()];
        fixture
            .vault
            .add(NewSecret {
                name: "key",
                hosts: &hosts,
                header: "Authorization",
                prefix: "Bearer ",
                value: SecretValue::new(b"abc".to_vec()),
            })
            .unwrap();
        let header = fixture.vault.header_for("key", "api.example.com").unwrap();
        assert_eq!(header.name, "authorization");
        assert_eq!(header.value.expose(), b"Bearer abc");
    }

    #[test]
    fn a_value_never_goes_in_a_file_or_in_anything_printed() {
        let fixture = Fixture::new();
        fixture.add("key", &["api.example.com"]).unwrap();
        for entry in fs::read_dir(fixture.root.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let text = fs::read(&path).unwrap();
                let text = String::from_utf8_lossy(&text);
                assert!(!text.contains(SECRET), "{path:?}");
            }
        }
        let header = fixture.vault.header_for("key", "api.example.com").unwrap();
        let value = SecretValue::new(SECRET.as_bytes().to_vec());
        let listed = fixture.vault.list().unwrap();
        for text in [
            format!("{header:?}"),
            format!("{value:?}"),
            format!("{listed:?}"),
        ] {
            assert!(!text.contains(SECRET), "{text}");
        }
        assert_eq!(format!("{value:?}"), "[hidden]");
    }

    #[test]
    fn errors_hold_no_values() {
        let fixture = Fixture::new();
        fixture.add("key", &["api.example.com"]).unwrap();
        let mut errors = vec![
            fixture.add("key", &["api.example.com"]).unwrap_err(),
            fixture.vault.header_for("key", "evil.example").unwrap_err(),
            fixture
                .vault
                .header_for("missing", "api.example.com")
                .unwrap_err(),
        ];
        errors.push(VaultError::Corrupt);
        errors.push(VaultError::KeystoreUnavailable);
        for error in errors {
            let text = format!("{error} {error:?}");
            assert!(!text.contains(SECRET), "{text}");
            assert!(!text.contains('/'), "{text}");
        }
    }

    #[test]
    fn secrets_can_be_listed_without_values_and_removed() {
        let fixture = Fixture::new();
        fixture.add("one", &["a.example", "b.example"]).unwrap();
        fixture.add("two", &["c.example"]).unwrap();
        let listed = fixture.vault.list().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "one");
        assert_eq!(listed[0].hosts, ["a.example", "b.example"]);
        assert_eq!(listed[0].header, "x-api-key");

        fixture.vault.remove("one").unwrap();
        assert_eq!(fixture.vault.list().unwrap().len(), 1);
        assert!(!fixture.store.values.lock().unwrap().contains_key("one"));
        assert_eq!(
            fixture.vault.header_for("one", "a.example").unwrap_err(),
            VaultError::NotFound
        );
        assert_eq!(fixture.vault.remove("one"), Err(VaultError::NotFound));
    }

    #[test]
    fn a_secret_isnt_replaced_by_accident() {
        let fixture = Fixture::new();
        fixture.add("key", &["a.example"]).unwrap();
        assert_eq!(
            fixture.add("key", &["evil.example"]),
            Err(VaultError::AlreadyExists)
        );
        assert_eq!(
            fixture.vault.header_for("key", "evil.example").unwrap_err(),
            VaultError::HostNotAllowed
        );
    }

    #[test]
    fn bad_input_is_refused() {
        let fixture = Fixture::new();
        for name in ["", "UPPER", "a b", "../x", "a/b", "a\nb"] {
            assert_eq!(
                fixture.add(name, &["a.example"]),
                Err(VaultError::InvalidName),
                "{name:?}"
            );
        }
        for hosts in [
            &[][..],
            &["localhost"][..],
            &["*.example.com"][..],
            &["127.0.0.1"][..],
            &["a.example", "BAD HOST"][..],
        ] {
            assert_eq!(
                fixture.add("key", hosts),
                Err(VaultError::InvalidHost),
                "{hosts:?}"
            );
        }
        let hosts = vec!["a.example".to_string()];
        for (header, prefix) in [
            ("host", ""),
            ("cookie", ""),
            ("x-custom", ""),
            ("referer", ""),
            ("authorization", "Bearer\r\nHost: evil.example\r\n"),
        ] {
            assert_eq!(
                fixture.vault.add(NewSecret {
                    name: "key",
                    hosts: &hosts,
                    header,
                    prefix,
                    value: SecretValue::new(b"x".to_vec()),
                }),
                Err(VaultError::InvalidHeader),
                "{header} {prefix:?}"
            );
        }
        let long = vec![b'x'; MAX_VALUE_BYTES + 1];
        for value in [
            &b""[..],
            &b"a\nb"[..],
            &b"a\r\nHost: evil"[..],
            &[0xff][..],
            &long[..],
        ] {
            assert_eq!(
                fixture.vault.add(NewSecret {
                    name: "key",
                    hosts: &hosts,
                    header: "x-api-key",
                    prefix: "",
                    value: SecretValue::new(value.to_vec()),
                }),
                Err(VaultError::InvalidValue)
            );
        }
        assert!(fixture.store.values.lock().unwrap().is_empty());
        assert_eq!(fixture.vault.list().unwrap().len(), 0);
    }

    #[test]
    fn with_no_keystore_nothing_is_stored_anywhere() {
        let root = TempDir::new().unwrap();
        let vault = Vault::new(
            root.path(),
            FakeStore {
                unavailable: true,
                ..FakeStore::default()
            },
        );
        let hosts = vec!["a.example".to_string()];
        assert_eq!(
            vault.add(NewSecret {
                name: "key",
                hosts: &hosts,
                header: "x-api-key",
                prefix: "",
                value: SecretValue::new(SECRET.as_bytes().to_vec()),
            }),
            Err(VaultError::KeystoreUnavailable)
        );
        assert_eq!(vault.list().unwrap().len(), 0);
        let file = root.path().join(VAULT_FILE);
        assert!(!file.exists());
    }

    #[test]
    fn a_value_that_cant_be_recorded_is_taken_back_out() {
        let fixture = Fixture::new();
        // A folder where the record goes, so that it can't be written.
        fs::create_dir(fixture.root.path().join("secrets.new")).unwrap();
        assert!(fixture.add("key", &["a.example"]).is_err());
        assert!(fixture.store.values.lock().unwrap().is_empty());
    }

    #[test]
    fn a_bad_record_allows_nothing() {
        for bad in [
            "not toml [",
            "extra = 1",
            "[secrets.key]\nhosts = [\"a.example\"]\nheader = \"cookie\"",
            "[secrets.key]\nhosts = [\"*.example\"]\nheader = \"x-api-key\"",
            "[secrets.KEY]\nhosts = [\"a.example\"]\nheader = \"x-api-key\"",
            "[secrets.key]\nhosts = [\"a.example\"]\nheader = \"x-api-key\"\nprefix = \"a\\nb\"",
            "[secrets.key]\nhosts = [\"a.example\"]\nheader = \"x-api-key\"\nextra = 1",
        ] {
            let fixture = Fixture::new();
            fixture.add("key", &["a.example"]).unwrap();
            fs::write(fixture.root.path().join(VAULT_FILE), bad).unwrap();
            assert_eq!(
                fixture.vault.header_for("key", "a.example").unwrap_err(),
                VaultError::Corrupt,
                "{bad}"
            );
            assert_eq!(fixture.vault.list().unwrap_err(), VaultError::Corrupt);
        }
    }

    /// Run on the real keystore in CI, where it's safe to add to it.
    #[cfg(not(target_os = "android"))]
    #[test]
    #[ignore = "uses the real keystore"]
    fn real_keystore_round_trip() {
        let store = OsKeystore;
        let name = format!("habitat-test-{}", std::process::id());
        store
            .put(&name, &SecretValue::new(SECRET.as_bytes().to_vec()))
            .unwrap();
        assert_eq!(store.get(&name).unwrap().expose(), SECRET.as_bytes());
        store.delete(&name).unwrap();
        assert!(store.get(&name).is_err());
    }
}
