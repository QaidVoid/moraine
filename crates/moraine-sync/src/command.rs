//! An injectable runner for external commands.
//!
//! Backends shell out to `rsync`, `git`, `emerge-webrsync`, and `gpg`. To keep
//! them testable without a network or real tooling, every invocation goes
//! through the [`CommandRunner`] trait. Production code uses [`SystemRunner`],
//! which spawns the process with [`std::process::Command`]. Tests substitute a
//! fake that records the requested invocations and returns scripted results, so
//! they can assert argument construction and drive change-detection and
//! verification gating deterministically.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::SyncError;

/// A single external command invocation: the program, its arguments, and an
/// optional working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// The program to run, for example `rsync` or `git`.
    pub program: String,
    /// The arguments passed to the program, in order.
    pub args: Vec<String>,
    /// The working directory to run the command in, if any.
    pub cwd: Option<PathBuf>,
}

impl CommandSpec {
    /// Build a command for `program` with no arguments and no working directory.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
        }
    }

    /// Append a single argument.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Append several arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set the working directory.
    pub fn cwd(mut self, dir: impl AsRef<Path>) -> Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }
}

/// The captured result of an external command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// The process exit status code, or `None` when terminated by a signal.
    pub code: Option<i32>,
    /// The captured standard output, decoded lossily as UTF-8.
    pub stdout: String,
    /// The captured standard error, decoded lossily as UTF-8.
    pub stderr: String,
}

impl CommandOutput {
    /// Whether the command exited successfully (status code zero).
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Runs external commands. Implementations must be `Send + Sync` so the engine
/// can drive backends from a thread pool.
pub trait CommandRunner: Send + Sync {
    /// Run `spec` to completion and capture its output.
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, SyncError>;
}

impl<R: CommandRunner + ?Sized> CommandRunner for std::sync::Arc<R> {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, SyncError> {
        (**self).run(spec)
    }
}

impl<R: CommandRunner + ?Sized> CommandRunner for &R {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, SyncError> {
        (**self).run(spec)
    }
}

/// The production [`CommandRunner`] that spawns real processes.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, SyncError> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args);
        if let Some(dir) = &spec.cwd {
            cmd.current_dir(dir);
        }
        let output = cmd.output().map_err(|source| SyncError::Command {
            program: spec.program.clone(),
            reason: source.to_string(),
        })?;
        Ok(CommandOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A scripted [`CommandRunner`] for tests.

    use std::sync::Mutex;

    use super::{CommandOutput, CommandRunner, CommandSpec};
    use crate::error::SyncError;

    /// A matcher and the scripted result it produces.
    type Rule = Box<dyn Fn(&CommandSpec) -> Option<Result<CommandOutput, SyncError>> + Send + Sync>;

    /// A [`CommandRunner`] that records invocations and replies from rules.
    #[derive(Default)]
    pub struct FakeRunner {
        rules: Vec<Rule>,
        calls: Mutex<Vec<CommandSpec>>,
    }

    impl FakeRunner {
        /// A runner with no rules; every invocation produces an error.
        pub fn new() -> Self {
            Self::default()
        }

        /// Add a rule. The first rule whose closure returns `Some` for an
        /// invocation supplies its result.
        pub fn rule<F>(mut self, f: F) -> Self
        where
            F: Fn(&CommandSpec) -> Option<Result<CommandOutput, SyncError>> + Send + Sync + 'static,
        {
            self.rules.push(Box::new(f));
            self
        }

        /// The invocations recorded so far, in order.
        pub fn calls(&self) -> Vec<CommandSpec> {
            self.calls.lock().expect("calls lock").clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, SyncError> {
            self.calls.lock().expect("calls lock").push(spec.clone());
            for rule in &self.rules {
                if let Some(result) = rule(spec) {
                    return result;
                }
            }
            Err(SyncError::Command {
                program: spec.program.clone(),
                reason: format!("no rule matched {:?}", spec.args),
            })
        }
    }
}
