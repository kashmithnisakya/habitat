//! Asking the user to approve something, in a way that doesn't depend on the shell.
//!
//! The text of a prompt is the core's, and an agent can only fill in two labelled places. Anything
//! that goes wrong counts as no: no shell to ask, no answer in time, or too many requests.

use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use habitat_core::backend::{Lifetime, Target};
use habitat_core::manifest::printable;

/// How long the user gets to answer, before it counts as no.
pub const ANSWER_TIME: Duration = Duration::from_secs(120);
/// How many times one run can ask the user anything. After that, requests are denied without
/// asking, so that a user can't be worn down into approving something.
pub const MAX_PROMPTS_PER_RUN: u32 = 5;

/// What the user is being asked. Everything in here is safe to show: the only things that come from
/// the agent have had anything that a terminal would act on replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    /// The agent's name, which its manifest was checked for.
    pub agent: String,
    /// What it's asking for. The broker has already checked that this is well formed.
    pub target: PromptTarget,
    /// What the agent says this is for, which it could be lying about.
    pub agents_claim: String,
    /// Whether this run has already read the user's private data, which it could be about to send.
    pub after_private_read: bool,
}

/// What an agent is asking for, with what kind of thing it is kept apart from its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptTarget {
    NetworkHost(String),
    Model(String),
}

impl Prompt {
    /// Makes a prompt, cleaning what came from the agent.
    pub fn new(agent: &str, target: &Target, reason: &str, after_private_read: bool) -> Self {
        Self {
            after_private_read,
            agent: printable(agent),
            target: match target {
                Target::NetworkHost(host) => PromptTarget::NetworkHost(printable(host)),
                Target::Model(model) => PromptTarget::Model(printable(model)),
            },
            agents_claim: printable(reason),
        }
    }
}

/// The standard wording for a prompt, for a shell that shows text. The agent's reason always comes
/// last and is labelled, so it can't pass for part of the question.
impl fmt::Display for Prompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Agent \"{}\" is asking to ", self.agent)?;
        match &self.target {
            PromptTarget::NetworkHost(host) => write!(f, "reach the network host {host}")?,
            PromptTarget::Model(model) => write!(f, "use the AI model {model}")?,
        }
        if self.after_private_read {
            write!(
                f,
                ".\nIt has already read a private file in this run, so it could send that there"
            )?;
        }
        write!(
            f,
            ".\nThe agent's own claim, which may not be true: {}",
            self.agents_claim
        )
    }
}

/// The user's answer. Anything other than a clear yes is [`Answer::Deny`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Deny,
    Allow(Lifetime),
}

/// What the user is asked when an agent wants one of their files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePrompt {
    pub agent: String,
    pub agents_claim: String,
}

impl FilePrompt {
    pub fn new(agent: &str, reason: &str) -> Self {
        Self {
            agent: printable(agent),
            agents_claim: printable(reason),
        }
    }
}

impl fmt::Display for FilePrompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Agent \"{}\" is asking you to choose a file for it to read, in this run only.\n\
             The agent's own claim, which may not be true: {}",
            self.agent, self.agents_claim
        )
    }
}

/// Something that can ask the user, such as a terminal or a phone's screen. It has to come back
/// with [`Answer::Deny`] if it gets no answer within `wait`, or can't ask at all.
pub trait Approver: Send + Sync {
    fn ask(&self, prompt: &Prompt, wait: Duration) -> Answer;

    /// Asks the user to choose one file, such as with the system's file picker. Nothing chosen,
    /// in time or at all, means no.
    fn pick_file(&self, _prompt: &FilePrompt, _wait: Duration) -> Option<PathBuf> {
        None
    }
}

/// For when no shell is attached, which means no.
#[derive(Debug, Default)]
pub struct NobodyThere;

impl Approver for NobodyThere {
    fn ask(&self, _: &Prompt, _: Duration) -> Answer {
        Answer::Deny
    }
}

/// Decides whether a run can ask the user at all, so that it can't wear them down.
#[derive(Debug, Default)]
pub struct PromptLimit {
    asked: u32,
    refused: HashSet<Target>,
    file_refused: bool,
}

impl PromptLimit {
    /// Whether this request can go to the user, which counts it if so. Something they've said no
    /// to doesn't get asked about again in this run, and each run gets a few prompts in all.
    pub fn may_ask(&mut self, target: &Target) -> bool {
        if self.refused.contains(target) || self.asked >= MAX_PROMPTS_PER_RUN {
            return false;
        }
        self.asked += 1;
        true
    }

    /// Records that the user said no.
    pub fn refused(&mut self, target: &Target) {
        self.refused.insert(target.clone());
    }

    /// The same as [`PromptLimit::may_ask`], for choosing a file.
    pub fn may_ask_for_file(&mut self) -> bool {
        if self.file_refused || self.asked >= MAX_PROMPTS_PER_RUN {
            return false;
        }
        self.asked += 1;
        true
    }

