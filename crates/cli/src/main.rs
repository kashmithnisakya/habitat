//! Command line for Habitat.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{self, ExitCode};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};
use habitat_backend_wasm::WasmBackend;
use habitat_broker::approval::{Answer, Approver, FilePrompt, Prompt};
use habitat_broker::audit;
use habitat_broker::broker::Broker;
use habitat_broker::grants::{list_saved, revoke_saved};
use habitat_broker::policy::Target;
use habitat_broker::vault::{NewSecret, OsKeystore, SecretValue, Vault, VaultError};
use habitat_core::backend::{Event, HostLimits, Lifetime, RunOutcome};
use habitat_core::home::{AgentStorage, HomeError};
use habitat_core::manifest::MAX_MANIFEST_BYTES;
use habitat_core::package::{MAX_COMPONENT_BYTES, Package};
use habitat_core::run_log::{RunEnding, RunRecord};
use habitat_core::supervisor::{Supervisor, SupervisorError};

const FAILURE: u8 = 1;
const AGENT_FAILED: u8 = 3;
const AGENT_NOT_STARTED: u8 = 4;
const AGENT_BUSY: u8 = 5;
const STOPPED: u8 = 130;

/// How long a running agent gets to stop, after Ctrl-C or a request to stop. An agent stops within
/// a moment, but a compile can't be interrupted, so this doesn't wait for the end of one.
const STOP_WAIT: Duration = Duration::from_secs(5);

const EXIT_CODES: &str = "\
Exit codes:
  0    It worked
  1    It failed, or was cancelled
  2    The command line was wrong
  3    The agent ran, and failed, crashed or hit a limit
  4    The agent's code never started
  5    The agent is already running
  130  The agent was stopped by Ctrl-C or a request to stop";

