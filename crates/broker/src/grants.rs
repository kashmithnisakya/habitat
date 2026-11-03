//! What an agent has been allowed to do, and for how long.
//!
//! Grants for one request or one run are only held in memory, so a crash can't make them outlive
//! it. Grants until revoked are saved outside the agent's home, and read again for each request, so
//! that revoking one takes effect on the next request, even from another process.

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::io;

use habitat_core::home::AgentStorage;
use habitat_core::manifest::{is_host_name, is_model_name};
use serde::{Deserialize, Serialize};

use crate::policy::{Grants, Target};

const FORMAT_VERSION: u32 = 1;

/// How long a grant lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifetime {
    /// Covers a single request.
    Once,
    /// Ends when the run does.
    ThisRun,
    /// Lasts until the user removes it.
    UntilRevoked,
}

/// Why the saved grants couldn't be read or changed. The text holds no host paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantError {
    /// A grant has to name a valid host or model.
    InvalidTarget,
    /// What's saved isn't valid, so it can't be relied on, or added to.
    Corrupt,
    /// The host's file system failed.
    Io(io::ErrorKind),
}

impl fmt::Display for GrantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTarget => write!(f, "that isn't a valid host or model name"),
            Self::Corrupt => write!(f, "the saved grants aren't valid"),
            Self::Io(kind) => write!(f, "file system error: {kind}"),
        }
    }
}

impl std::error::Error for GrantError {}

