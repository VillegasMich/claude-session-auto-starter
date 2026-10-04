//! Configuration loaded from environment variables. See `docs/configuration.md`.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use chrono_tz::Tz;

use crate::hours::ActiveHours;

pub const DEFAULT_CHECK_INTERVAL_MINUTES: u64 = 5;
pub const DEFAULT_STARTER_MODEL: &str = "haiku";
pub const DEFAULT_STARTER_PROMPT: &str = "hi";
pub const DEFAULT_DATA_DIR: &str = "/data";

/// Prefix of claude.ai subscription OAuth tokens (`claude setup-token`).
const TOKEN_PREFIX: &str = "sk-ant-oat";

/// Long prompts defeat the purpose of a minimal starter message.
const MAX_PROMPT_CHARS: usize = 200;

/// Validated service configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub token: Secret,
    pub check_interval: Duration,
    pub active_hours: Option<ActiveHours>,
    pub timezone: Tz,
    pub detection: Detection,
    pub starter_model: String,
    pub starter_prompt: String,
    pub data_dir: PathBuf,
}

/// A value that must never be logged: `Debug` and `Display` print a placeholder instead.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// How the service finds out whether a window is active (`DETECTION`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detection {
    /// Usage endpoint, falling back to the local state file.
    Auto,
    /// Usage endpoint only.
    OAuthUsage,
    /// Local state file only.
    Local,
}

impl Detection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::OAuthUsage => "oauth-usage",
            Self::Local => "local",
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error(
        "CLAUDE_CODE_OAUTH_TOKEN is not set; create one with `claude setup-token` \
         (see docs/configuration.md)"
    )]
    MissingToken,
    // The value is deliberately not included: it may be a credential.
    #[error(
        "CLAUDE_CODE_OAUTH_TOKEN looks like an Anthropic API key; only claude.ai subscription \
         tokens from `claude setup-token` are supported"
    )]
    ApiKeyAsToken,
    #[error(
        "CLAUDE_CODE_OAUTH_TOKEN looks like the authorization code shown in the browser (`code#state`); \
         paste it into the `claude setup-token` prompt and use the `sk-ant-oat01-...` token it prints"
    )]
    AuthorizationCodeAsToken,
    #[error(
        "CLAUDE_CODE_OAUTH_TOKEN is not a subscription token: expected `sk-ant-oat01-...` \
         as printed by `claude setup-token`"
    )]
    MalformedToken,
    #[error("{var}={value:?} is invalid: {reason}")]
    Invalid {
        var: &'static str,
        value: String,
        reason: String,
    },
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Builds the config from an arbitrary key lookup, so validation is testable without
    /// touching the process environment.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |key: &str| {
            lookup(key)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };

        let token = get("CLAUDE_CODE_OAUTH_TOKEN").ok_or(ConfigError::MissingToken)?;
        if token.starts_with("sk-ant-api") {
            return Err(ConfigError::ApiKeyAsToken);
        }
        if !token.starts_with(TOKEN_PREFIX) {
            return Err(if token.contains('#') {
                ConfigError::AuthorizationCodeAsToken
            } else {
                ConfigError::MalformedToken
            });
        }

        let check_interval = match get("CHECK_INTERVAL_MINUTES") {
            None => DEFAULT_CHECK_INTERVAL_MINUTES,
            Some(v) => match v.parse::<u64>() {
                Ok(n @ 1..=60) => n,
                _ => return Err(invalid("CHECK_INTERVAL_MINUTES", &v, "expected 1-60")),
            },
        };

        let active_hours = get("ACTIVE_HOURS")
            .map(|v| v.parse().map_err(|e| invalid("ACTIVE_HOURS", &v, e)))
            .transpose()?;

        let timezone = match get("TIMEZONE") {
            None => Tz::UTC,
            Some(v) => v
                .parse()
                .map_err(|_| invalid("TIMEZONE", &v, "expected an IANA name like Europe/Madrid"))?,
        };

        let detection = match get("DETECTION").as_deref() {
            None | Some("auto") => Detection::Auto,
            Some("oauth-usage") => Detection::OAuthUsage,
            Some("local") => Detection::Local,
            Some(v) => {
                return Err(invalid(
                    "DETECTION",
                    v,
                    "expected auto, oauth-usage or local",
                ));
            }
        };

        let starter_model = get("STARTER_MODEL").unwrap_or_else(|| DEFAULT_STARTER_MODEL.into());
        if starter_model.starts_with('-') || starter_model.contains(char::is_whitespace) {
            return Err(invalid(
                "STARTER_MODEL",
                &starter_model,
                "expected a model alias or name",
            ));
        }

        let starter_prompt = get("STARTER_PROMPT").unwrap_or_else(|| DEFAULT_STARTER_PROMPT.into());
        if starter_prompt.starts_with('-') {
            return Err(invalid(
                "STARTER_PROMPT",
                &starter_prompt,
                "must not start with '-'",
            ));
        }
        if starter_prompt.chars().count() > MAX_PROMPT_CHARS {
            return Err(invalid(
                "STARTER_PROMPT",
                &starter_prompt,
                format!("keep it under {MAX_PROMPT_CHARS} characters"),
            ));
        }

        let data_dir = PathBuf::from(get("DATA_DIR").unwrap_or_else(|| DEFAULT_DATA_DIR.into()));
        if !data_dir.is_absolute() {
            return Err(invalid(
                "DATA_DIR",
                &data_dir.display().to_string(),
                "must be an absolute path",
            ));
        }

        Ok(Self {
            token: Secret::new(token),
            check_interval: Duration::from_secs(check_interval * 60),
            active_hours,
            timezone,
            detection,
            starter_model,
            starter_prompt,
            data_dir,
        })
    }

    /// `HOME` of the `claude` child process.
    pub fn home_dir(&self) -> PathBuf {
        self.data_dir.join("home")
    }

    /// `CLAUDE_CONFIG_DIR` of the `claude` child process.
    pub fn claude_config_dir(&self) -> PathBuf {
        self.home_dir().join(".claude")
    }

    /// Always-empty working directory of the starter message.
    pub fn work_dir(&self) -> PathBuf {
        self.data_dir.join("work")
    }

    pub fn state_file(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }
}