/// Runs AI agents in a secure home of their own.
///
/// Early development: don't rely on this to protect anything yet.
#[derive(Parser)]
#[command(name = "habitat", version, after_help = EXIT_CODES)]
struct Cli {
    /// The folder that holds all agents and their files
    #[arg(long, global = true, env = "HABITAT_HOME", value_name = "FOLDER")]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Makes a package file from a manifest and a WebAssembly component
    Pack {
        /// The agent's agent.toml
        manifest: PathBuf,
        /// The agent's WebAssembly component
        component: PathBuf,
        /// The package file to make, which can't exist yet
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Installs an agent from a package file
    Install {
        /// The package file
        package: PathBuf,
    },
    /// Runs an agent for one message, and waits for it to end
    Run {
        /// The agent's name
        agent: String,
        /// The message for the agent to handle
        message: String,
    },
    /// Lists the installed agents
    List,
    /// Lists what an agent has been allowed until revoked
    Grants {
        /// The agent's name
        agent: String,
    },
    /// Removes something that an agent has been allowed until revoked
    #[command(group(clap::ArgGroup::new("target").required(true)))]
    Revoke {
        /// The agent's name
        agent: String,
        /// The network host to remove
        #[arg(long, value_name = "HOST", group = "target")]
        network: Option<String>,
        /// The AI model to remove
        #[arg(long, value_name = "NAME", group = "target")]
        model: Option<String>,
    },
    /// Adds, lists and removes secrets such as API keys, which agents can never read
    #[command(subcommand)]
    Secret(SecretCommand),
    /// Shows everything an agent has asked for outside its home, after checking the log
    Log {
        /// The agent's name
        agent: String,
        /// Only says whether the log has been changed
        #[arg(long)]
        verify: bool,
    },
    /// Deletes an agent and all its files
    Remove {
        /// The agent's name
        agent: String,
        /// Doesn't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum SecretCommand {
    /// Adds a secret to the system's keystore. Its value is asked for, and never goes on the
    /// command line, where other programs could see it
    Add {
        /// A name for the secret
        name: String,
        /// A host that it can be sent to, which can be given more than once
        #[arg(long = "host", value_name = "HOST", required = true)]
        hosts: Vec<String>,
        /// The header to send it in: authorization, x-api-key or api-key
        #[arg(long, value_name = "NAME", default_value = "authorization")]
        header: String,
        /// What goes before the value in the header
        #[arg(long, value_name = "TEXT", default_value = "")]
        prefix: String,
    },
    /// Lists the secrets, without their values
    List,
    /// Removes a secret
    Remove {
        /// The secret's name
        name: String,
    },
}

/// Why a command failed, and the exit code to show for it.
struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn general(message: impl Into<String>) -> Self {
        Self::new(FAILURE, message)
    }
}

impl From<SupervisorError> for Failure {
    fn from(error: SupervisorError) -> Self {
        let code = match error {
            SupervisorError::AlreadyRunning => AGENT_BUSY,
            SupervisorError::ShuttingDown => STOPPED,
            _ => FAILURE,
        };
        Self::new(code, error.to_string())
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run_command(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("error: {}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}

fn run_command(cli: Cli) -> Result<(), Failure> {
    // This one doesn't touch any agents, so it doesn't need to know where they are.
    if let Command::Pack {
        manifest,
        component,
        output,
    } = &cli.command
    {
        return pack(manifest, component, output);
    }

    let root = match cli.root {
        Some(root) => root,
        None => default_root().map_err(Failure::general)?,
    };
    let backend = WasmBackend::new().map_err(|error| Failure::general(error.to_string()))?;
    let supervisor = Supervisor::new(&root, backend, HostLimits::default())
        .with_broker(Broker::new(Terminal).with_vault(Vault::new(&root, OsKeystore)));
    match cli.command {
        Command::Pack { .. } => unreachable!("handled above"),
        Command::Install { package } => install(&supervisor, &package),
        Command::Run { agent, message } => run(supervisor, &agent, message),
        Command::List => list(&supervisor),
        Command::Grants { agent } => grants(&root, &agent),
        Command::Revoke {
            agent,
            network,
            model,
        } => {
            // The command line makes sure there's exactly one.
            let target = match (network, model) {
                (Some(host), _) => Target::NetworkHost(host),
                (None, Some(model)) => Target::Model(model),
                (None, None) => unreachable!("the command line requires one of these"),
            };
            revoke(&root, &agent, &target)
        }
        Command::Log { agent, verify } => log(&root, &agent, verify),
        Command::Secret(command) => secret(&root, command),
        Command::Remove { agent, yes } => remove(&supervisor, &agent, yes),
    }
}

/// Where agents are kept unless told otherwise, following each platform's convention.
fn default_root() -> Result<PathBuf, String> {
    const NOT_SET: &str = "the home folder isn't known, so use --root to say where agents are kept";
    let set = |name| env::var_os(name).filter(|value: &std::ffi::OsString| !value.is_empty());
    if cfg!(windows) {
        let data = set("LOCALAPPDATA").map(PathBuf::from).ok_or(NOT_SET)?;
        return Ok(data.join("habitat"));
    }
    let home = set("HOME").map(PathBuf::from).ok_or(NOT_SET)?;
    if cfg!(target_os = "macos") {
        return Ok(home.join("Library/Application Support/habitat"));
    }
    let data = set("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|data| data.is_absolute())
        .unwrap_or_else(|| home.join(".local/share"));
    Ok(data.join("habitat"))
}

/// Printing to something that's gone away, as in `habitat list | head -1`, shouldn't be an error.
fn say(text: impl std::fmt::Display) {
    let _ = writeln!(io::stdout(), "{text}");
}

fn describe_path(path: &Path, error: &io::Error) -> String {
    format!("{}: {}", path.display(), error.kind())
}

/// Reads a file that could be anything, stopping as soon as it's over its limit.
fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>, Failure> {
    let file = File::open(path).map_err(|error| Failure::general(describe_path(path, &error)))?;
    let mut data = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut data)
        .map_err(|error| Failure::general(describe_path(path, &error)))?;
    if data.len() as u64 > limit {
        return Err(Failure::general(format!(
            "{} is over the limit of {limit} bytes",
            path.display()
        )));
    }
    Ok(data)
}

fn pack(manifest: &Path, component: &Path, output: &Path) -> Result<(), Failure> {
    let manifest_text = read_limited(manifest, MAX_MANIFEST_BYTES as u64)?;
    let manifest_text = String::from_utf8(manifest_text)
        .map_err(|_| Failure::general(format!("{} isn't UTF-8", manifest.display())))?;
    let code = read_limited(component, MAX_COMPONENT_BYTES)?;
    let package =
        Package::new(&manifest_text, code).map_err(|error| Failure::general(error.to_string()))?;

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|error| Failure::general(describe_path(output, &error)))?;
    let written = package.write(&mut file).and_then(|()| {
        file.sync_all()?;
        Ok(())
    });
    if let Err(error) = written {
        let _ = fs::remove_file(output);
        return Err(Failure::general(error.to_string()));
    }
    say(format_args!(
        "Packed {} {} into {}",
        package.manifest().name(),
        package.manifest().version(),
        output.display()
    ));
    say(format_args!("Package hash: {}", package.hash()));
    Ok(())
}

fn install(supervisor: &Supervisor, path: &Path) -> Result<(), Failure> {
    let file = File::open(path).map_err(|error| Failure::general(describe_path(path, &error)))?;
    let package = Package::read(file).map_err(|error| Failure::general(error.to_string()))?;
    supervisor.install(&package)?;
    say(format_args!(
        "Installed {} {}",
        package.manifest().name(),
        package.manifest().version()
    ));
    say(format_args!("Package hash: {}", package.hash()));
    Ok(())
}

fn run(supervisor: Supervisor, agent: &str, message: String) -> Result<(), Failure> {
    let supervisor = Arc::new(supervisor);
    stop_on_signals(Arc::clone(&supervisor))?;

    let record = match supervisor.run(agent, &Event::UserMessage(message)) {
        Ok(record) => record,
        Err(SupervisorError::EndingNotSaved(record)) => {
            let saying = format!("{}, but that couldn't be saved", describe_ending(&record));
            return Err(Failure::new(code_for(&record, &supervisor), saying));
        }
        Err(error) => return Err(error.into()),
    };
    match code_for(&record, &supervisor) {
        0 => {
            say(format_args!(
                "{} (run {})",
                describe_ending(&record),
                record.id
            ));
            Ok(())
        }
        code => Err(Failure::new(
            code,
            format!("{} (run {})", describe_ending(&record), record.id),
        )),
    }
}

/// Asks the user on the terminal. The question goes to stderr, so it can't be mixed up with any
/// output, and answers come from stdin. If that's closed or empty, the answer is no.
struct Terminal;

/// Lines from stdin, from one thread for the whole program. A read can't be stopped, so after a
/// question times out, this same thread picks up whatever's typed next.
fn stdin_lines() -> &'static Mutex<Receiver<String>> {
    static LINES: OnceLock<Mutex<Receiver<String>>> = OnceLock::new();
    LINES.get_or_init(|| {
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Mutex::new(receive)
    })
}

/// Reads an answer. Only these count, so anything else is no.
fn parse_answer(line: &str) -> Answer {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Answer::Allow(Lifetime::Once),
        "r" | "run" => Answer::Allow(Lifetime::ThisRun),
        "a" | "always" => Answer::Allow(Lifetime::UntilRevoked),
        _ => Answer::Deny,
    }
}

impl Approver for Terminal {
    /// Marks every line, so that a message can't pass for anything that Habitat said. These go to
    /// stdout, as they're what the agent's output is.
    fn show_message(&self, agent: &str, text: &str) {
        for line in text.split('\n') {
            say(format_args!("[{agent}] {line}"));
        }
    }

