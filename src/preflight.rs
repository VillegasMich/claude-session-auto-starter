//! Startup checks: subscription auth only, `claude` usable, `DATA_DIR` layout safe.
//!
//! Every failure here is a configuration error and ends the process with a non-zero code.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::claude::{ClaudeCli, Plan};
use crate::config::Config;

/// Variables that make Claude Code use an API key or a third-party provider instead of the
/// subscription. They are never passed to the child either; refusing them makes the
/// misconfiguration visible instead of silently ignored.
pub const FORBIDDEN_VARS: [&str; 6] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// Files that would make Claude Code load extra context if found in the starter's working
/// directory or any directory above it.
const CONTEXT_FILES: [&str; 4] = [".git", "CLAUDE.md", "CLAUDE.local.md", ".claude/CLAUDE.md"];

#[derive(Debug, Clone)]
pub struct Report {
    pub version: String,
    pub plan: Plan,
    /// `None` if `claude --help` couldn't be read; then every starter flag is kept.
    pub flags: Option<BTreeSet<String>>,
}

/// Names of forbidden variables that are set (to anything non-empty).
pub fn forbidden_vars_set(lookup: impl Fn(&str) -> Option<String>) -> Vec<&'static str> {
    FORBIDDEN_VARS
        .into_iter()
        .filter(|var| lookup(var).is_some_and(|v| !v.is_empty()))
        .collect()
}

pub fn check_environment() -> Result<()> {
    let set = forbidden_vars_set(|k| std::env::var(k).ok());
    if !set.is_empty() {
        bail!(
            "refusing to start: {} set; this service only uses a claude.ai subscription \
             (CLAUDE_CODE_OAUTH_TOKEN), never the Anthropic API or third-party providers. \
             Unset {}.",
            set.join(", "),
            if set.len() == 1 { "it" } else { "them" }
        );
    }
    Ok(())
}

/// Creates `DATA_DIR/{home/.claude,work}` and makes sure the starter can't pick up context.
pub fn prepare_data_dir(config: &Config) -> Result<()> {
    let config_dir = config.claude_config_dir();
    let work_dir = config.work_dir();
    for dir in [&config_dir, &work_dir] {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // The isolated Claude config may hold session data; keep it private.
    fs::set_permissions(config.home_dir(), fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", config.home_dir().display()))?;
    check_work_dir(&work_dir)
}

/// `WORK_DIR` must be empty, and neither it nor any parent may hold a git repository or
/// `CLAUDE.md`: Claude Code would load them into the starter message.
pub fn check_work_dir(work_dir: &Path) -> Result<()> {
    let mut entries =
        fs::read_dir(work_dir).with_context(|| format!("reading {}", work_dir.display()))?;
    if let Some(entry) = entries.next() {
        let name = entry?.file_name();
        bail!(
            "{} must be empty but contains {:?}; it is the starter's working directory and \
             anything in it could be sent as context. Empty it.",
            work_dir.display(),
            name
        );
    }
    let work_dir = fs::canonicalize(work_dir)?;
    for dir in work_dir.ancestors() {
        for name in CONTEXT_FILES {
            let path = dir.join(name);
            if path.exists() {
                bail!(
                    "{} exists above the starter's working directory {}; Claude Code would \
                     load it as context. Choose a DATA_DIR outside any project or git \
                     repository (e.g. /var/lib/claude-session-starter).",
                    path.display(),
                    work_dir.display()
                );
            }
        }
    }
    Ok(())
}

/// Full startup check: environment, `DATA_DIR`, `claude` version, auth and flags.
pub fn run(config: &Config, cli: &ClaudeCli) -> Result<Report> {
    check_environment()?;
    prepare_data_dir(config)?;
    let version = cli
        .version()
        .with_context(|| format!("`{} --version` failed", cli.binary().display()))?;
    let auth = cli.auth_status()?;
    let plan = auth.verify()?;
    let flags = match cli.supported_flags() {
        Ok(flags) => Some(flags),
        Err(e) => {
            tracing::warn!(
                error = format!("{e:#}"),
                "cannot read `claude --help`; keeping all starter flags"
            );
            None
        }
    };
    Ok(Report {
        version,
        plan,
        flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_forbidden_variables() {
        let set = forbidden_vars_set(|k| match k {
            "ANTHROPIC_API_KEY" => Some("sk-ant-api03-x".into()),
            "CLAUDE_CODE_USE_BEDROCK" => Some("1".into()),
            "ANTHROPIC_BASE_URL" => Some(String::new()),
            _ => None,
        });
        assert_eq!(set, ["ANTHROPIC_API_KEY", "CLAUDE_CODE_USE_BEDROCK"]);
        assert!(forbidden_vars_set(|_| None).is_empty());
    }

    #[test]
    fn work_dir_must_be_empty() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        fs::create_dir(&work).unwrap();
        check_work_dir(&work).unwrap();

        fs::write(work.join("notes.txt"), "x").unwrap();
        let err = check_work_dir(&work).unwrap_err().to_string();
        assert!(err.contains("must be empty"), "{err}");
    }

    #[test]
    fn work_dir_must_not_be_inside_a_project() {
        for marker in [".git", "CLAUDE.md"] {
            let dir = tempfile::tempdir().unwrap();
            let work = dir.path().join("data/work");
            fs::create_dir_all(&work).unwrap();
            fs::create_dir_all(dir.path().join(".claude")).unwrap();
            if marker == ".git" {
                fs::create_dir(dir.path().join(marker)).unwrap();
            } else {
                fs::write(dir.path().join(marker), "x").unwrap();
            }
            let err = check_work_dir(&work).unwrap_err().to_string();
            assert!(err.contains(marker), "{err}");
        }
    }
}
