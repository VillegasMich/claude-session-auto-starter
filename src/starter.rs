//! Sends the starter message with `claude -p`. See `docs/architecture.md#starting-a-window`.
//!
//! The command must stay minimal: every flag here exists to *remove* context. Never add any.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use tracing::warn;

use crate::claude::ClaudeCli;
use crate::exec::{Cmd, Output};

pub const START_TIMEOUT: Duration = Duration::from_secs(2 * 60);

/// Replaces Claude Code's large default system prompt, which is most of the cost.
pub const SYSTEM_PROMPT: &str = "Reply with one word.";

/// Error summaries are cut to this many characters.
const MAX_ERROR_CHARS: usize = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// The message went through: a window is now active.
    Started(StartReport),
    /// `claude` reported a usage limit, so a window is already active (and full).
    LimitReached {
        message: String,
        /// When the 5-hour window resets, if `claude` reported it.
        resets_at: Option<DateTime<Utc>>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartReport {
    /// Reset time of the 5-hour window, from the `rate_limit_event` `claude` emits. Exact
    /// (Anthropic rounds it to the hour), and it reveals a window that was already running.
    pub resets_at: Option<DateTime<Utc>>,
    pub duration: Duration,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

pub trait Starter {
    fn start(&self) -> Result<StartOutcome>;
}

pub struct ClaudeCliStarter {
    cmd: Cmd,
}

impl ClaudeCliStarter {
    /// `supported` is the set of flags from `claude --help`; flags missing from it are dropped
    /// with a warning. `None` keeps every flag (used when help couldn't be read).
    pub fn new(
        cli: &ClaudeCli,
        model: &str,
        prompt: &str,
        supported: Option<&BTreeSet<String>>,
    ) -> Self {
        let (args, dropped) = starter_args(model, prompt, supported);
        if !dropped.is_empty() {
            warn!(
                flags = dropped.join(" "),
                "installed `claude` doesn't support these starter flags; dropping them \
                 (the starter message may cost more tokens)"
            );
        }
        Self {
            cmd: cli
                .command()
                .args(args)
                // No extended thinking: a one-word reply needs none, and it is billed output.
                .env("MAX_THINKING_TOKENS", "0")
                .timeout(START_TIMEOUT),
        }
    }

    pub fn command(&self) -> &Cmd {
        &self.cmd
    }
}

impl Starter for ClaudeCliStarter {
    fn start(&self) -> Result<StartOutcome> {
        let started = Instant::now();
        let output = self.cmd.run()?;
        let mut outcome = interpret(&output)?;
        if let StartOutcome::Started(report) = &mut outcome {
            report.duration = started.elapsed();
        }
        Ok(outcome)
    }
}

/// Arguments after `claude`, and the optional flags dropped because `supported` lacks them.
pub fn starter_args(
    model: &str,
    prompt: &str,
    supported: Option<&BTreeSet<String>>,
) -> (Vec<String>, Vec<&'static str>) {
    // Without these the starter can't work at all; never dropped. `stream-json` (which needs
    // `--verbose` with `-p`) only changes the output format: it adds the `rate_limit_event`
    // carrying the window's reset time. It sends nothing extra to the model.
    let mut args: Vec<String> = [
        "-p",
        prompt,
        "--model",
        model,
        "--output-format",
        "stream-json",
        "--verbose",
    ]
    .map(String::from)
    .into();

    // Each removes context or side effects. Flag and its value, if any.
    let optional: [(&'static str, Option<&str>); 6] = [
        ("--system-prompt", Some(SYSTEM_PROMPT)),
        ("--tools", Some("")),
        ("--strict-mcp-config", None),
        ("--setting-sources", Some("")),
        ("--disable-slash-commands", None),
        ("--no-session-persistence", None),
    ];
    let mut dropped = Vec::new();
    for (flag, value) in optional {
        if supported.is_some_and(|s| !s.contains(flag)) {
            dropped.push(flag);
            continue;
        }
        args.push(flag.to_owned());
        args.extend(value.map(String::from));
    }
    (args, dropped)
}

/// The final `{"type":"result",...}` message. Only the fields used here.
#[derive(Debug, Deserialize)]
struct CliResult {
    #[serde(default)]
    is_error: bool,
    subtype: Option<String>,
    result: Option<String>,
    #[serde(default)]
    usage: TokenUsage,
}

#[derive(Debug, Default, Deserialize)]
struct TokenUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

/// What matters in the `stream-json` output: the result and the 5-hour reset time.
#[derive(Debug, Default)]
struct Stream {
    result: Option<CliResult>,
    resets_at: Option<DateTime<Utc>>,
}

/// Parses `stream-json` output (one JSON object per line) defensively: unknown lines and
/// fields are ignored. Also accepts plain `json` output (a single result object).
fn parse_stream(stdout: &str) -> Stream {
    let mut stream = Stream::default();
    for value in stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
    {
        match value.get("type").and_then(Value::as_str) {
            Some("result") => stream.result = serde_json::from_value(value).ok(),
            Some("rate_limit_event") => {
                if let Some(t) = five_hour_reset(&value) {
                    stream.resets_at = Some(t);
                }
            }
            _ => {}
        }
    }
    // Plain `json` output, possibly pretty-printed over several lines.
    if stream.result.is_none() {
        stream.result = serde_json::from_str(stdout.trim()).ok();
    }
    stream
}

/// `rate_limit_info.unifiedWindows.five_hour.resetsAt`, or `rate_limit_info.resetsAt` when
/// `rateLimitType` is `five_hour`. Unix epoch seconds.
fn five_hour_reset(event: &Value) -> Option<DateTime<Utc>> {
    let info = event.get("rate_limit_info")?;
    let secs = info
        .pointer("/unifiedWindows/five_hour/resetsAt")
        .and_then(Value::as_f64)
        .or_else(|| {
            (info.get("rateLimitType")?.as_str()? == "five_hour")
                .then(|| info.get("resetsAt")?.as_f64())
                .flatten()
        })?;
    DateTime::from_timestamp(secs as i64, 0)
}

/// Classifies a finished `claude -p` run. Success needs exit 0 and `is_error: false`.
pub fn interpret(output: &Output) -> Result<StartOutcome> {
    let Stream { result, resets_at } = parse_stream(&output.stdout);
    let message = result
        .as_ref()
        .and_then(|r| r.result.clone())
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| output.stderr.trim().to_owned());

    match result {
        Some(r) if output.status.success() && !r.is_error => {
            Ok(StartOutcome::Started(StartReport {
                resets_at,
                duration: Duration::ZERO,
                input_tokens: r.usage.input_tokens,
                output_tokens: r.usage.output_tokens,
                cache_read_tokens: r.usage.cache_read_input_tokens,
                cache_creation_tokens: r.usage.cache_creation_input_tokens,
            }))
        }
        _ if is_limit_message(&message) || is_limit_message(&output.stderr) => {
            Ok(StartOutcome::LimitReached {
                message: truncate(&message),
                resets_at,
            })
        }
        result => {
            // `subtype` is "success" even for API errors; only other values add information.
            let detail = result
                .and_then(|r| r.subtype)
                .filter(|s| s != "success")
                .map(|s| format!(", {s}"))
                .unwrap_or_default();
            let message = if message.is_empty() {
                "no output".into()
            } else {
                truncate(&message)
            };
            Err(anyhow!(
                "`claude` failed ({}{detail}): {message}",
                output.status
            ))
        }
    }
}

fn is_limit_message(text: &str) -> bool {
    let text = text.to_lowercase();
    [
        "usage limit",
        "hit your limit",
        "limit reached",
        "limit will reset",
        "rate_limit",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn truncate(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(MAX_ERROR_CHARS) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
pub mod testing {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    use super::*;

    /// Returns queued results in order; `Err` entries become errors.
    pub struct FakeStarter {
        results: RefCell<VecDeque<Result<StartOutcome, String>>>,
        pub calls: Cell<usize>,
    }

    impl FakeStarter {
        pub fn new(results: impl IntoIterator<Item = Result<StartOutcome, String>>) -> Self {
            Self {
                results: RefCell::new(results.into_iter().collect()),
                calls: Cell::new(0),
            }
        }

        pub fn ok() -> Result<StartOutcome, String> {
            Ok(StartOutcome::Started(StartReport::default()))
        }
    }

    impl Starter for FakeStarter {
        fn start(&self) -> Result<StartOutcome> {
            self.calls.set(self.calls.get() + 1);
            self.results
                .borrow_mut()
                .pop_front()
                .expect("unexpected start")
                .map_err(|e| anyhow!(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    use super::*;

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn full_command_line() {
        let (args, dropped) = starter_args("haiku", "hi", None);
        assert_eq!(
            args,
            [
                "-p",
                "hi",
                "--model",
                "haiku",
                "--output-format",
                "stream-json",
                "--verbose",
                "--system-prompt",
                SYSTEM_PROMPT,
                "--tools",
                "",
                "--strict-mcp-config",
                "--setting-sources",
                "",
                "--disable-slash-commands",
                "--no-session-persistence",
            ]
        );
        assert!(dropped.is_empty());
    }

    #[test]
    fn unsupported_optional_flags_are_dropped() {
        let supported: BTreeSet<String> = ["--system-prompt", "--tools", "--strict-mcp-config"]
            .map(String::from)
            .into();
        let (args, dropped) = starter_args("haiku", "hi", Some(&supported));
        assert_eq!(
            dropped,
            [
                "--setting-sources",
                "--disable-slash-commands",
                "--no-session-persistence"
            ]
        );
        assert!(args.starts_with(&["-p".to_owned(), "hi".to_owned()]));
        assert!(args.contains(&"--tools".to_owned()));
        assert!(!args.contains(&"--setting-sources".to_owned()));
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    #[test]
    fn stream_reports_reset_time_and_tokens() {
        // 1791140400 = 2026-10-04T19:00:00Z
        let stdout = [
            r#"{"type":"system","subtype":"init","session_id":"x","tools":[]}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1791140400,"rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"utilization":0.04,"resetsAt":1791140400}}},"session_id":"x"}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Hello","usage":{"input_tokens":12,"output_tokens":3}}"#,
            "",
        ]
        .join("\n");
        assert_eq!(
            interpret(&output(0, &stdout, "")).unwrap(),
            StartOutcome::Started(StartReport {
                resets_at: Some(utc("2026-10-04T19:00:00Z")),
                input_tokens: 12,
                output_tokens: 3,
                ..StartReport::default()
            })
        );
    }

    #[test]
    fn reset_time_from_rate_limit_type_only() {
        let event: Value = serde_json::from_str(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1791140400,"rateLimitType":"five_hour"}}"#,
        )
        .unwrap();
        assert_eq!(five_hour_reset(&event), Some(utc("2026-10-04T19:00:00Z")));

        let weekly: Value = serde_json::from_str(
            r#"{"type":"rate_limit_event","rate_limit_info":{"resetsAt":1791140400,"rateLimitType":"seven_day"}}"#,
        )
        .unwrap();
        assert_eq!(five_hour_reset(&weekly), None);
    }

    #[test]
    fn success_reports_tokens() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":false,"result":"Hello",
            "duration_ms":1200,"total_cost_usd":0.0002,
            "usage":{"input_tokens":12,"output_tokens":3,"cache_read_input_tokens":0,
                     "cache_creation_input_tokens":0}}"#;
        assert_eq!(
            interpret(&output(0, stdout, "")).unwrap(),
            StartOutcome::Started(StartReport {
                input_tokens: 12,
                output_tokens: 3,
                ..StartReport::default()
            })
        );
    }

    #[test]
    fn usage_limit_is_not_a_failure() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":true,
            "result":"You've hit your limit · resets 7pm (UTC)"}"#;
        assert!(matches!(
            interpret(&output(1, stdout, "")).unwrap(),
            StartOutcome::LimitReached { message, .. } if message.contains("resets 7pm")
        ));
        assert!(matches!(
            interpret(&output(1, "", "Claude AI usage limit reached|1791140400")).unwrap(),
            StartOutcome::LimitReached { .. }
        ));
    }

    #[test]
    fn errors_carry_the_message() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":true,
            "result":"Invalid API key · Please run /login"}"#;
        let err = interpret(&output(1, stdout, "")).unwrap_err().to_string();
        assert!(err.contains("Invalid API key"), "{err}");

        let err = interpret(&output(1, "", "boom")).unwrap_err().to_string();
        assert!(err.contains("boom"), "{err}");

        // Exit 0 but not JSON: never assume success.
        assert!(interpret(&output(0, "Hello", "")).is_err());
    }

    #[test]
    fn long_messages_are_truncated() {
        let long = "x".repeat(1000);
        let err = interpret(&output(1, "", &long)).unwrap_err().to_string();
        assert!(err.len() < 400, "{}", err.len());
    }
}