    fn pick_file(&self, prompt: &FilePrompt, wait: Duration) -> Option<PathBuf> {
        let _ = writeln!(
            io::stderr(),
            "\n{prompt}\nType the path of a file to allow it, or press Enter for no: "
        );
        let lines = stdin_lines().lock().unwrap_or_else(PoisonError::into_inner);
        let line = lines.recv_timeout(wait).ok()?;
        let path = line.trim();
        (!path.is_empty()).then(|| PathBuf::from(path))
    }

    fn ask(&self, prompt: &Prompt, wait: Duration) -> Answer {
        let _ = writeln!(
            io::stderr(),
            "\n{prompt}\nAllow this? [y] once, [r] for this run, [a] until revoked, [N] no: "
        );
        let lines = stdin_lines().lock().unwrap_or_else(PoisonError::into_inner);
        match lines.recv_timeout(wait) {
            Ok(line) => parse_answer(&line),
            // Either there's no answer in time, or nothing more can arrive.
            Err(_) => Answer::Deny,
        }
    }
}

/// Stops the agent on Ctrl-C or a request to stop, which on Windows includes closing the console.
/// Exiting without waiting is safe, because the agent's files are always saved whole, and its run
/// will show as interrupted.
fn stop_on_signals(supervisor: Arc<Supervisor>) -> Result<(), Failure> {
    ctrlc::set_handler(move || {
        if !supervisor.shut_down(STOP_WAIT) {
            eprintln!("error: stopped while the agent's code was still compiling");
            process::exit(i32::from(STOPPED));
        }
    })
    .map_err(|error| Failure::general(format!("couldn't prepare to be stopped: {error}")))
}

fn code_for(record: &RunRecord, supervisor: &Supervisor) -> u8 {
    match &record.ending {
        Some(RunEnding::Ran(RunOutcome::Completed)) => 0,
        Some(RunEnding::Ran(RunOutcome::Stopped)) => STOPPED,
        Some(RunEnding::NotStarted(_)) if supervisor.is_shutting_down() => STOPPED,
        Some(RunEnding::NotStarted(_)) => AGENT_NOT_STARTED,
        _ => AGENT_FAILED,
    }
}

/// Says how a run went. All the text in a record is safe to print.
fn describe_ending(record: &RunRecord) -> String {
    let Some(ending) = &record.ending else {
        return "running".to_string();
    };
    match ending {
        RunEnding::Interrupted => "interrupted when the host stopped".to_string(),
        RunEnding::NotStarted(reason) => format!("didn't start: {reason}"),
        RunEnding::Ran(outcome) => match outcome {
            RunOutcome::Completed => "completed".to_string(),
            RunOutcome::Failed(message) => format!("failed, and the agent said: {message}"),
            RunOutcome::TimeLimit => "stopped at its time limit".to_string(),
            RunOutcome::MemoryLimit => "crashed at its memory limit".to_string(),
            RunOutcome::Stopped => "stopped".to_string(),
            RunOutcome::Crashed(reason) => format!("crashed: {reason}"),
            RunOutcome::Internal(reason) => format!("the host failed: {reason}"),
        },
    }
}

fn list(supervisor: &Supervisor) -> Result<(), Failure> {
    let agents = supervisor.list()?;
    if agents.is_empty() {
        say("No agents are installed.");
        return Ok(());
    }
    let width = agents
        .iter()
        .map(|agent| agent.name.len())
        .max()
        .unwrap_or_default();
    let versions: Vec<String> = agents
        .iter()
        .map(|agent| {
            agent
                .version
                .as_ref()
                .map_or("unknown".to_string(), ToString::to_string)
        })
        .collect();
    let version_width = versions.iter().map(String::len).max().unwrap_or_default();
    say(format_args!(
        "{:width$}  {:version_width$}  LAST RUN",
        "NAME",
        "VERSION",
        width = width.max(4),
        version_width = version_width.max(7)
    ));
    for (agent, version) in agents.iter().zip(&versions) {
        let last_run = agent
            .last_run
            .as_ref()
            .map_or("never".to_string(), describe_ending);
        say(format_args!(
            "{:width$}  {:version_width$}  {last_run}",
            agent.name,
            version,
            width = width.max(4),
            version_width = version_width.max(7)
        ));
    }
    Ok(())
}

/// Opens an agent's folder, without creating it for a name that's just been mistyped.
fn existing_agent(root: &Path, agent: &str) -> Result<AgentStorage, Failure> {
    AgentStorage::open_existing(root, agent).map_err(|error| match error {
        HomeError::InvalidPath(_) => SupervisorError::InvalidName.into(),
        HomeError::NotFound => SupervisorError::NotInstalled.into(),
        other => Failure::general(other.to_string()),
    })
}

fn grants(root: &Path, agent: &str) -> Result<(), Failure> {
    let storage = existing_agent(root, agent)?;
    let saved = list_saved(&storage).map_err(|error| Failure::general(error.to_string()))?;
    if saved.is_empty() {
        say(format_args!("{agent} has nothing allowed until revoked."));
    }
    // Everything saved has been checked, so it's safe to print.
    for target in saved {
        match target {
            Target::NetworkHost(host) => say(format_args!("network  {host}")),
            Target::Model(model) => say(format_args!("model    {model}")),
        }
    }
    Ok(())
}

fn revoke(root: &Path, agent: &str, target: &Target) -> Result<(), Failure> {
    let storage = existing_agent(root, agent)?;
    let removed =
        revoke_saved(&storage, target).map_err(|error| Failure::general(error.to_string()))?;
    if !removed {
        return Err(Failure::general(format!(
            "{agent} isn't allowed that until revoked, so there's nothing to remove"
        )));
    }
    say("Revoked. This takes effect on the agent's next request.");
    Ok(())
}

/// Reads a secret's value without showing it. Piped input is read as it is, with one line ending
/// taken off. A hidden prompt needs a terminal.
fn read_secret_value() -> Result<SecretValue, Failure> {
    let failed = |error: io::Error| Failure::general(format!("couldn't read it: {}", error.kind()));
    let mut line = if io::stdin().is_terminal() {
        rpassword::prompt_password("Value (it won't be shown): ").map_err(failed)?
    } else {
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line).map_err(failed)?;
        line
    };
    if line.ends_with('\n') {
        line.pop();
    }
    if line.ends_with('\r') {
        line.pop();
    }
    Ok(SecretValue::new(line.into_bytes()))
}

fn secret(root: &Path, command: SecretCommand) -> Result<(), Failure> {
    let vault = Vault::new(root, OsKeystore);
    let failed = |error: VaultError| Failure::general(error.to_string());
    match command {
        SecretCommand::Add {
            name,
            hosts,
            header,
            prefix,
        } => {
            let value = read_secret_value()?;
            vault
                .add(NewSecret {
                    name: &name,
                    hosts: &hosts,
                    header: &header,
                    prefix: &prefix,
                    value,
                })
                .map_err(failed)?;
            say(format_args!(
                "Added {name}. It can only be sent to: {}",
                hosts.join(", ")
            ));
        }
        SecretCommand::List => {
            let secrets = vault.list().map_err(failed)?;
            if secrets.is_empty() {
                say("There are no secrets.");
            }
            for secret in secrets {
                say(format_args!(
                    "{}  {}  to {}",
                    secret.name,
                    secret.header,
                    secret.hosts.join(", ")
                ));
            }
        }
        SecretCommand::Remove { name } => {
            vault.remove(&name).map_err(failed)?;
            say(format_args!("Removed {name}."));
        }
    }
    Ok(())
}

fn log(root: &Path, agent: &str, verify_only: bool) -> Result<(), Failure> {
    let storage = existing_agent(root, agent)?;
    let entries = audit::verify(&storage).map_err(|error| Failure::general(error.to_string()))?;
    if verify_only {
        say(format_args!(
            "All {} entries check out, so the log hasn't been changed.",
            entries.len()
        ));
        say("Someone who controls this device could still have replaced all of it.");
        return Ok(());
    }
    if entries.is_empty() {
        say(format_args!("{agent} hasn't asked for anything yet."));
    }
    for entry in &entries {
        say(audit::describe(entry));
    }
    Ok(())
}

fn remove(supervisor: &Supervisor, agent: &str, yes: bool) -> Result<(), Failure> {
    // Checked first, so that nobody is asked about an agent that can't be removed anyway.
    supervisor.history(agent)?;
    if !yes && !confirmed(agent)? {
        return Err(Failure::general("cancelled, and nothing was removed"));
    }
    supervisor.remove(agent)?;
    say(format_args!("Removed {agent} and all its files"));
    Ok(())
}

/// Asks for confirmation. Only a clear yes counts, so no answer at all means no.
fn confirmed(agent: &str) -> Result<bool, Failure> {
    let _ = write!(
        io::stderr(),
        "This deletes {agent} and all its files, and it can't be undone. Continue? [y/N] "
    );
    let mut answer = String::new();
    io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|error| Failure::general(format!("couldn't read an answer: {}", error.kind())))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use habitat_core::run_log::{EventKind, RunId};
    use tempfile::TempDir;

