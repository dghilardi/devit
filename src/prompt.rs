use anyhow::{Context, Result};
use inquire::{Confirm, Select};
use std::io::{self, IsTerminal};

/// Why a decision cannot be put to a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonInteractiveReason {
    Flag,
    NoTty,
}

impl NonInteractiveReason {
    fn describe(self) -> &'static str {
        match self {
            Self::Flag => "--non-interactive was passed",
            Self::NoTty => "stdin is not a terminal",
        }
    }
}

/// Whether prompts may be shown, and what to report when they may not.
///
/// Every prompt in the CLI goes through here so that a run without a human
/// attached fails naming the decision it could not ask, instead of surfacing
/// `The input device is not a TTY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptPolicy {
    blocked: Option<NonInteractiveReason>,
}

impl PromptPolicy {
    pub fn resolve(non_interactive: bool) -> Self {
        Self::new(non_interactive, io::stdin().is_terminal())
    }

    /// Split out from `resolve` so the terminal state can be supplied in tests.
    pub fn new(non_interactive: bool, stdin_is_terminal: bool) -> Self {
        let blocked = if non_interactive {
            Some(NonInteractiveReason::Flag)
        } else if !stdin_is_terminal {
            Some(NonInteractiveReason::NoTty)
        } else {
            None
        };

        Self { blocked }
    }

    pub fn is_interactive(self) -> bool {
        self.blocked.is_none()
    }

    /// Reports the decision that would have been asked, and how to supply it up front.
    pub fn cannot_ask(self, decision: &str, remedy: &str) -> anyhow::Error {
        let reason = self
            .blocked
            .map(NonInteractiveReason::describe)
            .unwrap_or("prompts are unavailable");

        anyhow::anyhow!("Cannot ask for {decision}: {reason}.\n{remedy}")
    }

    /// A yes/no confirmation guarding an action.
    pub fn confirm(
        self,
        decision: &str,
        question: &str,
        default: bool,
        remedy: &str,
    ) -> Result<bool> {
        if !self.is_interactive() {
            return Err(self.cannot_ask(decision, remedy));
        }

        Confirm::new(question)
            .with_default(default)
            .prompt()
            .with_context(|| format!("Asking for {decision} was cancelled"))
    }

    /// A choice among candidates.
    ///
    /// Never answered on the caller's behalf, not even in dry-run: an arbitrary
    /// pick would silently target something the caller did not ask for.
    pub fn select(
        self,
        decision: &str,
        question: &str,
        options: Vec<String>,
        remedy: &str,
    ) -> Result<String> {
        if !self.is_interactive() {
            return Err(self.cannot_ask(decision, remedy));
        }

        Select::new(question, options)
            .prompt()
            .with_context(|| format!("Asking for {decision} was cancelled"))
    }
}

/// Renders candidate values as an indented list for an error message.
pub fn format_candidates<S: AsRef<str>>(items: &[S]) -> String {
    items
        .iter()
        .map(|item| format!("  - {}", item.as_ref()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flag_blocks_prompts_even_on_a_terminal() {
        let policy = PromptPolicy::new(true, true);
        assert!(!policy.is_interactive());
    }

    #[test]
    fn test_missing_terminal_blocks_prompts() {
        assert!(!PromptPolicy::new(false, false).is_interactive());
    }

    #[test]
    fn test_terminal_without_flag_allows_prompts() {
        assert!(PromptPolicy::new(false, true).is_interactive());
    }

    #[test]
    fn test_error_names_the_decision_and_the_flag_reason() {
        let err = PromptPolicy::new(true, true).cannot_ask("a service", "Pass --service.");
        assert_eq!(
            err.to_string(),
            "Cannot ask for a service: --non-interactive was passed.\nPass --service."
        );
    }

    #[test]
    fn test_error_distinguishes_a_missing_terminal_from_the_flag() {
        let err = PromptPolicy::new(false, false).cannot_ask("an image tag", "Pass --tag.");
        assert_eq!(
            err.to_string(),
            "Cannot ask for an image tag: stdin is not a terminal.\nPass --tag."
        );
    }

    #[test]
    fn test_confirm_fails_without_prompting() {
        let err = PromptPolicy::new(true, true)
            .confirm("drift approval", "Continue?", false, "Resolve the drift.")
            .unwrap_err();
        assert!(err.to_string().contains("Cannot ask for drift approval"));
    }

    #[test]
    fn test_select_fails_without_prompting() {
        let err = PromptPolicy::new(true, true)
            .select(
                "a service",
                "Select:",
                vec!["a".to_string()],
                "Pass --service.",
            )
            .unwrap_err();
        assert!(err.to_string().contains("Cannot ask for a service"));
    }

    #[test]
    fn test_format_candidates_indents_each_entry() {
        assert_eq!(
            format_candidates(&["svc-api (no-namespace)", "svc-api (tenant-b)"]),
            "  - svc-api (no-namespace)\n  - svc-api (tenant-b)"
        );
    }
}
