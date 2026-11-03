//! The agent manifest, `agent.toml`.
//!
//! A manifest is untrusted input from the agent's author. It sets the agent's limits and lists
//! what the agent may ask for. Parsing one never grants access: requests only become access
//! through the broker, and the limits here are still capped by the host's own maximums.

use std::collections::HashSet;
use std::fmt;

use semver::Version;
use serde::Deserialize;

/// The largest manifest that will be parsed.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;

const MAX_NAME_CHARS: usize = 64;
const MAX_HOST_CHARS: usize = 253;
const MAX_HOST_LABEL_CHARS: usize = 63;
const MAX_MESSAGE_CHARS: usize = 300;
const MAX_REQUESTED_HOSTS: usize = 64;
const MAX_REQUESTED_MODELS: usize = 16;

const MEMORY_MB_RANGE: (u32, u32) = (1, 4096);
const RUN_SECONDS_RANGE: (u32, u32) = (1, 3600);
const DISK_MB_RANGE: (u32, u32) = (1, 10_240);
const MODEL_BUDGET_TOKENS_RANGE: (u64, u64) = (0, 100_000_000);

/// A manifest that has passed validation. The only way to get one is [`Manifest::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    name: String,
    version: Version,
    limits: Limits,
    requested_hosts: Vec<String>,
    requested_models: Vec<String>,
    requested_files: FileRequest,
}

/// The limits an agent declares. The host may enforce lower ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Memory for one run.
    pub memory_mb: u32,
    /// Time for one run.
    pub run_seconds: u32,
    /// Disk space for the agent's home.
    pub disk_mb: u32,
    /// AI model tokens for one run. Without a budget, the agent can't use a model.
    #[serde(default)]
    pub model_budget_tokens: u64,
}

/// Whether an agent may ask to read user files outside its home.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileRequest {
    /// The agent can't ask.
    #[default]
    None,
    /// The agent can ask, and the user decides each time.
    Ask,
}

/// Why a manifest was rejected. Messages are safe to print, even for a hostile manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// The manifest is larger than [`MAX_MANIFEST_BYTES`].
    TooLarge { bytes: usize },
    /// The manifest isn't valid TOML, has an unknown field, or has a field of the wrong type.
    Format {
        /// The position of the problem, as a line and column that both start at 1.
        position: Option<(usize, usize)>,
        message: String,
    },
    /// A field has a value that isn't allowed.
    Invalid {
        field: &'static str,
        message: String,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { bytes } => {
                write!(
                    f,
                    "manifest is {bytes} bytes, and the limit is {MAX_MANIFEST_BYTES}"
                )
            }
            Self::Format {
                position: Some((line, column)),
                message,
            } => write!(f, "line {line}, column {column}: {message}"),
            Self::Format {
                position: None,
                message,
            } => write!(f, "{message}"),
            Self::Invalid { field, message } => write!(f, "{field}: {message}"),
        }
    }
}

impl std::error::Error for ManifestError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    agent: RawAgent,
    limits: Limits,
    #[serde(default)]
    requests: RawRequests,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    name: String,
    version: String,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequests {
    #[serde(default)]
    network: Vec<String>,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    files: FileRequest,
}

