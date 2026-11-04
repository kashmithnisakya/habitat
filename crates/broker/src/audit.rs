//! A log of everything an agent asks for outside its home, which shows if it's been changed later.
//!
//! Each entry holds the hash of the one before, so changing, removing or reordering an entry breaks
//! the chain from there on. A separate file holds the latest position, so removing entries from the
//! end shows too. Someone who controls the device could still rewrite the whole log and that file,
//! and this can't detect that.
//!
//! An entry's fields are all things that the host chose or checked. Nothing that the agent wrote
//! goes in here, and no field can hold headers or request bodies, which is what keeps API keys out.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use habitat_core::backend::{Lifetime, Target};
use habitat_core::home::AgentStorage;
use habitat_core::manifest::printable;
use habitat_core::platform;
use habitat_core::run_log::RunId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How many entries one run can add. An agent can make requests as fast as it likes, so without
/// this it could fill the disk. The broker refuses requests beyond this, as they can't be logged.
pub const MAX_ENTRIES_PER_RUN: u32 = 10_000;
/// Far more than an entry can need.
const MAX_LINE_BYTES: usize = 4096;
/// How much of the log's end is read to find the last entry when opening it.
const FIRST_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Why the log can't be used. The text holds no host paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditError {
    /// The log has been changed, or doesn't make sense. This says which entry, counting from 1.
    Tampered { entry: u64 },
    /// This run has added as many entries as it can.
    TooManyEntries,
    /// The host's file system failed.
    Io(io::ErrorKind),
}

impl fmt::Display for AuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tampered { entry } => {
                write!(f, "the log has been changed, at or before entry {entry}")
            }
            Self::TooManyEntries => write!(f, "this run has added too many entries to the log"),
            Self::Io(kind) => write!(f, "file system error: {kind}"),
        }
    }
}

impl std::error::Error for AuditError {}

impl From<io::Error> for AuditError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

/// Something worth recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum AuditEvent {
    /// An agent asked for something.
    Requested {
        #[serde(with = "saved_target")]
        target: Target,
    },
    /// It got it.
    Allowed {
        #[serde(with = "saved_target")]
        target: Target,
        #[serde(with = "saved_lifetime")]
        lifetime: Lifetime,
    },
    /// It didn't.
    Refused {
        #[serde(with = "saved_target")]
        target: Target,
    },
    /// A network request was made, and this is what came back. Only the host is recorded, because a
    /// URL's path and query can hold private data.
    Fetched {
        #[serde(with = "saved_target")]
        target: Target,
        method: String,
        /// 0 if nothing came back.
        status: u16,
    },
    /// A request to an AI model ended, having used this many tokens. What was said isn't recorded.
    ModelUsed {
        model: String,
        input_tokens: u32,
        output_tokens: u32,
    },
    /// The agent showed the user a message. What it said isn't recorded, as it can be private.
    MessageSent,
    /// The run was given a file that the user chose. Its name isn't recorded, as it can be private.
    UserFilePicked,
    /// The run asked for a file, and didn't get one.
    UserFileRefused,
    /// The run has hit [`MAX_ENTRIES_PER_RUN`], so nothing else it asks for will be allowed.
    LimitReached,
    /// The run ended. This is one of a few words chosen by the supervisor.
    RunEnded { how: String },
}

/// Saves a target as a kind and a name. A request can be malformed, so the name is made safe first.
mod saved_target {
    use habitat_core::backend::Target;
    use habitat_core::manifest::printable;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    enum Saved {
        Network(String),
        Model(String),
    }

    pub(super) fn serialize<S: Serializer>(
        target: &Target,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match target {
            Target::NetworkHost(host) => Saved::Network(printable(host)),
            Target::Model(model) => Saved::Model(printable(model)),
        }
        .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Target, D::Error> {
        Ok(match Saved::deserialize(deserializer)? {
            Saved::Network(host) => Target::NetworkHost(host),
            Saved::Model(model) => Target::Model(model),
        })
    }
}

mod saved_lifetime {
    use habitat_core::backend::Lifetime;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    enum Saved {
        Once,
        ThisRun,
        UntilRevoked,
    }

