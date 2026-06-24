//! The injectable external-command surface.
//!
//! Every external process the build engine launches goes through a
//! [`CommandRunner`], so tests can substitute a fake that records invocations
//! and asserts on the constructed environment, argument list, and sandbox
//! wrapping without running real builds, downloads, or sandboxes.
//!
//! The production implementation, [`SystemRunner`], shells out with
//! [`std::process::Command`]. There is no HTTP-client, async, or sandbox-library
//! dependency: fetching is a `FETCHCOMMAND` subprocess, phases are a `bash`
//! subprocess, and the sandbox is the wrapper command list the engine prepends.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A command to run: a program, its arguments, environment, and working
/// directory, plus where to direct captured output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// The program to execute.
    pub program: String,
    /// The program arguments.
    pub args: Vec<String>,
    /// Environment variables to set for the process. The process inherits no
    /// other environment.
    pub env: BTreeMap<String, String>,
    /// The working directory for the process.
    pub cwd: PathBuf,
    /// A file to which combined stdout and stderr are appended, if any. When
    /// `None`, output is captured into [`CommandOutput::stdout`].
    pub log_path: Option<PathBuf>,
}

impl CommandSpec {
    /// Construct a command with empty args, env, and no log.
    pub fn new(program: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        CommandSpec {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: cwd.into(),
            log_path: None,
        }
    }

    /// Append an argument.
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

    /// Set the full environment.
    pub fn envs(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }

    /// Direct combined output to a log file.
    pub fn log_to(mut self, path: impl Into<PathBuf>) -> Self {
        self.log_path = Some(path.into());
        self
    }
}

/// The result of running a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// The process exit code, or a synthetic non-zero code if it was killed.
    pub status: i32,
    /// Captured stdout (and stderr when no log file was given). Empty when a log
    /// file captured the output.
    pub stdout: Vec<u8>,
}

impl CommandOutput {
    /// Whether the command exited successfully.
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

/// Errors a runner can surface when it cannot launch a process at all.
#[derive(Debug, thiserror::Error)]
#[error("could not run `{program}`: {reason}")]
pub struct RunError {
    /// The program that could not be launched.
    pub program: String,
    /// Why launching failed.
    pub reason: String,
}

/// The injectable boundary for launching external processes.
pub trait CommandRunner: Send + Sync {
    /// Run a command to completion, returning its exit status and any captured
    /// output. Returning `Err` means the process could not be launched; a
    /// non-zero exit is reported through [`CommandOutput::status`], not `Err`.
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, RunError>;
}

/// The production runner: launches processes with [`std::process::Command`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl SystemRunner {
    /// Construct a system runner.
    pub fn new() -> Self {
        SystemRunner
    }
}

impl CommandRunner for SystemRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, RunError> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args);
        cmd.current_dir(&spec.cwd);
        cmd.env_clear();
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        match &spec.log_path {
            Some(path) => run_logged(cmd, &spec.program, path),
            None => run_captured(cmd, &spec.program),
        }
    }
}

fn run_logged(mut cmd: Command, program: &str, log_path: &Path) -> Result<CommandOutput, RunError> {
    use std::process::Stdio;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| RunError {
            program: program.to_string(),
            reason: format!("cannot open log {}: {e}", log_path.display()),
        })?;
    let err_clone = file.try_clone().map_err(|e| RunError {
        program: program.to_string(),
        reason: format!("cannot clone log handle: {e}"),
    })?;
    cmd.stdout(Stdio::from(file));
    cmd.stderr(Stdio::from(err_clone));
    let status = cmd.status().map_err(|e| RunError {
        program: program.to_string(),
        reason: e.to_string(),
    })?;
    Ok(CommandOutput {
        status: status.code().unwrap_or(-1),
        stdout: Vec::new(),
    })
}

fn run_captured(mut cmd: Command, program: &str) -> Result<CommandOutput, RunError> {
    let output = cmd.output().map_err(|e| RunError {
        program: program.to_string(),
        reason: e.to_string(),
    })?;
    let mut stdout = output.stdout;
    stdout.extend_from_slice(&output.stderr);
    Ok(CommandOutput {
        status: output.status.code().unwrap_or(-1),
        stdout,
    })
}

/// Test helpers: a recording fake [`CommandRunner`].
///
/// This module is public so integration tests (which compile against the crate
/// as an external dependency) can drive [`crate::build_package`] without running
/// real processes. It is not part of the production surface.
pub mod testing {
    use super::*;
    use std::sync::Mutex;

    /// A recording fake runner for tests. It returns a programmable response per
    /// invocation and records every [`CommandSpec`] it was given.
    #[derive(Debug, Default)]
    pub struct FakeRunner {
        calls: Mutex<Vec<CommandSpec>>,
        responses: Mutex<Vec<Response>>,
    }

    /// How the fake runner should respond to one invocation.
    #[derive(Debug, Clone)]
    pub enum Response {
        /// Exit with this status and write `stdout` (or these bytes to the log).
        Output {
            /// The exit status to report.
            status: i32,
            /// The bytes to return as stdout or write to the log.
            bytes: Vec<u8>,
        },
        /// Exit with this status and, as a side effect, write `contents` to
        /// `path`. Emulates a fetch command that downloads a file.
        WriteFile {
            /// The exit status to report.
            status: i32,
            /// The file to create.
            path: PathBuf,
            /// The file contents to write.
            contents: Vec<u8>,
        },
        /// Fail to launch.
        Fail(String),
    }

    impl FakeRunner {
        /// A fake that returns success with empty output for every call.
        pub fn always_ok() -> Self {
            FakeRunner::default()
        }

        /// Queue a response for the next invocation. Responses are consumed in
        /// order; once exhausted, success with empty output is returned.
        pub fn push(&self, response: Response) {
            self.responses.lock().unwrap().push(response);
        }

        /// The recorded invocations, in order.
        pub fn calls(&self) -> Vec<CommandSpec> {
            self.calls.lock().unwrap().clone()
        }

        /// The number of invocations recorded.
        pub fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, RunError> {
            self.calls.lock().unwrap().push(spec.clone());
            let mut responses = self.responses.lock().unwrap();
            let response = if responses.is_empty() {
                Response::Output {
                    status: 0,
                    bytes: Vec::new(),
                }
            } else {
                responses.remove(0)
            };
            match response {
                Response::Output { status, bytes } => {
                    if let Some(log) = &spec.log_path {
                        use std::io::Write as _;
                        if let Some(parent) = log.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let mut f = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(log)
                            .map_err(|e| RunError {
                                program: spec.program.clone(),
                                reason: e.to_string(),
                            })?;
                        let _ = f.write_all(&bytes);
                        Ok(CommandOutput {
                            status,
                            stdout: Vec::new(),
                        })
                    } else {
                        Ok(CommandOutput {
                            status,
                            stdout: bytes,
                        })
                    }
                }
                Response::WriteFile {
                    status,
                    path,
                    contents,
                } => {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    std::fs::write(&path, &contents).map_err(|e| RunError {
                        program: spec.program.clone(),
                        reason: e.to_string(),
                    })?;
                    Ok(CommandOutput {
                        status,
                        stdout: Vec::new(),
                    })
                }
                Response::Fail(reason) => Err(RunError {
                    program: spec.program.clone(),
                    reason,
                }),
            }
        }
    }
}