impl Manifest {
    /// Parses and validates the text of an `agent.toml`.
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        if text.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge { bytes: text.len() });
        }
        let raw: RawManifest = toml::from_str(text).map_err(|error| ManifestError::Format {
            position: error.span().map(|span| position(text, span.start)),
            // Not the error's own Display, because that repeats the manifest's text.
            message: printable(error.message()),
        })?;

        check_name(&raw.agent.name)?;
        let version = Version::parse(&raw.agent.version)
            .map_err(|error| invalid("agent.version", format!("not a valid version: {error}")))?;

        check_range("limits.memory_mb", raw.limits.memory_mb, MEMORY_MB_RANGE)?;
        check_range(
            "limits.run_seconds",
            raw.limits.run_seconds,
            RUN_SECONDS_RANGE,
        )?;
        check_range("limits.disk_mb", raw.limits.disk_mb, DISK_MB_RANGE)?;
        check_range(
            "limits.model_budget_tokens",
            raw.limits.model_budget_tokens,
            MODEL_BUDGET_TOKENS_RANGE,
        )?;

        check_list(
            "requests.network",
            &raw.requests.network,
            MAX_REQUESTED_HOSTS,
            is_host_name,
            "must be a lowercase host name with a dot, and no scheme, port, path or wildcard",
        )?;
        check_list(
            "requests.models",
            &raw.requests.models,
            MAX_REQUESTED_MODELS,
            is_model_name,
            "must use lowercase letters, digits, '-', '.' or '_', and start with a letter or digit",
        )?;
        if !raw.requests.models.is_empty() && raw.limits.model_budget_tokens == 0 {
            return Err(invalid(
                "limits.model_budget_tokens",
                "must be above 0 when requests.models isn't empty".to_string(),
            ));
        }

        Ok(Self {
            name: raw.agent.name,
            version,
            limits: raw.limits,
            requested_hosts: raw.requests.network,
            requested_models: raw.requests.models,
            requested_files: raw.requests.files,
        })
    }

    /// The agent's name, which is also safe to use as a single path component.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn version(&self) -> &Version {
        &self.version
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Hosts the agent may ask to reach. This doesn't allow it to reach them.
    pub fn requested_hosts(&self) -> &[String] {
        &self.requested_hosts
    }

    /// Models the agent may ask to use. This doesn't allow it to use them.
    pub fn requested_models(&self) -> &[String] {
        &self.requested_models
    }

    /// Whether the agent may ask to read user files. This doesn't allow it to read them.
    pub fn requested_files(&self) -> FileRequest {
        self.requested_files
    }
}

fn invalid(field: &'static str, message: String) -> ManifestError {
    ManifestError::Invalid {
        field,
        message: printable(&message),
    }
}

/// Makes text from a manifest safe to print, so it can't send terminal escape codes or reorder
/// what the user sees.
pub(crate) fn printable(text: &str) -> String {
    let mut output = String::new();
    for c in text.chars().take(MAX_MESSAGE_CHARS) {
        if c.is_ascii_graphic() || c == ' ' {
            output.push(c);
        } else {
            output.extend(c.escape_unicode());
        }
    }
    if text.chars().count() > MAX_MESSAGE_CHARS {
        output.push_str("...");
    }
    output
}

fn position(text: &str, offset: usize) -> (usize, usize) {
    let before = &text.as_bytes()[..offset.min(text.len())];
    let line = before.iter().filter(|&&byte| byte == b'\n').count() + 1;
    let line_start = before
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |newline| newline + 1);
    (line, offset.min(text.len()) - line_start + 1)
}