impl From<io::Error> for GrantError {
    fn from(error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::InvalidData {
            Self::Corrupt
        } else {
            Self::Io(error.kind())
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedGrants {
    format: u32,
    #[serde(default)]
    network: BTreeSet<String>,
    #[serde(default)]
    models: BTreeSet<String>,
}

impl SavedGrants {
    fn parse(data: Option<Vec<u8>>) -> Result<Self, GrantError> {
        let Some(data) = data else {
            return Ok(Self {
                format: FORMAT_VERSION,
                ..Self::default()
            });
        };
        let text = String::from_utf8(data).map_err(|_| GrantError::Corrupt)?;
        let saved: Self = toml::from_str(&text).map_err(|_| GrantError::Corrupt)?;
        let valid = saved.format == FORMAT_VERSION
            && saved.network.iter().all(|host| is_host_name(host))
            && saved.models.iter().all(|model| is_model_name(model));
        if valid {
            Ok(saved)
        } else {
            Err(GrantError::Corrupt)
        }
    }

    fn set_for(&mut self, target: &Target) -> (&mut BTreeSet<String>, String) {
        match target {
            Target::NetworkHost(host) => (&mut self.network, host.clone()),
            Target::Model(model) => (&mut self.models, model.clone()),
        }
    }

    fn contains(&self, target: &Target) -> bool {
        match target {
            Target::NetworkHost(host) => self.network.contains(host),
            Target::Model(model) => self.models.contains(model),
        }
    }

    fn targets(self) -> Vec<Target> {
        let hosts = self.network.into_iter().map(Target::NetworkHost);
        hosts
            .chain(self.models.into_iter().map(Target::Model))
            .collect()
    }
}

fn is_valid(target: &Target) -> bool {
    match target {
        Target::NetworkHost(host) => is_host_name(host),
        Target::Model(model) => is_model_name(model),
    }
}

/// Lists an agent's saved grants, with hosts first, in order.
pub fn list_saved(storage: &AgentStorage) -> Result<Vec<Target>, GrantError> {
    Ok(SavedGrants::parse(storage.read_grants()?)?.targets())
}

/// Removes a saved grant. Returns whether there was one.
pub fn revoke_saved(storage: &AgentStorage, target: &Target) -> Result<bool, GrantError> {
    let mut removed = false;
    storage.update_grants(|data| {
        let mut saved = SavedGrants::parse(data)?;
        let (set, name) = saved.set_for(target);
        removed = set.remove(&name);
        serialize(&saved)
    })?;
    Ok(removed)
}

fn serialize(saved: &SavedGrants) -> Result<Vec<u8>, GrantError> {
    toml::to_string(saved)
        .map(String::into_bytes)
        .map_err(|_| GrantError::Corrupt)
}

/// One run's view of an agent's grants.
pub struct RunGrants<'a> {
    storage: &'a AgentStorage,
    once: HashSet<Target>,
    this_run: HashSet<Target>,
}

impl<'a> RunGrants<'a> {
    /// Starts with nothing but what's saved, so nothing from an earlier run can carry over.
    pub fn new(storage: &'a AgentStorage) -> Self {
        Self {
            storage,
            once: HashSet::new(),
            this_run: HashSet::new(),
        }
    }

    /// Adds a grant for exactly this target, after the user has allowed it.
    pub fn grant(&mut self, target: Target, lifetime: Lifetime) -> Result<(), GrantError> {
        if !is_valid(&target) {
            return Err(GrantError::InvalidTarget);
        }
        match lifetime {
            Lifetime::Once => {
                self.once.insert(target);
            }
            Lifetime::ThisRun => {
                self.this_run.insert(target);
            }
            Lifetime::UntilRevoked => self.storage.update_grants(|data| {
                let mut saved = SavedGrants::parse(data)?;
                let (set, name) = saved.set_for(&target);
                set.insert(name);
                serialize(&saved)
            })?,
        }
        Ok(())
    }

    /// Records that a request was allowed, which uses up a grant for one request. If there's more
    /// than one kind of grant for this target, this uses up the shortest.
    pub fn used(&mut self, target: &Target) {
        self.once.remove(target);
    }

    /// Removes every grant for this target that this run can reach. Returns whether there were any.
    pub fn revoke(&mut self, target: &Target) -> Result<bool, GrantError> {
        let once = self.once.remove(target);
        let this_run = self.this_run.remove(target);
        let saved = revoke_saved(self.storage, target)?;
        Ok(once || this_run || saved)
    }
}

impl Grants for RunGrants<'_> {
    /// Reads what's saved each time. If that fails, it doesn't count, because an unreadable record
    /// of what's allowed can't allow anything.
    fn covers(&self, target: &Target) -> bool {
        self.once.contains(target)
            || self.this_run.contains(target)
            || self
                .storage
                .read_grants()
                .ok()
                .and_then(|data| SavedGrants::parse(data).ok())
                .is_some_and(|saved| saved.contains(target))
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;

    const CRASH_DIR_VAR: &str = "HABITAT_TEST_GRANTS_CRASH_DIR";

    fn storage(root: &TempDir) -> AgentStorage {
        AgentStorage::open(root.path(), "agent").unwrap()
    }

    fn host(name: &str) -> Target {
        Target::NetworkHost(name.to_string())
    }

    fn model(name: &str) -> Target {
        Target::Model(name.to_string())
    }

    const HOST: &str = "api.github.com";

    #[test]
    fn nothing_is_granted_to_start_with() {
        let root = TempDir::new().unwrap();
        let storage = storage(&root);
        let grants = RunGrants::new(&storage);
        assert!(!grants.covers(&host(HOST)));
        assert!(!grants.covers(&model("default")));
        assert_eq!(list_saved(&storage), Ok(Vec::new()));
    }

    #[test]
    fn once_covers_until_its_used() {
        let root = TempDir::new().unwrap();
        let storage = storage(&root);
        let mut grants = RunGrants::new(&storage);
        grants.grant(host(HOST), Lifetime::Once).unwrap();
        assert!(grants.covers(&host(HOST)));
        assert!(grants.covers(&host(HOST)), "asking doesn't use it up");
        grants.used(&host(HOST));
        assert!(!grants.covers(&host(HOST)));
    }

    #[test]
    fn this_run_lasts_the_whole_run_and_no_longer() {
        let root = TempDir::new().unwrap();
        let storage = storage(&root);
        let mut grants = RunGrants::new(&storage);
        grants.grant(host(HOST), Lifetime::ThisRun).unwrap();
        for _ in 0..3 {
            assert!(grants.covers(&host(HOST)));
            grants.used(&host(HOST));
        }
        drop(grants);
        assert!(!RunGrants::new(&storage).covers(&host(HOST)));
    }

    #[test]
    fn once_doesnt_outlast_its_run_even_if_it_wasnt_used() {
        let root = TempDir::new().unwrap();
        let storage = storage(&root);
        let mut grants = RunGrants::new(&storage);
        grants.grant(host(HOST), Lifetime::Once).unwrap();
        drop(grants);
        assert!(!RunGrants::new(&storage).covers(&host(HOST)));
    }

    #[test]
    fn neither_short_grant_is_saved_anywhere() {
        let root = TempDir::new().unwrap();
        let storage = storage(&root);
        let mut grants = RunGrants::new(&storage);
        grants.grant(host(HOST), Lifetime::Once).unwrap();
        grants.grant(model("default"), Lifetime::ThisRun).unwrap();
        assert!(!storage.grants_path().exists());
        assert_eq!(list_saved(&storage), Ok(Vec::new()));
    }

    #[test]
    fn until_revoked_survives_a_restart_and_isnt_used_up() {
        let root = TempDir::new().unwrap();
        let storage_one = storage(&root);
        let mut grants = RunGrants::new(&storage_one);
        grants.grant(host(HOST), Lifetime::UntilRevoked).unwrap();
        grants
            .grant(model("default"), Lifetime::UntilRevoked)
            .unwrap();
        grants.used(&host(HOST));
        assert!(grants.covers(&host(HOST)));
        drop(grants);

        let reopened = self::storage(&root);
        let grants = RunGrants::new(&reopened);
        assert!(grants.covers(&host(HOST)));
        assert!(grants.covers(&model("default")));
        assert_eq!(
            list_saved(&reopened),
            Ok(vec![host(HOST), model("default")])
        );
    }

    #[test]
    fn using_up_a_once_grant_leaves_a_longer_one_alone() {
        let root = TempDir::new().unwrap();
        let storage = storage(&root);
        let mut grants = RunGrants::new(&storage);
        grants.grant(host(HOST), Lifetime::ThisRun).unwrap();
        grants.grant(host(HOST), Lifetime::Once).unwrap();
        grants.used(&host(HOST));
        assert!(grants.covers(&host(HOST)));
    }

    #[test]
    fn a_grant_covers_exactly_what_it_names() {
        let root = TempDir::new().unwrap();
        let storage = storage(&root);
        let mut grants = RunGrants::new(&storage);
        for lifetime in [Lifetime::Once, Lifetime::ThisRun, Lifetime::UntilRevoked] {
            grants.grant(host(HOST), lifetime).unwrap();
            for other in [
                host("github.com"),
                host("uploads.github.com"),
                host("api.github.com.evil.example"),
                host("xapi.github.com"),
                model("api.github.com"),
            ] {
                assert!(!grants.covers(&other), "{other:?}");
            }
        }
    }

    #[test]
    fn invalid_targets_cant_be_granted() {
        let root = TempDir::new().unwrap();
        let storage = storage(&root);
        let mut grants = RunGrants::new(&storage);
        for lifetime in [Lifetime::Once, Lifetime::ThisRun, Lifetime::UntilRevoked] {
            for target in [
                host(""),
                host("*.github.com"),
                host("API.github.com"),
                host("127.0.0.1"),
                host("localhost"),
                host("a.com\nnetwork = [\"evil.example\"]"),
                model(""),
                model("BAD NAME"),
                model("x\"]\nnetwork = [\"evil.example\"]\n#"),
            ] {
                assert_eq!(
                    grants.grant(target.clone(), lifetime),
                    Err(GrantError::InvalidTarget),
                    "{target:?}"
                );
                assert!(!grants.covers(&target));
            }
        }
        assert!(!storage.grants_path().exists());
    }

    #[test]
    fn revoking_takes_effect_on_the_next_request() {
        let root = TempDir::new().unwrap();
        let storage = storage(&root);
        let mut grants = RunGrants::new(&storage);
        for lifetime in [Lifetime::Once, Lifetime::ThisRun, Lifetime::UntilRevoked] {
            grants.grant(host(HOST), lifetime).unwrap();
            assert!(grants.covers(&host(HOST)));
            assert_eq!(grants.revoke(&host(HOST)), Ok(true));
            assert!(!grants.covers(&host(HOST)), "{lifetime:?}");
        }
        assert_eq!(grants.revoke(&host(HOST)), Ok(false));
    }

    #[test]
    fn revoking_all_kinds_at_once_removes_them_all() {
        let root = TempDir::new().unwrap();
        let storage = storage(&root);
        let mut grants = RunGrants::new(&storage);
        for lifetime in [Lifetime::Once, Lifetime::ThisRun, Lifetime::UntilRevoked] {
            grants.grant(host(HOST), lifetime).unwrap();
        }
        assert_eq!(grants.revoke(&host(HOST)), Ok(true));
        assert!(!grants.covers(&host(HOST)));
    }

    #[test]
    fn revoking_from_outside_a_run_reaches_a_run_thats_going() {
        let root = TempDir::new().unwrap();
        let in_run = storage(&root);
        let mut grants = RunGrants::new(&in_run);
        grants.grant(host(HOST), Lifetime::UntilRevoked).unwrap();
        assert!(grants.covers(&host(HOST)));

        // As another process would, with its own view of the same folder.
        let outside = self::storage(&root);
        assert_eq!(revoke_saved(&outside, &host(HOST)), Ok(true));
        assert!(!grants.covers(&host(HOST)));
    }

    #[test]
    fn revoking_one_leaves_the_others() {
        let root = TempDir::new().unwrap();
        let storage = storage(&root);
        let mut grants = RunGrants::new(&storage);
        grants.grant(host(HOST), Lifetime::UntilRevoked).unwrap();
        grants
            .grant(host("example.com"), Lifetime::UntilRevoked)
            .unwrap();
        grants.grant(model(HOST), Lifetime::UntilRevoked).unwrap();
        assert_eq!(grants.revoke(&host(HOST)), Ok(true));
        assert!(grants.covers(&host("example.com")));
        assert!(grants.covers(&model(HOST)));
    }

    #[test]
    fn a_bad_saved_file_allows_nothing_and_isnt_overwritten() {
        let bad_files: [&[u8]; 7] = [
            b"not toml [",
            b"\xff\xfe",
            b"network = [\"api.github.com\"]",
            b"format = 2\nnetwork = [\"api.github.com\"]",
            b"format = 1\nextra = 1\nnetwork = [\"api.github.com\"]",
            b"format = 1\nnetwork = [\"api.github.com\", \"*.evil\"]",
            b"format = 1\nnetwork = [\"api.github.com\"]\nmodels = [\"BAD NAME\"]",
        ];
        for bad in bad_files {
            let root = TempDir::new().unwrap();
            let storage = storage(&root);
            fs::write(storage.grants_path(), bad).unwrap();

            let mut grants = RunGrants::new(&storage);
            assert!(!grants.covers(&host(HOST)));
            assert_eq!(list_saved(&storage), Err(GrantError::Corrupt));
            assert_eq!(
                grants.grant(host("example.com"), Lifetime::UntilRevoked),
                Err(GrantError::Corrupt)
            );
            assert_eq!(
                revoke_saved(&storage, &host(HOST)),
                Err(GrantError::Corrupt)
            );
            assert_eq!(fs::read(storage.grants_path()).unwrap(), bad);
        }
    }

    #[test]
    fn a_huge_saved_file_allows_nothing() {
        let root = TempDir::new().unwrap();
        let storage = storage(&root);
        let mut text = String::from("format = 1\nnetwork = [\"api.github.com\"]\n");
        text.push_str(&"#".repeat(2 * 1024 * 1024));
        fs::write(storage.grants_path(), text).unwrap();
        assert!(!RunGrants::new(&storage).covers(&host(HOST)));
        assert_eq!(list_saved(&storage), Err(GrantError::Corrupt));
    }

    #[test]
    fn a_link_in_place_of_the_saved_file_allows_nothing() {
        let root = TempDir::new().unwrap();
        let storage = storage(&root);
        let elsewhere = root.path().join("elsewhere");
        fs::write(&elsewhere, "format = 1\nnetwork = [\"api.github.com\"]\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&elsewhere, storage.grants_path()).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&elsewhere, storage.grants_path()).unwrap();
        assert!(!RunGrants::new(&storage).covers(&host(HOST)));
        assert!(list_saved(&storage).is_err());
    }

    #[test]
    fn changes_from_several_places_at_once_all_count() {
        let root = TempDir::new().unwrap();
        thread::scope(|scope| {
            for index in 0..8 {
                let root = &root;
                scope.spawn(move || {
                    let storage = self::storage(root);
                    let mut grants = RunGrants::new(&storage);
                    grants
                        .grant(
                            host(&format!("host{index}.example")),
                            Lifetime::UntilRevoked,
                        )
                        .unwrap();
                });
            }
        });
        assert_eq!(list_saved(&storage(&root)).unwrap().len(), 8);
    }

    #[test]
    #[ignore = "run by killing_a_change_leaves_the_old_or_new_grants"]
    fn grants_crash_writer() {
        let dir = env::var(CRASH_DIR_VAR).expect("only for the crash test");
        let storage = AgentStorage::open(Path::new(&dir), "agent").unwrap();
        let mut grants = RunGrants::new(&storage);
        loop {
            grants.grant(host(HOST), Lifetime::UntilRevoked).unwrap();
            revoke_saved(&storage, &host(HOST)).unwrap();
        }
    }

    #[test]
    fn killing_a_change_leaves_the_old_or_new_grants() {
        let root = TempDir::new().unwrap();
        let storage = storage(&root);
        for round in 0..10 {
            let mut child = Command::new(env::current_exe().unwrap())
                .args(["--exact", "grants::tests::grants_crash_writer", "--ignored"])
                .env(CRASH_DIR_VAR, root.path())
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            thread::sleep(Duration::from_millis(40 + round * 11));
            child.kill().unwrap();
            child.wait().unwrap();

            // It reads as valid, and a killed process didn't keep hold of the lock.
            let saved = list_saved(&storage).unwrap();
            assert!(saved.is_empty() || saved == [host(HOST)], "{saved:?}");
            revoke_saved(&storage, &host(HOST)).unwrap();
        }
    }
}