fn invalid(var: &'static str, value: &str, reason: impl fmt::Display) -> ConfigError {
    ConfigError::Invalid {
        var,
        value: value.to_owned(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "sk-ant-oat01-test-token";

    fn load(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let vars: Vec<(String, String)> = [("CLAUDE_CODE_OAUTH_TOKEN", TOKEN)]
            .iter()
            .chain(vars)
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        // Later entries win, so tests can override the token.
        Config::from_lookup(|key| {
            vars.iter()
                .rev()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        })
    }

    #[test]
    fn defaults() {
        let c = load(&[]).unwrap();
        assert_eq!(c.token.expose(), TOKEN);
        assert_eq!(c.check_interval, Duration::from_secs(300));
        assert_eq!(c.active_hours, None);
        assert_eq!(c.timezone, Tz::UTC);
        assert_eq!(c.detection, Detection::Auto);
        assert_eq!(c.starter_model, "haiku");
        assert_eq!(c.starter_prompt, "hi");
        assert_eq!(c.data_dir, PathBuf::from("/data"));
        assert_eq!(c.work_dir(), PathBuf::from("/data/work"));
        assert_eq!(c.claude_config_dir(), PathBuf::from("/data/home/.claude"));
        assert_eq!(c.state_file(), PathBuf::from("/data/state.json"));
    }

    #[test]
    fn overrides() {
        let c = load(&[
            ("CHECK_INTERVAL_MINUTES", "15"),
            ("ACTIVE_HOURS", "07:00-23:00"),
            ("TIMEZONE", "America/Bogota"),
            ("DETECTION", "local"),
            ("STARTER_MODEL", "sonnet"),
            ("STARTER_PROMPT", "ok"),
            ("DATA_DIR", "/var/lib/css"),
        ])
        .unwrap();
        assert_eq!(c.check_interval, Duration::from_secs(900));
        assert_eq!(c.active_hours, Some("07:00-23:00".parse().unwrap()));
        assert_eq!(c.timezone, chrono_tz::America::Bogota);
        assert_eq!(c.detection, Detection::Local);
        assert_eq!(c.starter_model, "sonnet");
        assert_eq!(c.starter_prompt, "ok");
        assert_eq!(c.data_dir, PathBuf::from("/var/lib/css"));
    }

    #[test]
    fn token_is_required() {
        assert_eq!(
            load(&[("CLAUDE_CODE_OAUTH_TOKEN", " ")]),
            Err(ConfigError::MissingToken)
        );
    }

    #[test]
    fn api_keys_are_rejected_without_echoing_them() {
        let err = load(&[("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-api03-secret")]).unwrap_err();
        assert_eq!(err, ConfigError::ApiKeyAsToken);
        assert!(!err.to_string().contains("secret"));
    }

    #[test]
    fn non_subscription_tokens_are_rejected_without_echoing_them() {
        let err = load(&[("CLAUDE_CODE_OAUTH_TOKEN", "abcSECRET#stateSECRET")]).unwrap_err();
        assert_eq!(err, ConfigError::AuthorizationCodeAsToken);
        assert!(!err.to_string().contains("SECRET"));

        let err = load(&[("CLAUDE_CODE_OAUTH_TOKEN", "randomSECRET")]).unwrap_err();
        assert_eq!(err, ConfigError::MalformedToken);
        assert!(!err.to_string().contains("SECRET"));
    }

    #[test]
    fn token_never_appears_in_debug_output() {
        let c = load(&[]).unwrap();
        assert!(!format!("{c:?}").contains(TOKEN));
        assert_eq!(c.token.to_string(), "[redacted]");
    }

    #[test]
    fn rejects_bad_values() {
        for (var, value) in [
            ("CHECK_INTERVAL_MINUTES", "0"),
            ("CHECK_INTERVAL_MINUTES", "61"),
            ("CHECK_INTERVAL_MINUTES", "five"),
            ("ACTIVE_HOURS", "7-23"),
            ("TIMEZONE", "Mars/Olympus"),
            ("DETECTION", "magic"),
            ("STARTER_MODEL", "--bare"),
            ("STARTER_MODEL", "two words"),
            ("STARTER_PROMPT", "--help"),
            ("DATA_DIR", "relative/dir"),
        ] {
            let err = load(&[(var, value)]).unwrap_err();
            assert!(
                matches!(err, ConfigError::Invalid { var: v, .. } if v == var),
                "{var}={value}: {err}"
            );
        }
        let long = "x".repeat(MAX_PROMPT_CHARS + 1);
        assert!(load(&[("STARTER_PROMPT", &long)]).is_err());
    }
}
