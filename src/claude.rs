//! The `claude` CLI, always run in an isolated, from-scratch environment.
//!
//! See `docs/architecture.md#starting-a-window` for why each variable is set.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::{Config, Secret};
use crate::exec::Cmd;

/// Directories always on the child's `PATH`, besides the one holding `claude`.
const BASE_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// `authMethod` values that mean a claude.ai subscription login: an interactive login
/// (`claude.ai`) or a `claude setup-token` token in `CLAUDE_CODE_OAUTH_TOKEN` (`oauth_token`).
const SUBSCRIPTION_AUTH_METHODS: [&str; 2] = ["claude.ai", "oauth_token"];

/// Plans the window behavior is known to work with.
const KNOWN_PLANS: [&str; 2] = ["pro", "max"];

#[derive(Debug, Clone)]
pub struct ClaudeCli {
    binary: PathBuf,
    home: PathBuf,
    config_dir: PathBuf,
    work_dir: PathBuf,
    token: Secret,
    cancel: Option<Arc<AtomicBool>>,
}

impl ClaudeCli {
    pub fn new(config: &Config, binary: PathBuf) -> Self {
        Self {
            binary,
            home: config.home_dir(),
            config_dir: config.claude_config_dir(),
            work_dir: config.work_dir(),
            token: config.token.clone(),
            cancel: None,
        }
    }

    /// A running `claude` is stopped (after a grace period) when `flag` is set.
    pub fn cancel_on(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// Base command: `claude` in `WORK_DIR` with the isolated environment. Nothing from the
    /// service's own environment is inherited, so an API key can never reach the child.
    pub fn command(&self) -> Cmd {
        let mut path = OsString::new();
        if let Some(dir) = self.binary.parent() {
            path.push(dir);
            path.push(":");
        }
        path.push(BASE_PATH);

        let mut cmd = Cmd::new(&self.binary)
            .current_dir(&self.work_dir)
            .env("HOME", &self.home)
            .env("CLAUDE_CONFIG_DIR", &self.config_dir)
            .secret_env("CLAUDE_CODE_OAUTH_TOKEN", self.token.clone())
            .env("DISABLE_AUTOUPDATER", "1")
            .env("PATH", path);
        if let Some(flag) = &self.cancel {
            cmd = cmd.cancel_on(Arc::clone(flag));
        }
        cmd
    }

    /// For `--version` and `--help`, which load no context: runs in `/` so they work before
    /// `WORK_DIR` exists (e.g. `start --dry-run` on a fresh `DATA_DIR`).
    fn info_command(&self) -> Cmd {
        self.command().current_dir("/")
    }

    pub fn version(&self) -> Result<String> {
        let out = self.info_command().arg("--version").run()?;
        if !out.status.success() {
            bail!(
                "`claude --version` exited with {}: {}",
                out.status,
                out.stderr.trim()
            );
        }
        Ok(out.stdout.trim().to_owned())
    }

    pub fn auth_status(&self) -> Result<AuthStatus> {
        let out = self.command().args(["auth", "status", "--json"]).run()?;
        // Not logged in exits non-zero but still prints the JSON.
        AuthStatus::parse(&out.stdout).with_context(|| {
            format!(
                "unexpected `claude auth status --json` output (exit {})",
                out.status
            )
        })
    }

    /// Long options listed by `claude --help`.
    pub fn supported_flags(&self) -> Result<BTreeSet<String>> {
        let out = self.info_command().arg("--help").run()?;
        if !out.status.success() {
            bail!("`claude --help` exited with {}", out.status);
        }
        Ok(parse_flags(&out.stdout))
    }
}

/// Finds `claude` on the service's `PATH`.
pub fn locate() -> Result<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    find_in_path(&path, "claude").context(
        "`claude` (Claude Code CLI) is required but was not found on PATH; \
         install it from https://docs.claude.com/en/docs/claude-code",
    )
}

fn find_in_path(path: &std::ffi::OsStr, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Every `--long-option` mentioned in help text.
pub fn parse_flags(help: &str) -> BTreeSet<String> {
    help.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|w| w.len() > 2 && w.starts_with("--") && !w.starts_with("---"))
        .map(str::to_owned)
        .collect()
}

/// The subset of `claude auth status --json` the service cares about. Other fields (email,
/// organization) are deliberately not deserialized so they can't end up in a log line.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    #[serde(default)]
    pub logged_in: bool,
    pub auth_method: Option<String>,
    pub api_provider: Option<String>,
    pub subscription_type: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("`claude` is not logged in; set CLAUDE_CODE_OAUTH_TOKEN (from `claude setup-token`)")]
    NotLoggedIn,
    #[error(
        "`claude` would authenticate with {0:?} instead of a claude.ai subscription; \
         only Pro/Max subscription logins are supported"
    )]
    NotSubscription(String),
    #[error("`claude` would use the {0:?} provider; only Anthropic (firstParty) is supported")]
    ThirdPartyProvider(String),
}

