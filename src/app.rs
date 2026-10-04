//! Wires config, preflight, probes, the starter and the scheduler into the CLI commands.

use anyhow::{Result, bail};
use chrono::TimeDelta;
use tracing::{info, warn};

use crate::claude::{self, ClaudeCli, Plan};
use crate::clock::{Clock, SystemClock};
use crate::config::{Config, Detection};
use crate::preflight;
use crate::probe::{AutoProbe, LocalStateProbe, UsageApiProbe, UsageProbe, WindowStatus};
use crate::scheduler::{Cycle, GRACE, Scheduler, Settings, fmt_time};
use crate::starter::ClaudeCliStarter;
use crate::state::{FileStateStore, StateStore};

/// Run forever: check, start a window when none is active, sleep until it resets.
pub fn daemon(config: Config, clock: SystemClock) -> Result<()> {
    let (cli, report) = startup(&config, &clock)?;
    let store = FileStateStore::new(config.state_file());
    let probe = build_probe(&config, &store);
    let starter = starter(&config, &cli, &report);
    info!(
        detection = config.detection.as_str(),
        model = %config.starter_model,
        active_hours = config.active_hours.map(|h| h.to_string()).unwrap_or("all day".into()),
        timezone = %config.timezone,
        "service started"
    );
    Scheduler::new(&clock, probe.as_ref(), &starter, &store, settings(&config)).run_forever();
    Ok(())
}

/// One check; start a window if none is active (inside `ACTIVE_HOURS`), then exit.
pub fn once(config: Config, clock: SystemClock) -> Result<()> {
    let (cli, report) = startup(&config, &clock)?;
    let store = FileStateStore::new(config.state_file());
    let probe = build_probe(&config, &store);
    let starter = starter(&config, &cli, &report);
    let cycle =
        Scheduler::new(&clock, probe.as_ref(), &starter, &store, settings(&config)).cycle(true);
    finish(cycle)
}

/// Sends the starter message. Without `force`, only if no window is active.
pub fn start(config: Config, clock: SystemClock, force: bool, dry_run: bool) -> Result<()> {
    if dry_run {
        let cli = ClaudeCli::new(&config, claude::locate()?);
        let flags = cli
            .supported_flags()
            .inspect_err(|e| warn!(error = format!("{e:#}"), "cannot read `claude --help`"))
            .ok();
        let starter = ClaudeCliStarter::new(
            &cli,
            &config.starter_model,
            &config.starter_prompt,
            flags.as_ref(),
        );
        println!("{}", starter.command().describe());
        return Ok(());
    }

    let (cli, report) = startup(&config, &clock)?;
    let store = FileStateStore::new(config.state_file());
    let probe = build_probe(&config, &store);
    let starter = starter(&config, &cli, &report);
    let mut scheduler = Scheduler::new(&clock, probe.as_ref(), &starter, &store, settings(&config));
    let cycle = if force {
        scheduler.start()
    } else {
        scheduler.cycle(false)
    };
    finish(cycle)
}

/// Read-only: what detection sees, the last start, and when the daemon would act next.
pub fn status(config: Config, clock: SystemClock) -> Result<()> {
    let store = FileStateStore::new(config.state_file());
    let probe = build_probe(&config, &store);
    let now = clock.now();
    let result = probe.check(now);
    let state = store.load().unwrap_or_default();

    println!("detection:    {}", config.detection.as_str());
    match result.status {
        WindowStatus::Active { resets_at } => println!(
            "window:       active (source: {}), resets at {} (in {})",
            result.source,
            fmt_time(resets_at),
            human(resets_at - now)
        ),
        WindowStatus::Inactive => println!("window:       none active (source: {})", result.source),
        WindowStatus::Unknown => println!("window:       unknown (detection failed, see logs)"),
    }
    match state.last_start {
        Some(t) => println!("last start:   {} ({} ago)", fmt_time(t), human(now - t)),
        None => println!("last start:   never (by this service)"),
    }

    let inside_hours = match config.active_hours {
        Some(hours) => {
            let inside = hours.contains(now, config.timezone);
            println!(
                "active hours: {hours} {} (now {})",
                config.timezone,
                if inside { "inside" } else { "outside" }
            );
            inside
        }
        None => {
            println!("active hours: all day");
            true
        }
    };

    let next = match (inside_hours, result.status) {
        (false, _) => {
            let hours = config.active_hours.expect("outside hours implies a range");
            format!(
                "{} (active hours start)",
                fmt_time(hours.next_start(now, config.timezone))
            )
        }
        (true, WindowStatus::Active { resets_at }) => fmt_time(resets_at + GRACE),
        (true, WindowStatus::Inactive) => "now: the daemon would start a window".into(),
        (true, WindowStatus::Unknown) => fmt_time(now + to_delta(config.check_interval)),
    };
    println!("next check:   {next}");
    Ok(())
}

