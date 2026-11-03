//! The record of an agent's runs, which is kept outside its home so that the agent can't reach it.
//!
//! Each run has a file of its own, saved when the run starts and replaced when it ends. If the host
//! is killed, the record is left with no ending, and the next run marks it as interrupted. A file
//! is always replaced whole, so a crash can't leave half of a record.

use std::fmt;
use std::io::{self, Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use serde::{Deserialize, Serialize};

use crate::backend::{Event, RunOutcome};
use crate::home::{AgentStorage, sync_dir};

/// How many runs of each agent are remembered. The oldest are removed first, so that an agent that
/// runs often can't fill the disk with records. Each one is a few KB at most.
pub const MAX_RECORDS: usize = 1000;
/// Far more than any record needs. Anything larger didn't come from here.
const MAX_RECORD_BYTES: u64 = 64 * 1024;
const RECORD_EXTENSION: &str = ".toml";
const TEMP_EXTENSION: &str = ".new";
const RANDOM_BYTES: usize = 10;
const MAX_ID_MS: u128 = (1 << 48) - 1;

/// Identifies one run, for good. An audit log entry can hold this to say which run it came from.
///
/// It's made of the time in its top 48 bits and random bits below that, so sorting the IDs of an
/// agent's runs puts them in order, and IDs from different agents or devices don't clash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunId(u128);

impl RunId {
    /// Reads an ID in the form that this prints as, which is the only form it accepts.
    pub fn parse(text: &str) -> Option<Self> {
        let is_lowercase_hex = |byte: &u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte);
        if text.len() != 32 || !text.as_bytes().iter().all(is_lowercase_hex) {
            return None;
        }
        u128::from_str_radix(text, 16).ok().map(Self)
    }

    /// Makes the ID that follows `latest`. It always sorts after it, even if the clock has gone
    /// backwards, as a phone's can.
    fn after(latest: Option<Self>, now_ms: u64, random: [u8; RANDOM_BYTES]) -> Self {
        let earliest = latest.map_or(0, |id| (id.0 >> (RANDOM_BYTES * 8)) + 1);
        let time = u128::from(now_ms).max(earliest).min(MAX_ID_MS);
        let mut bytes = [0; 16];
        bytes[16 - RANDOM_BYTES..].copy_from_slice(&random);
        Self((time << (RANDOM_BYTES * 8)) | u128::from_be_bytes(bytes))
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

/// What kind of event a run was for. The event's text isn't recorded, because it can be private.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    UserMessage,
    Scheduled,
}

impl From<&Event> for EventKind {
    fn from(event: &Event) -> Self {
        match event {
            Event::UserMessage(_) => Self::UserMessage,
            Event::Scheduled(_) => Self::Scheduled,
        }
    }
}

/// How a run ended. Text in here is safe to print, even for a hostile agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnding {
    /// The agent ran, and this is how that went.
    Ran(RunOutcome),
    /// The agent's code never ran, for this reason.
    NotStarted(String),
    /// The host stopped before it could record an ending, such as by being killed or losing power.
    Interrupted,
}

/// What's known about one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub id: RunId,
    pub event: EventKind,
    pub started_at_ms: u64,
    /// Not known for a run that was interrupted.
    pub ended_at_ms: Option<u64>,
    /// `None` while the run is going.
    pub ending: Option<RunEnding>,
}

impl RunRecord {
    /// Records how the run ended, as of now.
    pub(crate) fn end(&mut self, ending: RunEnding) {
        self.ended_at_ms = Some(now_ms());
        self.ending = Some(ending);
    }
}