/// What a passing auth check knows about the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Pro or Max: verified to work.
    Supported(String),
    /// Reported, but not Pro/Max (e.g. free, team): the window behavior is unverified.
    Other(String),
    /// Not reported. Token logins (`oauth_token`) don't include it.
    Unknown,
}

impl AuthStatus {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json.trim())
    }

    /// Subscription auth only: logged in, via claude.ai OAuth, against Anthropic itself.
    pub fn verify(&self) -> Result<Plan, AuthError> {
        if !self.logged_in {
            return Err(AuthError::NotLoggedIn);
        }
        let method = self.auth_method.as_deref().unwrap_or("none");
        if !SUBSCRIPTION_AUTH_METHODS.contains(&method) {
            return Err(AuthError::NotSubscription(method.to_owned()));
        }
        let provider = self.api_provider.as_deref().unwrap_or("unknown");
        if provider != "firstParty" {
            return Err(AuthError::ThirdPartyProvider(provider.to_owned()));
        }
        Ok(match self.subscription_type.as_deref() {
            None => Plan::Unknown,
            Some(p) if KNOWN_PLANS.contains(&p) => Plan::Supported(p.to_owned()),
            Some(p) => Plan::Other(p.to_owned()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(json: &str) -> AuthStatus {
        AuthStatus::parse(json).unwrap()
    }

    #[test]
    fn interactive_pro_login_passes() {
        let s = status(
            r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty",
                "email":"someone@example.com","orgName":"x","subscriptionType":"pro"}"#,
        );
        assert_eq!(s.verify(), Ok(Plan::Supported("pro".into())));
        assert!(!format!("{s:?}").contains("example.com"));
    }

    #[test]
    fn setup_token_login_passes_with_unknown_plan() {
        // Shape reported by Claude Code 2.1 for CLAUDE_CODE_OAUTH_TOKEN.
        let s = status(
            r#"{"loggedIn":true,"authMethod":"oauth_token","apiProvider":"firstParty",
                "analyticsDisabled":false,"configDirectory":"/data/home/.claude"}"#,
        );
        assert_eq!(s.verify(), Ok(Plan::Unknown));
    }

    #[test]
    fn other_plans_pass_with_a_warning() {
        let s = status(
            r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty",
                "subscriptionType":"team"}"#,
        );
        assert_eq!(s.verify(), Ok(Plan::Other("team".into())));
    }

    #[test]
    fn api_keys_and_third_party_providers_fail() {
        let api = status(r#"{"loggedIn":true,"authMethod":"api_key","apiProvider":"firstParty"}"#);
        assert_eq!(
            api.verify(),
            Err(AuthError::NotSubscription("api_key".into()))
        );

        let bedrock =
            status(r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"bedrock"}"#);
        assert_eq!(
            bedrock.verify(),
            Err(AuthError::ThirdPartyProvider("bedrock".into()))
        );

        let none = status(r#"{"loggedIn":false}"#);
        assert_eq!(none.verify(), Err(AuthError::NotLoggedIn));
    }

    #[test]
    fn parses_long_flags_from_help() {
        let help = "Options:\n  --tools <tools...>   Use \"\" to disable\n  \
                    --setting-sources <sources>  (user, project)\n  -p, --print  Print\n  \
                    --allowedTools, --allowed-tools <tools...>\n  ----- not a flag";
        let flags = parse_flags(help);
        for f in [
            "--tools",
            "--setting-sources",
            "--print",
            "--allowedTools",
            "--allowed-tools",
        ] {
            assert!(flags.contains(f), "{f}");
        }
        assert!(!flags.contains("-p"));
        assert!(!flags.iter().any(|f| f.starts_with("---")));
    }

    #[test]
    fn finds_executables_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("claude");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        let path = std::env::join_paths(["/nonexistent".as_ref(), dir.path()]).unwrap();
        assert_eq!(find_in_path(&path, "claude"), None, "not executable yet");

        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_in_path(&path, "claude"), Some(exe));
    }

    #[test]
    fn child_environment_is_isolated() {
        let config = Config::from_lookup(|k| match k {
            "CLAUDE_CODE_OAUTH_TOKEN" => Some("sk-ant-oat01-abc".into()),
            "DATA_DIR" => Some("/srv/css".into()),
            _ => None,
        })
        .unwrap();
        let cli = ClaudeCli::new(&config, PathBuf::from("/opt/claude/bin/claude"));
        let described = cli.command().arg("--version").describe();
        assert_eq!(
            described,
            "cwd: /srv/css/work\n\
             env (everything else cleared):\n  \
             HOME=/srv/css/home\n  \
             CLAUDE_CONFIG_DIR=/srv/css/home/.claude\n  \
             CLAUDE_CODE_OAUTH_TOKEN=[redacted]\n  \
             DISABLE_AUTOUPDATER=1\n  \
             PATH=/opt/claude/bin:/usr/local/bin:/usr/bin:/bin\n\
             command:\n  /opt/claude/bin/claude --version"
        );
    }
}