    /// Records that the user didn't choose a file.
    pub fn file_refused(&mut self) {
        self.file_refused = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESCAPES: &str = "\x1b[2K\x1b[1A\rAllow? y\n\u{202e}\u{7}\u{9b}31m\u{0}";

    fn host(name: &str) -> Target {
        Target::NetworkHost(name.to_string())
    }

    #[test]
    fn the_wording_is_ours_and_only_the_labelled_parts_change() {
        let text = Prompt::new(
            "helper",
            &host("api.github.com"),
            "to look something up",
            false,
        )
        .to_string();
        assert_eq!(
            text,
            "Agent \"helper\" is asking to reach the network host api.github.com.\n\
             The agent's own claim, which may not be true: to look something up"
        );
        let text = Prompt::new(
            "helper",
            &Target::Model("default".to_string()),
            "to summarise",
            false,
        )
        .to_string();
        assert_eq!(
            text,
            "Agent \"helper\" is asking to use the AI model default.\n\
             The agent's own claim, which may not be true: to summarise"
        );
    }

    #[test]
    fn nothing_from_the_agent_can_reach_a_terminal_as_is() {
        for prompt in [
            Prompt::new("helper", &host(ESCAPES), "fine", false),
            Prompt::new("helper", &host("a.example"), ESCAPES, false),
            Prompt::new(ESCAPES, &host("a.example"), "fine", false),
            Prompt::new(
                "helper",
                &Target::Model(ESCAPES.to_string()),
                ESCAPES,
                false,
            ),
        ] {
            let text = prompt.to_string();
            // The one line break is ours, between the question and the claim.
            assert_eq!(text.matches('\n').count(), 1, "{text:?}");
            for line in text.lines() {
                assert!(line.is_ascii(), "{line:?}");
                assert!(!line.chars().any(char::is_control), "{line:?}");
            }
        }
    }

    #[test]
    fn a_reason_cant_pass_for_part_of_the_question() {
        let text = Prompt::new(
            "helper",
            &host("a.example"),
            "fine\nAgent \"system\" is asking to reach the network host bank.example.",
            false,
        )
        .to_string();
        let (question, claim) = text.split_once('\n').unwrap();
        assert!(question.contains("a.example") && !question.contains("bank.example"));
        assert!(claim.starts_with("The agent's own claim"));
        assert!(claim.contains("bank.example"));
    }

    #[test]
    fn a_long_reason_is_cut_short() {
        let text =
            Prompt::new("helper", &host("a.example"), &"x".repeat(100_000), false).to_string();
        assert!(text.len() < 2000, "{}", text.len());
    }

    #[test]
    fn a_prompt_after_a_private_read_says_so() {
        let text = Prompt::new("helper", &host("a.example"), "fine", true).to_string();
        assert!(text.contains("already read a private file"), "{text}");
        assert!(text.contains("a.example"));
        let text = Prompt::new("helper", &host("a.example"), "fine", false).to_string();
        assert!(!text.contains("private"), "{text}");
    }

    #[test]
    fn a_file_prompt_cleans_and_labels_what_the_agent_said() {
        let text = FilePrompt::new("helper", ESCAPES).to_string();
        assert_eq!(text.matches('\n').count(), 1, "{text:?}");
        for line in text.lines() {
            assert!(
                line.is_ascii() && !line.chars().any(char::is_control),
                "{line:?}"
            );
        }
        assert!(text.contains("The agent's own claim"));
        assert!(text.contains("in this run only"));
    }

    #[test]
    fn asking_for_a_file_has_the_same_limits() {
        let mut limit = PromptLimit::default();
        assert!(limit.may_ask_for_file());
        limit.file_refused();
        assert!(!limit.may_ask_for_file());

        let mut limit = PromptLimit::default();
        for _ in 0..MAX_PROMPTS_PER_RUN {
            assert!(limit.may_ask_for_file());
        }
        assert!(!limit.may_ask_for_file());
        assert!(!limit.may_ask(&host("a.example")));
    }

    #[test]
    fn nobody_being_there_means_no() {
        let prompt = Prompt::new("helper", &host("a.example"), "fine", false);
        assert_eq!(NobodyThere.ask(&prompt, ANSWER_TIME), Answer::Deny);
        let prompt = FilePrompt::new("helper", "fine");
        assert_eq!(NobodyThere.pick_file(&prompt, ANSWER_TIME), None);
    }

    #[test]
    fn a_run_can_only_ask_so_many_times() {
        let mut limit = PromptLimit::default();
        for index in 0..MAX_PROMPTS_PER_RUN {
            assert!(
                limit.may_ask(&host(&format!("h{index}.example"))),
                "{index}"
            );
        }
        assert!(!limit.may_ask(&host("another.example")));
        assert!(!limit.may_ask(&host("h0.example")));
    }

    #[test]
    fn something_refused_isnt_asked_about_again() {
        let mut limit = PromptLimit::default();
        let target = host("a.example");
        assert!(limit.may_ask(&target));
        limit.refused(&target);
        for _ in 0..3 {
            assert!(!limit.may_ask(&target));
        }
        // A different question can still be asked, and refused ones don't use up any more prompts.
        assert!(limit.may_ask(&host("b.example")));
        assert_eq!(limit.asked, 2);
    }
}