/// Whether a name is valid for an agent. These names are safe to use as a single path component.
pub(crate) fn is_agent_name(name: &str) -> bool {
    name.len() <= MAX_NAME_CHARS
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn check_name(name: &str) -> Result<(), ManifestError> {
    if is_agent_name(name) {
        Ok(())
    } else {
        Err(invalid(
            "agent.name",
            format!(
                "{name:?} must be at most {MAX_NAME_CHARS} lowercase letters, digits and single \
                 hyphens, and start with a letter"
            ),
        ))
    }
}

fn check_range<T>(field: &'static str, value: T, (min, max): (T, T)) -> Result<(), ManifestError>
where
    T: PartialOrd + fmt::Display,
{
    if value < min || value > max {
        return Err(invalid(
            field,
            format!("{value} is outside the allowed range of {min} to {max}"),
        ));
    }
    Ok(())
}

fn check_list(
    field: &'static str,
    values: &[String],
    max: usize,
    is_valid: fn(&str) -> bool,
    rule: &str,
) -> Result<(), ManifestError> {
    if values.len() > max {
        return Err(invalid(
            field,
            format!("has {} entries, and the limit is {max}", values.len()),
        ));
    }
    let mut seen = HashSet::new();
    for value in values {
        if !is_valid(value) {
            return Err(invalid(field, format!("{value:?} {rule}")));
        }
        if !seen.insert(value) {
            return Err(invalid(field, format!("{value:?} is listed twice")));
        }
    }
    Ok(())
}

/// Requires a dot and a last label that isn't all digits. This rules out `localhost`, wildcards,
/// and IP addresses in forms such as `127.0.0.1` and `0x7f000001`.
pub fn is_host_name(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    host.len() <= MAX_HOST_CHARS
        && labels.len() >= 2
        && labels.iter().all(|label| is_host_label(label))
        && !labels
            .last()
            .is_some_and(|label| label.chars().all(|c| c.is_ascii_digit()))
}

fn is_host_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_HOST_LABEL_CHARS
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn is_model_name(name: &str) -> bool {
    name.len() <= MAX_NAME_CHARS
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../examples/research-helper/agent.toml");

    const MINIMAL: &str = r#"
        [agent]
        name = "hello"
        version = "0.1.0"

        [limits]
        memory_mb = 16
        run_seconds = 5
        disk_mb = 1
    "#;

    /// A valid manifest with one line replaced.
    fn with_line(old: &str, new: &str) -> String {
        let text = format!("{MINIMAL}\n[requests]\nnetwork = []\nmodels = []\nfiles = \"none\"\n");
        assert!(text.contains(old), "{old}");
        text.replace(old, new)
    }

    fn invalid_field(text: &str) -> &'static str {
        match Manifest::parse(text) {
            Err(ManifestError::Invalid { field, .. }) => field,
            other => panic!("expected an invalid field, got {other:?}"),
        }
    }

    fn format_message(text: &str) -> String {
        match Manifest::parse(text) {
            Err(ManifestError::Format { message, .. }) => message,
            other => panic!("expected a format error, got {other:?}"),
        }
    }

    #[test]
    fn example_manifest_parses() {
        let manifest = Manifest::parse(EXAMPLE).unwrap();
        assert_eq!(manifest.name(), "research-helper");
        assert_eq!(manifest.version(), &Version::new(0, 1, 0));
        assert_eq!(
            manifest.limits(),
            Limits {
                memory_mb: 256,
                run_seconds: 120,
                disk_mb: 100,
                model_budget_tokens: 50_000,
            }
        );
        assert_eq!(manifest.requested_hosts(), ["api.github.com"]);
        assert_eq!(manifest.requested_models(), ["default"]);
        assert_eq!(manifest.requested_files(), FileRequest::Ask);
    }

    #[test]
    fn leaving_out_requests_asks_for_nothing() {
        let manifest = Manifest::parse(MINIMAL).unwrap();
        assert!(manifest.requested_hosts().is_empty());
        assert!(manifest.requested_models().is_empty());
        assert_eq!(manifest.requested_files(), FileRequest::None);
        assert_eq!(manifest.limits().model_budget_tokens, 0);
    }

    #[test]
    fn missing_fields_are_rejected() {
        for (line, field) in [
            ("name = \"hello\"", "name"),
            ("version = \"0.1.0\"", "version"),
            ("memory_mb = 16", "memory_mb"),
            ("run_seconds = 5", "run_seconds"),
            ("disk_mb = 1", "disk_mb"),
        ] {
            let message = format_message(&with_line(line, ""));
            assert!(message.contains(field), "{field}: {message}");
        }
    }

    #[test]
    fn missing_sections_are_rejected() {
        assert!(format_message("[agent]\nname = \"a\"\nversion = \"1.0.0\"\n").contains("limits"));
        assert!(
            format_message("[limits]\nmemory_mb = 1\nrun_seconds = 1\ndisk_mb = 1\n")
                .contains("agent")
        );
        assert!(format_message("").contains("agent"));
    }

    #[test]
    fn unknown_fields_are_rejected_at_every_level() {
        for text in [
            format!("{MINIMAL}\n[grants]\nnetwork = [\"example.com\"]\n"),
            with_line("name = \"hello\"", "name = \"hello\"\nadmin = true"),
            with_line("memory_mb = 16", "memory_mb = 16\ngpu_mb = 16"),
            with_line("files = \"none\"", "files = \"none\"\ncamera = true"),
        ] {
            assert!(format_message(&text).contains("unknown field"), "{text}");
        }
    }

    #[test]
    fn duplicate_fields_are_rejected() {
        let text = with_line("memory_mb = 16", "memory_mb = 16\nmemory_mb = 32");
        assert!(matches!(
            Manifest::parse(&text),
            Err(ManifestError::Format { .. })
        ));
    }

    #[test]
    fn limits_at_the_ends_of_their_ranges_are_accepted() {
        for (line, values) in [
            ("memory_mb = 16", ["memory_mb = 1", "memory_mb = 4096"]),
            ("run_seconds = 5", ["run_seconds = 1", "run_seconds = 3600"]),
            ("disk_mb = 1", ["disk_mb = 1", "disk_mb = 10240"]),
        ] {
            for value in values {
                Manifest::parse(&with_line(line, value)).unwrap();
            }
        }
        let text = with_line(
            "disk_mb = 1",
            "disk_mb = 1\nmodel_budget_tokens = 100000000",
        );
        Manifest::parse(&text).unwrap();
    }

    #[test]
    fn limits_outside_their_ranges_are_rejected() {
        for (line, value, field) in [
            ("memory_mb = 16", "memory_mb = 0", "limits.memory_mb"),
            ("memory_mb = 16", "memory_mb = 4097", "limits.memory_mb"),
            ("run_seconds = 5", "run_seconds = 0", "limits.run_seconds"),
            (
                "run_seconds = 5",
                "run_seconds = 3601",
                "limits.run_seconds",
            ),
            ("disk_mb = 1", "disk_mb = 0", "limits.disk_mb"),
            ("disk_mb = 1", "disk_mb = 10241", "limits.disk_mb"),
            (
                "disk_mb = 1",
                "disk_mb = 1\nmodel_budget_tokens = 100000001",
                "limits.model_budget_tokens",
            ),
        ] {
            assert_eq!(invalid_field(&with_line(line, value)), field, "{value}");
        }
    }

    #[test]
    fn limits_of_the_wrong_type_or_size_are_rejected() {
        for value in [
            "memory_mb = -1",
            "memory_mb = 1.5",
            "memory_mb = \"16\"",
            "memory_mb = 4294967296",
            "memory_mb = true",
        ] {
            let text = with_line("memory_mb = 16", value);
            assert!(
                matches!(Manifest::parse(&text), Err(ManifestError::Format { .. })),
                "{value}"
            );
        }
    }

    #[test]
    fn bad_names_are_rejected() {
        let too_long = "a".repeat(MAX_NAME_CHARS + 1);
        for name in [
            "",
            "..",
            "../escape",
            "a/b",
            "a\\\\b",
            "Hello",
            "1hello",
            "-hello",
            "hello-",
            "a--b",
            "a.b",
            "a b",
            "a\\u0000b",
            "caf\u{e9}",
            &too_long,
        ] {
            let text = with_line("name = \"hello\"", &format!("name = \"{name}\""));
            assert_eq!(invalid_field(&text), "agent.name", "{name}");
        }
    }

    #[test]
    fn longest_name_is_accepted() {
        let name = "a".repeat(MAX_NAME_CHARS);
        let text = with_line("name = \"hello\"", &format!("name = \"{name}\""));
        assert_eq!(Manifest::parse(&text).unwrap().name(), name);
    }

    #[test]
    fn bad_versions_are_rejected() {
        for version in ["", "1", "1.0", "v1.0.0", "01.0.0", "1.0.0.0", "latest"] {
            let text = with_line("version = \"0.1.0\"", &format!("version = \"{version}\""));
            assert_eq!(invalid_field(&text), "agent.version", "{version}");
        }
    }

    #[test]
    fn valid_hosts_are_accepted() {
        for host in [
            "example.com",
            "api.example.co.uk",
            "a-b.example.com",
            "x1.io",
        ] {
            let text = with_line("network = []", &format!("network = [\"{host}\"]"));
            assert_eq!(Manifest::parse(&text).unwrap().requested_hosts(), [host]);
        }
    }

    #[test]
    fn bad_hosts_are_rejected() {
        let long_label = format!("{}.com", "a".repeat(MAX_HOST_LABEL_CHARS + 1));
        let long_host = format!("{}.com", "a.".repeat(MAX_HOST_CHARS / 2));
        for host in [
            "",
            "localhost",
            "com",
            "*",
            "*.example.com",
            "https://example.com",
            "example.com/path",
            "example.com:443",
            "user@example.com",
            "Example.com",
            "example.com.",
            ".example.com",
            "a..example.com",
            "-a.example.com",
            "a-.example.com",
            "ex ample.com",
            "ex\\u0000ample.com",
            "ex\u{e4}mple.com",
            "127.0.0.1",
            "1.1.1.1",
            "0x7f000001",
            "2130706433",
            "::1",
            "[::1]",
            &long_label,
            &long_host,
        ] {
            let text = with_line("network = []", &format!("network = [\"{host}\"]"));
            assert_eq!(invalid_field(&text), "requests.network", "{host}");
        }
    }

    #[test]
    fn duplicate_requests_are_rejected() {
        let text = with_line("network = []", "network = [\"a.com\", \"a.com\"]");
        assert_eq!(invalid_field(&text), "requests.network");
    }

    #[test]
    fn too_many_requests_are_rejected() {
        let hosts = |count: usize| {
            let list: Vec<String> = (0..count)
                .map(|i| format!("\"h{i}.example.com\""))
                .collect();
            with_line("network = []", &format!("network = [{}]", list.join(", ")))
        };
        Manifest::parse(&hosts(MAX_REQUESTED_HOSTS)).unwrap();
        assert_eq!(
            invalid_field(&hosts(MAX_REQUESTED_HOSTS + 1)),
            "requests.network"
        );
    }

    #[test]
    fn requesting_models_needs_a_budget() {
        let models = with_line("models = []", "models = [\"default\"]");
        assert_eq!(invalid_field(&models), "limits.model_budget_tokens");

        let with_budget = models.replace("disk_mb = 1", "disk_mb = 1\nmodel_budget_tokens = 10");
        assert_eq!(
            Manifest::parse(&with_budget).unwrap().requested_models(),
            ["default"]
        );
    }

    #[test]
    fn bad_model_names_are_rejected() {
        let budget = with_line("disk_mb = 1", "disk_mb = 1\nmodel_budget_tokens = 10");
        for model in ["", "-a", "A", "a/b", "../a", "a b"] {
            let text = budget.replace("models = []", &format!("models = [\"{model}\"]"));
            assert_eq!(invalid_field(&text), "requests.models", "{model}");
        }
    }

    #[test]
    fn file_request_only_accepts_known_values() {
        for value in ["all", "yes", "Ask", "NONE", ""] {
            let text = with_line("files = \"none\"", &format!("files = \"{value}\""));
            assert!(
                matches!(Manifest::parse(&text), Err(ManifestError::Format { .. })),
                "{value}"
            );
        }
        let text = with_line("files = \"none\"", "files = true");
        assert!(matches!(
            Manifest::parse(&text),
            Err(ManifestError::Format { .. })
        ));
    }

    #[test]
    fn oversized_manifests_are_rejected_before_parsing() {
        let padded = format!("{MINIMAL}{}", "#".repeat(MAX_MANIFEST_BYTES));
        assert!(matches!(
            Manifest::parse(&padded),
            Err(ManifestError::TooLarge { .. })
        ));
    }

    #[test]
    fn syntax_errors_give_a_position() {
        let error = Manifest::parse("[agent]\nname = \n").unwrap_err();
        assert!(
            matches!(
                error,
                ManifestError::Format {
                    position: Some((2, _)),
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(error.to_string().starts_with("line 2, column "), "{error}");
    }

    #[test]
    fn deep_nesting_fails_cleanly() {
        let text = format!("x = {}{}", "[".repeat(50_000), "]".repeat(50_000));
        assert!(text.len() > MAX_MANIFEST_BYTES);
        assert!(Manifest::parse(&text).is_err());

        let within_limit = format!("x = {}{}", "[".repeat(30_000), "]".repeat(30_000));
        assert!(within_limit.len() <= MAX_MANIFEST_BYTES);
        assert!(Manifest::parse(&within_limit).is_err());
    }

    #[test]
    fn errors_never_print_control_or_invisible_characters() {
        // An escape code in a quoted key, in a value, and a right-to-left override in a value.
        for text in [
            format!("{MINIMAL}\n[\"\\u001b[31mfake\"]\nx = 1\n"),
            format!("{MINIMAL}\n\"\u{1b}[31mfake\" = 1\n"),
            with_line("name = \"hello\"", "name = \"\\u001b[31mfake\""),
            with_line("name = \"hello\"", "name = \"a\u{202e}b\""),
            with_line("network = []", "network = [\"\\u001b[31m.com\"]"),
            with_line("version = \"0.1.0\"", "version = \"\\u001b[31m\""),
        ] {
            let message = Manifest::parse(&text).unwrap_err().to_string();
            assert!(
                message.chars().all(|c| c.is_ascii_graphic() || c == ' '),
                "{message:?}"
            );
        }
    }

    #[test]
    fn long_values_are_cut_short_in_errors() {
        let name = "A".repeat(10_000);
        let text = with_line("name = \"hello\"", &format!("name = \"{name}\""));
        let message = Manifest::parse(&text).unwrap_err().to_string();
        assert!(message.len() < 400, "{} bytes", message.len());
    }
}