    pub(super) fn serialize<S: Serializer>(
        value: &Lifetime,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Lifetime::Once => Saved::Once,
            Lifetime::ThisRun => Saved::ThisRun,
            Lifetime::UntilRevoked => Saved::UntilRevoked,
        }
        .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Lifetime, D::Error> {
        Ok(match Saved::deserialize(deserializer)? {
            Saved::Once => Lifetime::Once,
            Saved::ThisRun => Lifetime::ThisRun,
            Saved::UntilRevoked => Lifetime::UntilRevoked,
        })
    }
}

/// One entry in the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditEntry {
    /// Its position in the log, counting from 1.
    pub number: u64,
    pub at_ms: u64,
    /// The run it belongs to, which links it to that run's record.
    pub run: String,
    pub event: AuditEvent,
    /// The hash of the entry before it.
    previous: String,
}

fn hash_of(body: &str) -> String {
    Sha256::digest(body.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

fn head_path(log: &Path) -> PathBuf {
    log.with_extension("head")
}

/// The position that the separate file says the log has reached. It's only ever behind the log if
/// the host died between writing the two, so it's never allowed to be ahead.
fn read_head(log: &Path) -> Result<(u64, String), AuditError> {
    let bad = AuditError::Tampered { entry: 0 };
    let file = match platform::open_no_follow(&head_path(log)) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((0, FIRST_HASH.to_string()));
        }
        other => other?,
    };
    if !file.metadata()?.is_file() {
        return Err(bad);
    }
    let mut text = String::new();
    file.take(256)
        .read_to_string(&mut text)
        .map_err(|_| bad.clone())?;
    let (number, hash) = text.trim_end().split_once(' ').ok_or(bad.clone())?;
    let number: u64 = number.parse().map_err(|_| bad.clone())?;
    Ok((number, hash.to_string()))
}