    use super::*;

    fn record(ending: RunEnding) -> RunRecord {
        RunRecord {
            id: RunId::parse(&"0".repeat(32)).unwrap(),
            event: EventKind::UserMessage,
            started_at_ms: 0,
            ended_at_ms: None,
            ending: Some(ending),
        }
    }

    #[test]
    fn only_a_clear_answer_allows_anything() {
        for (line, answer) in [
            ("y", Answer::Allow(Lifetime::Once)),
            (" YES ", Answer::Allow(Lifetime::Once)),
            ("r", Answer::Allow(Lifetime::ThisRun)),
            ("run", Answer::Allow(Lifetime::ThisRun)),
            ("a", Answer::Allow(Lifetime::UntilRevoked)),
            ("Always", Answer::Allow(Lifetime::UntilRevoked)),
            ("", Answer::Deny),
            ("n", Answer::Deny),
            ("no", Answer::Deny),
            ("yep", Answer::Deny),
            ("y y", Answer::Deny),
            ("yes please", Answer::Deny),
            ("\u{1b}[A", Answer::Deny),
        ] {
            assert_eq!(parse_answer(line), answer, "{line:?}");
        }
    }

    #[test]
    fn a_run_that_didnt_start_because_of_stopping_counts_as_stopped() {
        let root = TempDir::new().unwrap();
        let backend = WasmBackend::new().unwrap();
        let supervisor = Supervisor::new(root.path(), backend, HostLimits::default());
        let not_started = record(RunEnding::NotStarted("a reason".to_string()));
        assert_eq!(code_for(&not_started, &supervisor), AGENT_NOT_STARTED);
        assert!(supervisor.shut_down(Duration::ZERO));
        assert_eq!(code_for(&not_started, &supervisor), STOPPED);
    }
}
