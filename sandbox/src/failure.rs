//! The two ways the wrapper itself fails, with their fixed exit codes.

/// Exit code for a command line the wrapper cannot understand.
pub const USAGE: i32 = 2;
/// Exit code when the policy cannot be enforced on this host; the tool never runs unconfined.
pub const UNENFORCEABLE: i32 = 101;

/// A wrapper failure: one stderr line and an exit code, never a tool run.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// Bad arguments (exit 2).
    Usage(String),
    /// The policy cannot be enforced here (exit 101).
    Unenforceable(String),
}

impl Failure {
    /// The process exit code for this failure.
    pub fn code(&self) -> i32 {
        match self {
            Failure::Usage(_) => USAGE,
            Failure::Unenforceable(_) => UNENFORCEABLE,
        }
    }

    /// The one-line message, without the `cs-sandbox:` prefix.
    pub fn message(&self) -> &str {
        match self {
            Failure::Usage(text) | Failure::Unenforceable(text) => text,
        }
    }
}

/// Shorthand for a usage failure.
pub fn usage(text: impl Into<String>) -> Failure {
    Failure::Usage(text.into())
}

/// Shorthand for an unenforceable-policy failure.
pub fn unenforceable(text: impl Into<String>) -> Failure {
    Failure::Unenforceable(text.into())
}
