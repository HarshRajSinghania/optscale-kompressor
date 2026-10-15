//! What a command can fail with, and what the shell learns from it.
//!
//! The exit code is part of the interface: `bench replay --compare` is meant to
//! run in CI, where "the numbers got worse" and "the corpus file is missing"
//! have to be told apart without parsing the output. So a regression has a code
//! of its own, and so does a bad invocation.

use std::fmt;

/// Failure, classified by what the caller should do about it.
#[derive(Debug)]
pub enum CliError {
    /// Something went wrong while doing the work: a file that would not open, a
    /// database that would not answer, a handle that does not resolve.
    Runtime(String),
    /// The command or the configuration was wrong. Nothing was attempted.
    Usage(String),
    /// A benchmark comparison found a regression. The run itself succeeded.
    Regressed,
}

impl CliError {
    /// Process exit code: 0 success, 1 runtime, 2 usage or config, 3 regression.
    pub fn code(&self) -> i32 {
        match self {
            CliError::Runtime(_) => 1,
            CliError::Usage(_) => 2,
            CliError::Regressed => 3,
        }
    }

    /// A runtime failure from anything that renders itself.
    pub fn runtime(context: &str, error: impl fmt::Display) -> CliError {
        CliError::Runtime(format!("{context}: {error}"))
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Runtime(message) | CliError::Usage(message) => f.write_str(message),
            // The detail is in the comparison table already printed; repeating
            // it on stderr would bury the table it refers to.
            CliError::Regressed => f.write_str("benchmark comparison regressed"),
        }
    }
}

pub type CliResult = Result<(), CliError>;
