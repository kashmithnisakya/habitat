//! Command line for Habitat.

use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use habitat_backend_wasm::WasmBackend;
use habitat_core::backend::{Event, HostLimits, RunOutcome};
use habitat_core::package::Package;
use habitat_core::supervisor::{InstalledAgent, RunStatus, Supervisor};

const USAGE: &str = "\
Habitat runs AI agents inside a sandbox. This is early development, and it isn't secure yet.

Usage:
  habitat pack <agent.toml> <agent.wasm> <output>  Make a package from a manifest and its code
  habitat install <package>                        Check a package and install it
  habitat list                                     List installed agents
  habitat run <agent> <message>...                 Send an agent a message and show its reply
  habitat runs <agent>                             Show an agent's recent runs
  habitat remove <agent> [--yes]                   Delete an agent and all of its files
  habitat help                                     Show this help

Agents are stored in $HABITAT_DATA, or ~/.habitat if that isn't set.";

const RECENT_RUNS: usize = 20;
const CORE_MODULE_HEADER: &[u8] = b"\0asm\x01\0\0\0";

type CliResult = Result<ExitCode, String>;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match words.as_slice() {
        ["help" | "--help" | "-h"] => {
            println!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        ["pack", manifest, code, output] => pack(manifest, code, output),
        ["install", package] => install(package),
        ["list"] => list(),
        ["run", agent, first, rest @ ..] => run(agent, first, rest),
        ["runs", agent] => runs(agent),
        ["remove", agent] => remove(agent, false),
        ["remove", agent, "--yes"] => remove(agent, true),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    result.unwrap_or_else(|message| {
        eprintln!("error: {message}");
        ExitCode::FAILURE
    })
}

fn supervisor() -> Result<Supervisor<WasmBackend>, String> {
    let root = match env::var_os("HABITAT_DATA") {
        Some(path) => PathBuf::from(path),
        None => env::home_dir()
            .ok_or("couldn't find your home folder, so set HABITAT_DATA")?
            .join(".habitat"),
    };
    let backend = WasmBackend::new().map_err(|error| error.to_string())?;
    Ok(Supervisor::new(root, backend, HostLimits::default()))
}

fn pack(manifest: &str, code: &str, output: &str) -> CliResult {
    let manifest_text = fs::read_to_string(manifest)
        .map_err(|error| format!("couldn't read {manifest}: {error}"))?;
    let code = fs::read(code).map_err(|error| format!("couldn't read {code}: {error}"))?;
    let component = if code.starts_with(CORE_MODULE_HEADER) {
        wit_component::ComponentEncoder::default()
            .module(&code)
            .and_then(|encoder| encoder.validate(true).encode())
            .map_err(|error| format!("couldn't make a component from the code: {error:#}"))?
    } else {
        code
    };
    let package = Package::new(&manifest_text, component).map_err(|error| error.to_string())?;
    let file = File::create(output).map_err(|error| format!("couldn't write {output}: {error}"))?;
    package.write(file).map_err(|error| error.to_string())?;
    println!(
        "Packed {} {} into {output}",
        package.manifest().name(),
        package.manifest().version()
    );
    Ok(ExitCode::SUCCESS)
}

fn install(path: &str) -> CliResult {
    let file = File::open(path).map_err(|error| format!("couldn't read {path}: {error}"))?;
    let agent = supervisor()?
        .install(file)
        .map_err(|error| error.to_string())?;
    println!("Installed {} {}", agent.name, agent.version);
    Ok(ExitCode::SUCCESS)
}

fn list() -> CliResult {
    let agents = supervisor()?.agents().map_err(|error| error.to_string())?;
    if agents.is_empty() {
        println!("No agents installed.");
    }
    for agent in agents {
        match agent {
            InstalledAgent::Ready(info) => println!("{} {}", info.name, info.version),
            InstalledAgent::Damaged { name, reason } => println!("{name} (damaged: {reason})"),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run(agent: &str, first: &str, rest: &[&str]) -> CliResult {
    let text = std::iter::once(first)
        .chain(rest.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    let report = supervisor()?
        .run(agent, &Event::UserMessage(text))
        .map_err(|error| error.to_string())?;
    for message in &report.output.messages {
        for line in message.lines() {
            println!("[{agent}] {line}");
        }
    }
    match outcome_text(&report.output.outcome) {
        None => Ok(ExitCode::SUCCESS),
        Some(reason) => {
            eprintln!("error: {reason} (run {})", report.run_id);
            Ok(ExitCode::FAILURE)
        }
    }
}

fn runs(agent: &str) -> CliResult {
    let runs = supervisor()?
        .recent_runs(agent, RECENT_RUNS)
        .map_err(|error| error.to_string())?;
    if runs.is_empty() {
        println!("No runs yet.");
    }
    for run in runs {
        let status = match &run.status {
            RunStatus::Running => "still running".to_string(),
            RunStatus::Interrupted => "interrupted".to_string(),
            RunStatus::Finished(outcome) => {
                outcome_text(outcome).unwrap_or_else(|| "completed".to_string())
            }
        };
        println!("{}  {}  {status}", run.run_id, ago(run.started_ms));
    }
    Ok(ExitCode::SUCCESS)
}

fn remove(agent: &str, confirmed: bool) -> CliResult {
    if !confirmed && !confirm_removal(agent).map_err(|error| error.to_string())? {
        println!("Nothing was removed.");
        return Ok(ExitCode::FAILURE);
    }
    supervisor()?
        .remove(agent)
        .map_err(|error| error.to_string())?;
    println!("Removed {agent}");
    Ok(ExitCode::SUCCESS)
}

// Anything but a clear yes counts as no, including no answer at all.
fn confirm_removal(agent: &str) -> io::Result<bool> {
    print!("Delete {agent} and all of its files? Type yes to confirm: ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    Ok(answer.trim() == "yes")
}

fn outcome_text(outcome: &RunOutcome) -> Option<String> {
    match outcome {
        RunOutcome::Completed => None,
        RunOutcome::Failed(message) => Some(format!("the agent says it failed: {message}")),
        RunOutcome::TimeLimit => Some("the agent ran out of time and was stopped".to_string()),
        RunOutcome::MemoryLimit => Some("the agent ran out of memory".to_string()),
        RunOutcome::Crashed(reason) => Some(format!("the agent crashed: {reason}")),
        RunOutcome::Internal(reason) => Some(format!("Habitat couldn't run the agent: {reason}")),
    }
}

fn ago(time_ms: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64);
    match now.saturating_sub(time_ms) / 1000 {
        seconds @ 0..60 => format!("{seconds}s ago"),
        seconds @ 60..3600 => format!("{}m ago", seconds / 60),
        seconds @ 3600..86400 => format!("{}h ago", seconds / 3600),
        seconds => format!("{}d ago", seconds / 86400),
    }
}
