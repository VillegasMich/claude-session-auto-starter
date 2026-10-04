//! Thin wrapper around [`std::process::Command`] used to invoke `claude`.
//!
//! Every command runs non-interactively, with stdin closed, an environment built from scratch
//! (nothing is inherited) and a hard timeout. Secrets are only ever passed through the
//! environment and are redacted when the command is displayed.

use std::ffi::OsString;
use std::fmt;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::config::Secret;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// After a shutdown request, a running child gets this long to finish before it is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("could not start `{program}`")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("i/o error while running `{program}`")]
    Io {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`{program}` timed out after {}s and was killed", timeout.as_secs())]
    Timeout { program: String, timeout: Duration },
    #[error("`{program}` was stopped by shutdown")]
    Cancelled { program: String },
}

/// Result of a command that ran to completion, successfully or not.
#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone)]
enum EnvValue {
    Plain(OsString),
    Secret(Secret),
}

/// A command to run. Arguments must never contain secrets: they are visible in `ps` and logs.
#[derive(Clone)]
pub struct Cmd {
    program: PathBuf,
    args: Vec<String>,
    env: Vec<(String, EnvValue)>,
    current_dir: Option<PathBuf>,
    timeout: Duration,
    cancel: Option<Arc<AtomicBool>>,
}

impl Cmd {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            current_dir: None,
            timeout: DEFAULT_TIMEOUT,
            cancel: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: &str, value: impl Into<OsString>) -> Self {
        self.env
            .push((key.to_owned(), EnvValue::Plain(value.into())));
        self
    }

    /// An environment variable whose value is never displayed or logged.
    pub fn secret_env(mut self, key: &str, value: Secret) -> Self {
        self.env.push((key.to_owned(), EnvValue::Secret(value)));
        self
    }

    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// When `flag` becomes true the child gets a short grace period, then it is killed.
    pub fn cancel_on(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }

    fn program_name(&self) -> String {
        self.program.file_name().map_or_else(
            || self.program.display().to_string(),
            |n| n.to_string_lossy().into(),
        )
    }

    /// Multi-line description for `--dry-run`: working directory, environment (secrets
    /// redacted) and the command line.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        if let Some(dir) = &self.current_dir {
            out += &format!("cwd: {}\n", dir.display());
        }
        out += "env (everything else cleared):\n";
        for (key, value) in &self.env {
            let value = match value {
                EnvValue::Plain(v) => v.to_string_lossy().into_owned(),
                EnvValue::Secret(s) => s.to_string(),
            };
            out += &format!("  {key}={value}\n");
        }
        out += &format!("command:\n  {self}");
        out
    }

    /// Runs the command to completion. A non-zero exit is not an error here: the caller
    /// inspects [`Output::status`] (`claude` reports failures as JSON on stdout).
    pub fn run(&self) -> Result<Output, ExecError> {
        let program = self.program_name();
        debug!(command = %self, "exec");

        let mut process = Command::new(&self.program);
        process
            .args(&self.args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &self.env {
            match value {
                EnvValue::Plain(v) => process.env(key, v),
                EnvValue::Secret(s) => process.env(key, s.expose()),
            };
        }
        if let Some(dir) = &self.current_dir {
            process.current_dir(dir);
        }

        let mut child = process.spawn().map_err(|source| ExecError::Spawn {
            program: program.clone(),
            source,
        })?;
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());

        let status = match self.wait(&mut child) {
            Ok(Wait::Exited(status)) => status,
            Ok(Wait::TimedOut) => {
                kill(&mut child);
                return Err(ExecError::Timeout {
                    program,
                    timeout: self.timeout,
                });
            }
            Ok(Wait::Cancelled) => {
                kill(&mut child);
                return Err(ExecError::Cancelled { program });
            }
            Err(source) => {
                kill(&mut child);
                return Err(ExecError::Io { program, source });
            }
        };

        Ok(Output {
            status,
            stdout: stdout.join().unwrap_or_default(),
            stderr: stderr.join().unwrap_or_default(),
        })
    }

    fn wait(&self, child: &mut Child) -> std::io::Result<Wait> {
        let deadline = Instant::now() + self.timeout;
        let mut cancel_deadline: Option<Instant> = None;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(Wait::Exited(status));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(Wait::TimedOut);
            }
            let cancelled = self
                .cancel
                .as_ref()
                .is_some_and(|f| f.load(Ordering::Relaxed));
            if cancelled {
                match cancel_deadline {
                    None => {
                        warn!(
                            grace_secs = SHUTDOWN_GRACE.as_secs(),
                            "shutdown requested; waiting for `{}` to finish",
                            self.program_name()
                        );
                        cancel_deadline = Some(now + SHUTDOWN_GRACE);
                    }
                    Some(limit) if now >= limit => return Ok(Wait::Cancelled),
                    Some(_) => {}
                }
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

enum Wait {
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
    // Reader threads are detached: grandchildren may still hold the pipes open.
}

/// Shows program and arguments only. The environment is left out on purpose.
impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.program.display())?;
        for arg in &self.args {
            if arg.is_empty() || arg.contains(|c: char| c.is_whitespace() || c == '"') {
                write!(f, " {arg:?}")?;
            } else {
                write!(f, " {arg}")?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Cmd({self})")
    }
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Cmd {
        Cmd::new("/bin/sh").args(["-c", script])
    }

    #[test]
    fn captures_output_and_status() {
        let out = sh("echo out; echo err >&2; exit 3").run().unwrap();
        assert_eq!(out.stdout, "out\n");
        assert_eq!(out.stderr, "err\n");
        assert_eq!(out.status.code(), Some(3));
    }

    #[test]
    fn environment_is_built_from_scratch() {
        let out = sh("echo \"${HOME:-unset}:${CSS_TEST:-unset}\"")
            .env("CSS_TEST", "yes")
            .run()
            .unwrap();
        assert_eq!(out.stdout, "unset:yes\n");
    }

    #[test]
    fn secrets_reach_the_child_but_are_never_displayed() {
        let cmd = sh("printf %s \"$TOKEN\"").secret_env("TOKEN", Secret::new("s3cr3t"));
        assert_eq!(cmd.run().unwrap().stdout, "s3cr3t");
        assert!(!cmd.describe().contains("s3cr3t"));
        assert!(cmd.describe().contains("TOKEN=[redacted]"));
        assert!(!format!("{cmd} {cmd:?}").contains("s3cr3t"));
    }

    #[test]
    fn runs_in_current_dir() {
        let dir = tempfile::tempdir().unwrap();
        let out = Cmd::new("/bin/pwd").current_dir(dir.path()).run().unwrap();
        assert_eq!(
            std::fs::canonicalize(out.stdout.trim()).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn missing_program_is_spawn_error() {
        let err = Cmd::new("/definitely/not/a/program").run().unwrap_err();
        assert!(matches!(err, ExecError::Spawn { .. }));
    }

    #[test]
    fn times_out() {
        let err = Cmd::new("/bin/sleep")
            .arg("5")
            .timeout(Duration::from_millis(100))
            .run()
            .unwrap_err();
        assert!(matches!(err, ExecError::Timeout { .. }));
    }

    #[test]
    fn display_quotes_empty_and_spaced_args() {
        let cmd = Cmd::new("claude").args(["-p", "hi there", "--tools", ""]);
        assert_eq!(cmd.to_string(), r#"claude -p "hi there" --tools """#);
    }
}