/// Validates config, `claude`, authentication and detection, then exits.
pub fn check(config: Config, clock: SystemClock) -> Result<()> {
    println!("config:       ok (DATA_DIR={})", config.data_dir.display());
    let (cli, report) = startup(&config, &clock)?;
    println!(
        "claude:       {} ({})",
        report.version,
        cli.binary().display()
    );
    println!(
        "auth:         claude.ai subscription, plan: {}",
        match &report.plan {
            Plan::Supported(p) | Plan::Other(p) => p.as_str(),
            Plan::Unknown => "not reported (token login)",
        }
    );
    let starter = starter(&config, &cli, &report);
    println!("starter:      {}", starter.command());

    let usage = UsageApiProbe::new(config.token.clone()).check(clock.now());
    println!(
        "usage API:    {}",
        match usage.status {
            WindowStatus::Active { resets_at } =>
                format!("ok, window active until {}", fmt_time(resets_at)),
            WindowStatus::Inactive => "ok, no window active".into(),
            WindowStatus::Unknown => match config.detection {
                Detection::OAuthUsage =>
                    "FAILED (see warning above); DETECTION=oauth-usage won't work".into(),
                Detection::Auto =>
                    "unavailable (see warning above); falling back to local state".into(),
                Detection::Local => "unavailable (not used: DETECTION=local)".into(),
            },
        }
    );
    if usage.status == WindowStatus::Unknown && config.detection == Detection::OAuthUsage {
        bail!("usage endpoint unavailable with DETECTION=oauth-usage");
    }
    println!("all checks passed");
    Ok(())
}

/// Locates `claude` and runs preflight. Errors are fatal.
fn startup(config: &Config, clock: &SystemClock) -> Result<(ClaudeCli, preflight::Report)> {
    let cli = ClaudeCli::new(config, claude::locate()?).cancel_on(clock.shutdown_flag());
    let report = preflight::run(config, &cli)?;
    match &report.plan {
        Plan::Supported(plan) => info!(version = %report.version, %plan, "claude ready"),
        Plan::Unknown => info!(
            version = %report.version,
            "claude ready (token login; plan not reported)"
        ),
        Plan::Other(plan) => warn!(
            version = %report.version,
            %plan,
            "plan is not Pro or Max; the 5-hour window behavior is unverified"
        ),
    }
    Ok((cli, report))
}

fn build_probe<'a>(config: &Config, store: &'a dyn StateStore) -> Box<dyn UsageProbe + 'a> {
    let api = || Box::new(UsageApiProbe::new(config.token.clone()));
    let local = || Box::new(LocalStateProbe::new(store));
    match config.detection {
        Detection::Auto => Box::new(AutoProbe::new(api(), local())),
        Detection::OAuthUsage => api(),
        Detection::Local => local(),
    }
}

fn starter(config: &Config, cli: &ClaudeCli, report: &preflight::Report) -> ClaudeCliStarter {
    ClaudeCliStarter::new(
        cli,
        &config.starter_model,
        &config.starter_prompt,
        report.flags.as_ref(),
    )
}

fn settings(config: &Config) -> Settings {
    Settings {
        check_interval: config.check_interval,
        active_hours: config.active_hours,
        timezone: config.timezone,
    }
}

/// Result of a one-shot command. A failed start is an error for scripts calling `once`/`start`.
fn finish(cycle: Cycle) -> Result<()> {
    match cycle {
        Cycle::StartFailed { .. } => bail!("could not start a window (see the error above)"),
        Cycle::Unknown { .. } => bail!("could not tell whether a window is active"),
        Cycle::Active { resets_at, source } => {
            info!(%source, resets_at = %fmt_time(resets_at), "window already active; nothing sent");
            Ok(())
        }
        Cycle::OutsideActiveHours { .. } | Cycle::Started { .. } | Cycle::LimitReached { .. } => {
            Ok(())
        }
    }
}

fn to_delta(d: std::time::Duration) -> TimeDelta {
    TimeDelta::from_std(d).unwrap_or(TimeDelta::MAX)
}

/// `2h 13m`, `45s`.
fn human(d: TimeDelta) -> String {
    let secs = d.num_seconds().max(0);
    match (secs / 3600, secs % 3600 / 60) {
        (0, 0) => format!("{secs}s"),
        (0, m) => format!("{m}m"),
        (h, m) => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_durations() {
        assert_eq!(human(TimeDelta::seconds(45)), "45s");
        assert_eq!(human(TimeDelta::seconds(600)), "10m");
        assert_eq!(human(TimeDelta::seconds(2 * 3600 + 13 * 60 + 5)), "2h 13m");
        assert_eq!(human(TimeDelta::seconds(-5)), "0s");
    }
}