fn write_head(log: &Path, number: u64, hash: &str) -> io::Result<()> {
    let path = head_path(log);
    let temp = path.with_extension("head-new");
    if temp.symlink_metadata().is_ok() {
        fs::remove_file(&temp)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(format!("{number} {hash}\n").as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, &path)
}

/// What reading the whole log found: its entries, the last hash, and where valid entries end.
struct Chain {
    entries: Vec<AuditEntry>,
    last_hash: String,
    valid_bytes: u64,
}

/// Reads and checks the whole log. The last line can be cut short if the host died while writing
/// it, but only if the separate file doesn't say that entry is finished.
fn read_chain(log: &Path) -> Result<Chain, AuditError> {
    let (head_number, head_hash) = read_head(log)?;
    let mut text = Vec::new();
    match platform::open_no_follow(log) {
        Ok(file) => {
            if !file.metadata()?.is_file() {
                return Err(AuditError::Tampered { entry: 0 });
            }
            let mut file = file;
            file.read_to_end(&mut text)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let mut entries = Vec::new();
    let mut last_hash = FIRST_HASH.to_string();
    let mut head_matched = head_number == 0 && head_hash == FIRST_HASH;
    let mut position = 0;
    while position < text.len() {
        let number = entries.len() as u64 + 1;
        let Some(end) = text[position..].iter().position(|byte| *byte == b'\n') else {
            // Cut short, which a crash can do to the last line and only the last.
            if number > head_number {
                break;
            }
            return Err(AuditError::Tampered { entry: number });
        };
        let bad = AuditError::Tampered { entry: number };
        let line = std::str::from_utf8(&text[position..position + end]).map_err(|_| bad.clone())?;
        let (hash, body) = line.split_once(' ').ok_or(bad.clone())?;
        let entry: AuditEntry = serde_json::from_str(body).map_err(|_| bad.clone())?;
        if entry.number != number || entry.previous != last_hash || hash != hash_of(body) {
            return Err(bad);
        }
        if number == head_number {
            if hash != head_hash {
                return Err(bad);
            }
            head_matched = true;
        }
        last_hash = hash.to_string();
        entries.push(entry);
        position += end + 1;
    }
    if !head_matched {
        // The log is shorter than the separate file says it should be, so entries have gone.
        return Err(AuditError::Tampered {
            entry: entries.len() as u64 + 1,
        });
    }
    Ok(Chain {
        entries,
        last_hash,
        valid_bytes: position as u64,
    })
}

/// Checks a whole log, and returns its entries.
pub fn verify(storage: &AgentStorage) -> Result<Vec<AuditEntry>, AuditError> {
    Ok(read_chain(&storage.audit_path())?.entries)
}

/// Says what an entry records, for showing to someone. The file could have been changed by
/// anything, so this cleans what it prints, even after it's been checked.
pub fn describe(entry: &AuditEntry) -> String {
    let target = |target: &Target| match target {
        Target::NetworkHost(host) => format!("network host {}", printable(host)),
        Target::Model(model) => format!("AI model {}", printable(model)),
    };
    let what = match &entry.event {
        AuditEvent::Requested { target: asked } => format!("asked for {}", target(asked)),
        AuditEvent::Allowed {
            target: asked,
            lifetime,
        } => {
            let lasting = match lifetime {
                Lifetime::Once => "once",
                Lifetime::ThisRun => "for this run",
                Lifetime::UntilRevoked => "until revoked",
            };
            format!("allowed {} {lasting}", target(asked))
        }
        AuditEvent::Refused { target: asked } => format!("refused {}", target(asked)),
        AuditEvent::Fetched {
            target: asked,
            method,
            status,
        } => format!("{} to {} gave {status}", printable(method), target(asked)),
        AuditEvent::ModelUsed {
            model,
            input_tokens,
            output_tokens,
        } => format!(
            "AI model {} used {input_tokens} tokens in and {output_tokens} out",
            printable(model)
        ),
        AuditEvent::MessageSent => "showed the user a message".to_string(),
        AuditEvent::UserFilePicked => "given a file that the user chose".to_string(),
        AuditEvent::UserFileRefused => "asked for a file, and didn't get one".to_string(),
        AuditEvent::LimitReached => "reached the limit on requests".to_string(),
        AuditEvent::RunEnded { how } => format!("run ended: {}", printable(how)),
    };
    format!(
        "{} {} run {} {what}",
        entry.number,
        entry.at_ms,
        printable(&entry.run)
    )
}

/// Adds entries to one agent's log, for one run. Only one run of an agent happens at a time, so
/// this is the only thing writing to its log.
pub struct AuditLog {
    path: PathBuf,
    file: File,
    run: String,
    next_number: u64,
    last_hash: String,
    added: u32,
}

impl AuditLog {
    /// Opens the log, checking all of it. If the log has been changed, this fails, and nothing new
    /// can be added to it.
    pub fn open(storage: &AgentStorage, run_id: RunId) -> Result<Self, AuditError> {
        let path = storage.audit_path();
        let chain = read_chain(&path)?;
        let mut file = platform::open_or_create_no_follow(&path)?;
        if !file.metadata()?.is_file() {
            return Err(AuditError::Tampered { entry: 0 });
        }
        // Takes off anything cut short by a crash, which was never finished.
        file.set_len(chain.valid_bytes)?;
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            path,
            file,
            run: run_id.to_string(),
            next_number: chain.entries.len() as u64 + 1,
            last_hash: chain.last_hash,
            added: 0,
        })
    }

    /// Acts as if this run had already added this many entries. Saving each one to disk takes too
    /// long for a test to really add thousands.
    #[cfg(test)]
    pub(crate) fn skip_ahead(&mut self, added: u32) {
        self.added = added;
    }

    /// Adds an entry. Once this returns, it's on disk.
    pub fn append(&mut self, event: &AuditEvent) -> Result<(), AuditError> {
        if self.added > MAX_ENTRIES_PER_RUN {
            return Err(AuditError::TooManyEntries);
        }
        let limit_hit = self.added == MAX_ENTRIES_PER_RUN;
        let event = if limit_hit {
            &AuditEvent::LimitReached
        } else {
            event
        };
        let entry = AuditEntry {
            number: self.next_number,
            at_ms: now_ms(),
            run: self.run.clone(),
            event: event.clone(),
            previous: self.last_hash.clone(),
        };
        let body = serde_json::to_string(&entry)
            .map_err(|_| AuditError::Io(io::ErrorKind::InvalidData))?;
        let hash = hash_of(&body);
        let line = format!("{hash} {body}\n");
        if line.len() > MAX_LINE_BYTES {
            return Err(AuditError::Io(io::ErrorKind::InvalidData));
        }
        self.file.write_all(line.as_bytes())?;
        self.file.sync_all()?;
        write_head(&self.path, self.next_number, &hash)?;
        self.next_number += 1;
        self.last_hash = hash;
        self.added += 1;
        if limit_hit {
            return Err(AuditError::TooManyEntries);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn host(name: &str) -> Target {
        Target::NetworkHost(name.to_string())
    }

    fn run_id() -> RunId {
        RunId::parse(&"a".repeat(32)).unwrap()
    }

    struct Fixture {
        _root: TempDir,
        storage: AgentStorage,
    }

    impl Fixture {
        /// A log with three entries.
        fn new() -> Self {
            let root = TempDir::new().unwrap();
            let storage = AgentStorage::open(root.path(), "agent").unwrap();
            let mut log = AuditLog::open(&storage, run_id()).unwrap();
            for name in ["a.example", "b.example", "c.example"] {
                log.append(&AuditEvent::Requested { target: host(name) })
                    .unwrap();
            }
            Self {
                _root: root,
                storage,
            }
        }

        fn lines(&self) -> Vec<String> {
            fs::read_to_string(self.storage.audit_path())
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect()
        }

        fn write_lines(&self, lines: &[String]) {
            let mut text = lines.join("\n");
            if !lines.is_empty() {
                text.push('\n');
            }
            fs::write(self.storage.audit_path(), text).unwrap();
        }

        fn assert_tampered(&self) {
            assert!(
                matches!(verify(&self.storage), Err(AuditError::Tampered { .. })),
                "{:?}",
                verify(&self.storage)
            );
            assert!(matches!(
                AuditLog::open(&self.storage, run_id()),
                Err(AuditError::Tampered { .. })
            ));
        }
    }

    /// Rewrites the whole log with a change, with all the hashes made to match, as someone who
    /// knows how it works would. This leaves only what's inside the entries to catch it.
    fn forge(fixture: &Fixture, change: impl Fn(&str, usize) -> String) {
        let mut previous = FIRST_HASH.to_string();
        let mut lines = Vec::new();
        for (index, line) in fixture.lines().iter().enumerate() {
            let (_, body) = line.split_once(' ').unwrap();
            let mut entry: serde_json::Value = serde_json::from_str(body).unwrap();
            entry["previous"] = serde_json::Value::String(previous.clone());
            let body = change(&serde_json::to_string(&entry).unwrap(), index);
            previous = hash_of(&body);
            lines.push(format!("{previous} {body}"));
        }
        fixture.write_lines(&lines);
        write_head(&fixture.storage.audit_path(), lines.len() as u64, &previous).unwrap();
    }

    #[test]
    fn a_forged_log_with_nothing_changed_still_checks_out() {
        let fixture = Fixture::new();
        forge(&fixture, |body, _| body.to_string());
        assert_eq!(verify(&fixture.storage).unwrap().len(), 3);
    }

    #[test]
    fn entries_that_dont_count_up_are_detected_even_with_matching_hashes() {
        let fixture = Fixture::new();
        forge(&fixture, |body, index| {
            if index == 1 {
                body.replace("\"number\":2", "\"number\":7")
            } else {
                body.to_string()
            }
        });
        fixture.assert_tampered();
    }

    #[test]
    fn unknown_fields_are_detected_even_with_matching_hashes() {
        let fixture = Fixture::new();
        forge(&fixture, |body, _| body.replacen('{', "{\"extra\":1,", 1));
        fixture.assert_tampered();
    }

    #[test]
    fn hostile_text_in_a_forged_log_is_still_cleaned_when_shown() {
        let fixture = Fixture::new();
        forge(&fixture, |body, _| {
            body.replace(".example", "\\u001b[31m.example")
        });
        let entries = verify(&fixture.storage).unwrap();
        for entry in &entries {
            let text = describe(entry);
            assert!(!text.contains('\x1b'), "{text:?}");
            assert!(
                text.is_ascii() && !text.chars().any(char::is_control),
                "{text:?}"
            );
        }
        // It really did get through to here with the escape in it.
        assert!(
            format!("{:?}", entries[0].event).contains("\\u{1b}"),
            "{:?}",
            entries[0]
        );
    }

    #[test]
    fn a_log_reads_back_what_was_added() {
        let fixture = Fixture::new();
        let entries = verify(&fixture.storage).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].number, 2);
        assert_eq!(entries[1].run, "a".repeat(32));
        assert_eq!(
            entries[2].event,
            AuditEvent::Requested {
                target: host("c.example")
            }
        );
    }

    #[test]
    fn a_log_that_doesnt_exist_yet_is_empty() {
        let root = TempDir::new().unwrap();
        let storage = AgentStorage::open(root.path(), "agent").unwrap();
        assert_eq!(verify(&storage), Ok(Vec::new()));
    }

    #[test]
    fn a_later_run_carries_on_the_same_chain() {
        let fixture = Fixture::new();
        let mut log =
            AuditLog::open(&fixture.storage, RunId::parse(&"b".repeat(32)).unwrap()).unwrap();
        log.append(&AuditEvent::RunEnded {
            how: "completed".to_string(),
        })
        .unwrap();
        let entries = verify(&fixture.storage).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[3].run, "b".repeat(32));
    }

    #[test]
    fn editing_an_entry_is_detected() {
        for index in 0..3 {
            let fixture = Fixture::new();
            let mut lines = fixture.lines();
            lines[index] = lines[index].replace(".example", ".evil");
            fixture.write_lines(&lines);
            fixture.assert_tampered();
        }
    }

    #[test]
    fn editing_an_entry_and_its_own_hash_is_detected() {
        for index in 0..3 {
            let fixture = Fixture::new();
            let mut lines = fixture.lines();
            let (_, body) = lines[index].split_once(' ').unwrap();
            let body = body.replace(".example", ".evil");
            lines[index] = format!("{} {body}", hash_of(&body));
            fixture.write_lines(&lines);
            fixture.assert_tampered();
        }
    }

    #[test]
    fn removing_an_entry_is_detected() {
        for index in 0..3 {
            let fixture = Fixture::new();
            let mut lines = fixture.lines();
            lines.remove(index);
            fixture.write_lines(&lines);
            fixture.assert_tampered();
        }
    }

    #[test]
    fn removing_all_the_entries_is_detected() {
        let fixture = Fixture::new();
        fixture.write_lines(&[]);
        fixture.assert_tampered();
        fs::remove_file(fixture.storage.audit_path()).unwrap();
        fixture.assert_tampered();
    }

    #[test]
    fn reordering_entries_is_detected() {
        let fixture = Fixture::new();
        let mut lines = fixture.lines();
        lines.swap(0, 1);
        fixture.write_lines(&lines);
        fixture.assert_tampered();
    }

    #[test]
    fn adding_a_copy_of_an_entry_is_detected() {
        let fixture = Fixture::new();
        let mut lines = fixture.lines();
        let copy = lines[2].clone();
        lines.push(copy);
        fixture.write_lines(&lines);
        fixture.assert_tampered();
    }

    #[test]
    fn a_changed_or_missing_head_is_detected() {
        let fixture = Fixture::new();
        let head = head_path(&fixture.storage.audit_path());
        let good = fs::read_to_string(&head).unwrap();
        for bad in ["not valid", "3 0000", "9 0000", "x y", ""] {
            fs::write(&head, bad).unwrap();
            fixture.assert_tampered();
        }
        fs::write(&head, good).unwrap();
        assert!(verify(&fixture.storage).is_ok());
    }

    #[test]
    fn nothing_can_be_added_to_a_log_thats_been_changed() {
        let fixture = Fixture::new();
        let mut lines = fixture.lines();
        lines.remove(1);
        fixture.write_lines(&lines);
        assert!(AuditLog::open(&fixture.storage, run_id()).is_err());
        assert_eq!(fixture.lines().len(), 2, "the evidence is left as it was");
    }

    #[test]
    fn an_unfinished_last_line_from_a_crash_is_taken_off() {
        let fixture = Fixture::new();
        let full = fs::read(fixture.storage.audit_path()).unwrap();
        // As if the host died part way through writing a fourth entry.
        let mut cut = full.clone();
        cut.extend_from_slice(b"abc123 {\"number\":4,\"at");
        fs::write(fixture.storage.audit_path(), &cut).unwrap();
        assert_eq!(verify(&fixture.storage).unwrap().len(), 3);

        let mut log = AuditLog::open(&fixture.storage, run_id()).unwrap();
        log.append(&AuditEvent::LimitReached).unwrap();
        assert_eq!(verify(&fixture.storage).unwrap().len(), 4);
    }

    #[test]
    fn a_finished_entry_being_cut_short_is_detected() {
        let fixture = Fixture::new();
        let full = fs::read(fixture.storage.audit_path()).unwrap();
        fs::write(fixture.storage.audit_path(), &full[..full.len() - 20]).unwrap();
        fixture.assert_tampered();
    }

    #[test]
    fn a_log_ahead_of_its_head_from_a_crash_is_fine() {
        let fixture = Fixture::new();
        let head = head_path(&fixture.storage.audit_path());
        let lines = fixture.lines();
        let (hash, _) = lines[1].split_once(' ').unwrap();
        // As if the host died between writing the entry and the head.
        fs::write(&head, format!("2 {hash}\n")).unwrap();
        assert_eq!(verify(&fixture.storage).unwrap().len(), 3);
    }

    #[test]
    fn a_leftover_head_file_from_a_crash_doesnt_get_in_the_way() {
        let fixture = Fixture::new();
        let leftover = head_path(&fixture.storage.audit_path()).with_extension("head-new");
        fs::write(&leftover, "junk").unwrap();
        let mut log = AuditLog::open(&fixture.storage, run_id()).unwrap();
        log.append(&AuditEvent::LimitReached).unwrap();
        assert_eq!(verify(&fixture.storage).unwrap().len(), 4);
    }

    #[test]
    fn a_link_in_place_of_the_log_is_refused() {
        let root = TempDir::new().unwrap();
        let storage = AgentStorage::open(root.path(), "agent").unwrap();
        let elsewhere = root.path().join("elsewhere");
        fs::write(&elsewhere, "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&elsewhere, storage.audit_path()).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&elsewhere, storage.audit_path()).unwrap();
        assert!(AuditLog::open(&storage, run_id()).is_err());
        assert_eq!(fs::read(&elsewhere).unwrap(), b"");
    }

    #[test]
    fn hostile_text_in_a_target_is_made_safe_and_kept_to_a_size() {
        let root = TempDir::new().unwrap();
        let storage = AgentStorage::open(root.path(), "agent").unwrap();
        let mut log = AuditLog::open(&storage, run_id()).unwrap();
        let hostile = format!("\x1b[31m\n{}", "x".repeat(1_000_000));
        log.append(&AuditEvent::Requested {
            target: host(&hostile),
        })
        .unwrap();
        let text = fs::read_to_string(storage.audit_path()).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(!text.contains('\x1b'));
        assert!(text.len() < MAX_LINE_BYTES);
        let entries = verify(&storage).unwrap();
        let shown = describe(&entries[0]);
        assert!(
            shown.is_ascii() && !shown.chars().any(char::is_control),
            "{shown}"
        );
    }

    #[test]
    fn describing_an_entry_says_what_happened() {
        let fixture = Fixture::new();
        let entries = verify(&fixture.storage).unwrap();
        let text = describe(&entries[0]);
        assert!(text.contains("asked for network host a.example"), "{text}");
        assert!(text.contains(&"a".repeat(32)), "{text}");
    }

    #[test]
    fn a_run_stops_at_its_limit_and_says_so() {
        let root = TempDir::new().unwrap();
        let storage = AgentStorage::open(root.path(), "agent").unwrap();
        let mut log = AuditLog::open(&storage, run_id()).unwrap();
        // Skips ahead, because each entry is saved to disk, which takes too long to do this often.
        log.added = MAX_ENTRIES_PER_RUN - 1;
        let event = AuditEvent::Requested {
            target: host("a.example"),
        };
        assert_eq!(log.append(&event), Ok(()));
        assert_eq!(log.append(&event), Err(AuditError::TooManyEntries));
        assert_eq!(log.append(&event), Err(AuditError::TooManyEntries));
        let entries = verify(&storage).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].event, AuditEvent::LimitReached);
    }
}