/// An agent's recent runs, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunHistory {
    pub runs: Vec<RunRecord>,
    /// How many records couldn't be read. Only something other than the core could cause this.
    pub unreadable: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum RawEnding {
    Completed,
    Failed,
    TimeLimit,
    MemoryLimit,
    Stopped,
    Crashed,
    Internal,
    NotStarted,
    Interrupted,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRecord {
    event: EventKind,
    started_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    ended_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ending: Option<RawEnding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl RawRecord {
    fn new(record: &RunRecord) -> Self {
        let (ending, detail) = match &record.ending {
            None => (None, None),
            Some(RunEnding::Interrupted) => (Some(RawEnding::Interrupted), None),
            Some(RunEnding::NotStarted(reason)) => {
                (Some(RawEnding::NotStarted), Some(reason.clone()))
            }
            Some(RunEnding::Ran(outcome)) => {
                let (ending, detail) = match outcome {
                    RunOutcome::Completed => (RawEnding::Completed, None),
                    RunOutcome::Failed(message) => (RawEnding::Failed, Some(message.clone())),
                    RunOutcome::TimeLimit => (RawEnding::TimeLimit, None),
                    RunOutcome::MemoryLimit => (RawEnding::MemoryLimit, None),
                    RunOutcome::Stopped => (RawEnding::Stopped, None),
                    RunOutcome::Crashed(reason) => (RawEnding::Crashed, Some(reason.clone())),
                    RunOutcome::Internal(reason) => (RawEnding::Internal, Some(reason.clone())),
                };
                (Some(ending), detail)
            }
        };
        Self {
            event: record.event,
            started_at_ms: record.started_at_ms,
            ended_at_ms: record.ended_at_ms,
            ending,
            detail,
        }
    }

    fn into_record(self, id: RunId) -> Option<RunRecord> {
        let ending = match (self.ending, self.detail) {
            (None, None) => None,
            (Some(RawEnding::Completed), None) => Some(RunEnding::Ran(RunOutcome::Completed)),
            (Some(RawEnding::TimeLimit), None) => Some(RunEnding::Ran(RunOutcome::TimeLimit)),
            (Some(RawEnding::MemoryLimit), None) => Some(RunEnding::Ran(RunOutcome::MemoryLimit)),
            (Some(RawEnding::Stopped), None) => Some(RunEnding::Ran(RunOutcome::Stopped)),
            (Some(RawEnding::Interrupted), None) => Some(RunEnding::Interrupted),
            (Some(RawEnding::Failed), Some(text)) => Some(RunEnding::Ran(RunOutcome::Failed(text))),
            (Some(RawEnding::Crashed), Some(text)) => {
                Some(RunEnding::Ran(RunOutcome::Crashed(text)))
            }
            (Some(RawEnding::Internal), Some(text)) => {
                Some(RunEnding::Ran(RunOutcome::Internal(text)))
            }
            (Some(RawEnding::NotStarted), Some(text)) => Some(RunEnding::NotStarted(text)),
            _ => return None,
        };
        Some(RunRecord {
            id,
            event: self.event,
            started_at_ms: self.started_at_ms,
            ended_at_ms: self.ended_at_ms,
            ending,
        })
    }
}

/// One agent's records. Everything that writes here needs the agent's run lock to be held, because
/// that's what makes it safe to treat a record with no ending as interrupted.
pub(crate) struct RunLog {
    dir: Dir,
}

impl RunLog {
    pub(crate) fn open(storage: &AgentStorage) -> io::Result<Self> {
        Ok(Self {
            dir: storage.runs_dir()?,
        })
    }

    /// Saves the record of a new run, after closing the record of any run that didn't end.
    pub(crate) fn begin(&self, event: &Event) -> io::Result<RunRecord> {
        self.close_interrupted()?;
        let mut random = [0; RANDOM_BYTES];
        getrandom::fill(&mut random).map_err(|_| io::Error::from(io::ErrorKind::Other))?;
        let now = now_ms();
        let record = RunRecord {
            id: RunId::after(self.ids()?.last().copied(), now, random),
            event: event.into(),
            started_at_ms: now,
            ended_at_ms: None,
            ending: None,
        };
        self.save(&record)?;
        Ok(record)
    }

    /// Replaces a run's record. Once this returns, it's on disk.
    pub(crate) fn save(&self, record: &RunRecord) -> io::Result<()> {
        let text = toml::to_string(&RawRecord::new(record))
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        let temp = format!("{}{TEMP_EXTENSION}", record.id);

        let mut options = OpenOptions::new();
        options
            .write(true)
            .create(true)
            .truncate(true)
            .follow(FollowSymlinks::No)
            .mode(0o600);
        let mut file = self.dir.open_with(&temp, &options)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);

        self.dir
            .rename(&temp, &self.dir, format!("{}{RECORD_EXTENSION}", record.id))?;
        sync_dir(&self.dir).map_err(|_| io::Error::from(io::ErrorKind::Other))
    }

    /// Marks a run that never recorded an ending as interrupted. Only the latest can need this,
    /// because every new run does this first.
    pub(crate) fn close_interrupted(&self) -> io::Result<()> {
        let latest = self.ids()?.last().copied();
        if let Some(mut record) = latest.and_then(|id| self.read(id))
            && record.ending.is_none()
        {
            record.ending = Some(RunEnding::Interrupted);
            self.save(&record)?;
        }
        Ok(())
    }

    /// Removes the oldest records beyond the limit, and any temporary files left by a crash.
    pub(crate) fn prune(&self) -> io::Result<()> {
        let ids = self.ids()?;
        for id in &ids[..ids.len().saturating_sub(MAX_RECORDS)] {
            self.dir.remove_file(format!("{id}{RECORD_EXTENSION}"))?;
        }
        for entry in self.dir.entries()? {
            let name = entry?.file_name();
            if name.to_string_lossy().ends_with(TEMP_EXTENSION) {
                self.dir.remove_file(name)?;
            }
        }
        Ok(())
    }

    pub(crate) fn history(&self) -> io::Result<RunHistory> {
        let ids = self.ids()?;
        let runs: Vec<RunRecord> = ids.iter().filter_map(|id| self.read(*id)).collect();
        Ok(RunHistory {
            unreadable: ids.len() - runs.len(),
            runs,
        })
    }

    /// The IDs of all records, oldest first. A file with any other name isn't a record.
    fn ids(&self) -> io::Result<Vec<RunId>> {
        let mut ids = Vec::new();
        for entry in self.dir.entries()? {
            let name = entry?.file_name();
            let id = name
                .to_str()
                .and_then(|name| name.strip_suffix(RECORD_EXTENSION))
                .and_then(RunId::parse);
            ids.extend(id);
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Reads a record, or returns nothing if it isn't a valid one. That's never a reason to stop
    /// an agent from running.
    fn read(&self, id: RunId) -> Option<RunRecord> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .follow(FollowSymlinks::No)
            .custom_flags(libc::O_NONBLOCK);
        let file = self
            .dir
            .open_with(format!("{id}{RECORD_EXTENSION}"), &options)
            .ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let mut text = String::new();
        file.take(MAX_RECORD_BYTES + 1)
            .read_to_string(&mut text)
            .ok()?;
        if text.len() as u64 > MAX_RECORD_BYTES {
            return None;
        }
        toml::from_str::<RawRecord>(&text).ok()?.into_record(id)
    }
}

/// The time now, or 0 if the clock is set before 1970.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use super::*;

    const HOSTILE: &str = "\"\n[evil]\nending = \"completed\"";

    struct Fixture {
        root: TempDir,
        log: RunLog,
    }

    impl Fixture {
        fn new() -> Self {
            let root = TempDir::new().unwrap();
            let storage = AgentStorage::open(root.path(), "agent").unwrap();
            let log = RunLog::open(&storage).unwrap();
            Self { root, log }
        }

        fn path(&self, name: impl fmt::Display) -> std::path::PathBuf {
            self.root.path().join(format!("agents/agent/runs/{name}"))
        }

        fn begin(&self) -> RunRecord {
            self.log
                .begin(&Event::Scheduled("wake".to_string()))
                .unwrap()
        }

        fn record_path(&self, id: RunId) -> std::path::PathBuf {
            self.path(format!("{id}{RECORD_EXTENSION}"))
        }
    }

    #[test]
    fn ids_only_read_back_in_the_form_they_print_in() {
        let id = RunId(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        assert_eq!(RunId::parse(&id.to_string()), Some(id));
        assert_eq!(RunId(1).to_string().len(), 32);
        for bad in [
            "",
            "0",
            "0123456789ABCDEF0123456789ABCDEF",
            "+123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "g123456789abcdef0123456789abcdef",
            "../../../../../../../../../../..",
        ] {
            assert_eq!(RunId::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn ids_hold_the_time_and_randomness() {
        let id = RunId::after(None, 0x1234, [0xab; RANDOM_BYTES]);
        assert_eq!(id.to_string(), "000000001234abababababababababab");
    }

    #[test]
    fn ids_sort_in_order_even_when_the_clock_goes_backwards_or_stands_still() {
        let first = RunId::after(None, 1000, [0xff; RANDOM_BYTES]);
        let same_time = RunId::after(Some(first), 1000, [0; RANDOM_BYTES]);
        let clock_went_back = RunId::after(Some(same_time), 5, [0; RANDOM_BYTES]);
        let clock_recovered = RunId::after(Some(clock_went_back), 9000, [0; RANDOM_BYTES]);
        assert!(first < same_time);
        assert!(same_time < clock_went_back);
        assert!(clock_went_back < clock_recovered);
    }

    #[test]
    fn an_id_at_the_end_of_time_doesnt_overflow() {
        let last = RunId::after(None, u64::MAX, [0; RANDOM_BYTES]);
        assert_eq!(last.0 >> 80, MAX_ID_MS);
        let next = RunId::after(Some(last), 0, [1; RANDOM_BYTES]);
        assert_eq!(next.0 >> 80, MAX_ID_MS);
        assert_ne!(next, last);
    }

    #[test]
    fn every_ending_reads_back_the_same() {
        let endings = [
            None,
            Some(RunEnding::Interrupted),
            Some(RunEnding::NotStarted(HOSTILE.to_string())),
            Some(RunEnding::Ran(RunOutcome::Completed)),
            Some(RunEnding::Ran(RunOutcome::Failed(HOSTILE.to_string()))),
            Some(RunEnding::Ran(RunOutcome::TimeLimit)),
            Some(RunEnding::Ran(RunOutcome::MemoryLimit)),
            Some(RunEnding::Ran(RunOutcome::Stopped)),
            Some(RunEnding::Ran(RunOutcome::Crashed(HOSTILE.to_string()))),
            Some(RunEnding::Ran(RunOutcome::Internal(HOSTILE.to_string()))),
        ];
        let fixture = Fixture::new();
        for ending in endings {
            let mut record = fixture.begin();
            record.ended_at_ms = Some(7);
            record.ending = ending.clone();
            fixture.log.save(&record).unwrap();
            assert_eq!(fixture.log.read(record.id), Some(record), "{ending:?}");
        }
    }

    #[test]
    fn the_events_text_is_not_recorded() {
        let fixture = Fixture::new();
        let secret = "my-private-message";
        let record = fixture
            .log
            .begin(&Event::UserMessage(secret.to_string()))
            .unwrap();
        assert_eq!(record.event, EventKind::UserMessage);
        let text = fs::read_to_string(fixture.record_path(record.id)).unwrap();
        assert!(!text.contains(secret), "{text}");
    }

    #[test]
    fn a_new_run_marks_the_one_before_it_as_interrupted() {
        let fixture = Fixture::new();
        let first = fixture.begin();
        let second = fixture.begin();
        assert!(first.id < second.id);

        let history = fixture.log.history().unwrap();
        assert_eq!(history.runs.len(), 2);
        assert_eq!(history.runs[0].id, first.id);
        assert_eq!(history.runs[0].ending, Some(RunEnding::Interrupted));
        assert_eq!(history.runs[0].ended_at_ms, None);
        assert_eq!(history.runs[1].ending, None);
    }

    #[test]
    fn a_run_that_ended_isnt_marked_as_interrupted() {
        let fixture = Fixture::new();
        let mut record = fixture.begin();
        record.end(RunEnding::Ran(RunOutcome::Completed));
        fixture.log.save(&record).unwrap();
        fixture.begin();
        assert_eq!(
            fixture.log.history().unwrap().runs[0].ending,
            Some(RunEnding::Ran(RunOutcome::Completed))
        );
    }

    #[test]
    fn bad_records_are_counted_and_dont_get_in_the_way() {
        let bad_records = [
            b"not toml [".to_vec(),
            b"\xff\xfe".to_vec(),
            b"event = \"user-message\"".to_vec(),
            b"event = \"user-message\"\nstarted_at_ms = 1\nextra = 1".to_vec(),
            b"event = \"other\"\nstarted_at_ms = 1".to_vec(),
            // An ending that should hold a reason and doesn't, and the other way around.
            b"event = \"scheduled\"\nstarted_at_ms = 1\nending = \"failed\"".to_vec(),
            b"event = \"scheduled\"\nstarted_at_ms = 1\nending = \"stopped\"\ndetail = \"x\""
                .to_vec(),
            b"event = \"scheduled\"\nstarted_at_ms = 1\ndetail = \"x\"".to_vec(),
            // A good record, apart from being too long.
            format!(
                "event = \"scheduled\"\nstarted_at_ms = 1\n#{}",
                "x".repeat(MAX_RECORD_BYTES as usize)
            )
            .into_bytes(),
        ];
        for bad in bad_records {
            let fixture = Fixture::new();
            let good = fixture.begin();
            let id = RunId(good.id.0 + (1 << 80));
            fs::write(fixture.record_path(id), &bad).unwrap();

            let history = fixture.log.history().unwrap();
            assert_eq!(history.unreadable, 1);
            assert_eq!(history.runs, vec![good]);

            let after = fixture.begin();
            assert!(id < after.id);
            assert_eq!(fixture.log.history().unwrap().unreadable, 1);
        }
    }

    #[test]
    fn a_symlink_isnt_read_as_a_record() {
        let fixture = Fixture::new();
        let record = "event = \"scheduled\"\nstarted_at_ms = 1\n";
        let outside = fixture.root.path().join("outside");
        fs::write(&outside, record).unwrap();
        fs::write(fixture.path("beside"), record).unwrap();
        symlink(&outside, fixture.record_path(RunId(1))).unwrap();
        symlink("beside", fixture.record_path(RunId(2))).unwrap();
        assert_eq!(fixture.log.read(RunId(1)), None);
        assert_eq!(fixture.log.read(RunId(2)), None);
        assert_eq!(fixture.log.history().unwrap().unreadable, 2);
    }

    #[test]
    fn other_files_arent_counted_as_records() {
        let fixture = Fixture::new();
        for name in ["notes.txt", "1234.toml", ".toml"] {
            fs::write(fixture.path(name), "x").unwrap();
        }
        let history = fixture.log.history().unwrap();
        assert_eq!(history.unreadable, 0);
        assert!(history.runs.is_empty());
    }

    #[test]
    fn only_the_oldest_records_beyond_the_limit_are_removed() {
        let fixture = Fixture::new();
        let total = MAX_RECORDS + 5;
        for index in 0..total {
            fs::write(fixture.record_path(RunId(index as u128)), "x").unwrap();
        }
        fs::write(fixture.path("left-by-a-crash.new"), "x").unwrap();
        fixture.log.prune().unwrap();

        let ids = fixture.log.ids().unwrap();
        assert_eq!(ids.len(), MAX_RECORDS);
        assert_eq!(ids[0], RunId(5));
        assert_eq!(ids[MAX_RECORDS - 1], RunId(total as u128 - 1));
        assert!(!fixture.path("left-by-a-crash.new").exists());
    }

    #[test]
    fn nothing_is_removed_at_the_limit() {
        let fixture = Fixture::new();
        for index in 0..MAX_RECORDS {
            fs::write(fixture.record_path(RunId(index as u128)), "x").unwrap();
        }
        fixture.log.prune().unwrap();
        assert_eq!(fixture.log.ids().unwrap().len(), MAX_RECORDS);
    }

    #[test]
    fn saving_leaves_no_temporary_file() {
        let fixture = Fixture::new();
        let record = fixture.begin();
        assert!(
            !fixture
                .path(format!("{}{TEMP_EXTENSION}", record.id))
                .exists()
        );
    }
}
