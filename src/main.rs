mod config;
mod error;
mod projectionlab;
mod simplefin;

use std::path::{Path, PathBuf};

use chrono::Timelike;
use config::Config;
use croner::Cron;
use projectionlab::ProjectionLabClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                // chromiumoxide logs a WARN for every CDP event it can't deserialize
                // against its (slightly stale) schema, e.g. `Page.frameRequestedNavigation`
                // and similar events we never consume. Harmless, but noisy at info level.
                "info,chromiumoxide::handler=error".into()
            }),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("claim") {
        let Some(setup_token) = args.get(2) else {
            eprintln!("usage: {} claim <base64-setup-token>", args[0]);
            std::process::exit(1);
        };
        let access_url =
            simplefin::claim_access_url(&reqwest::Client::new(), setup_token).await?;
        println!("{access_url}");
        println!("(paste this into config.toml as [simplefin] access_url; it can only be claimed once)");
        return Ok(());
    }

    let config = if std::env::var("SIMPLEFIN_ACCESS_URL").is_ok() {
        Config::from_env()?
    } else {
        let config_path = std::env::var("SYNC_CONFIG")
            .unwrap_or_else(|_| "config.toml".to_string());
        Config::load(Path::new(&config_path))?
    };

    if args.get(1).map(String::as_str) == Some("list-accounts") {
        return list_accounts(&config).await;
    }

    if args.get(1).map(String::as_str) == Some("run-once") {
        tracing::info!(mappings = config.mappings.len(), "starting one-shot run");
        return run_full_cycle(&config).await;
    }

    // Default: long-running process with an internal cron scheduler. Each cycle
    // still opens a fresh browser, syncs, and closes it; the browser/session is
    // never kept alive between runs, only the process itself stays up to wait for
    // the next scheduled time (and to receive SIGTERM cleanly from an orchestrator
    // like k8s in between runs).
    let schedule = config.schedule.parsed_cron()?;
    tracing::info!(
        mappings = config.mappings.len(),
        cron = %config.schedule.cron,
        "starting projectionlab-sync-server (cron scheduler)"
    );

    loop {
        let sleep_duration = duration_until_next(&schedule)?;
        tracing::info!(sleep_secs = sleep_duration.as_secs(), "sleeping until next scheduled run");

        tokio::select! {
            _ = tokio::time::sleep(sleep_duration) => {}
            _ = shutdown_signal() => {
                tracing::info!("received shutdown signal, exiting");
                return Ok(());
            }
        }

        if let Err(e) = run_full_cycle(&config).await {
            tracing::error!(error = %e, "sync cycle failed, will retry at next scheduled time");
        }
    }
}

/// Time remaining until the next time `schedule` fires, in local time.
fn duration_until_next(schedule: &Cron) -> anyhow::Result<std::time::Duration> {
    // Truncated to the second so the wake-up lands on the scheduled instant
    // rather than carrying `now`'s sub-second remainder into every occurrence.
    let now = chrono::Local::now()
        .with_nanosecond(0)
        .unwrap_or_else(chrono::Local::now);
    duration_until_next_from(schedule, now)
}

fn duration_until_next_from<Tz: chrono::TimeZone>(
    schedule: &Cron,
    now: chrono::DateTime<Tz>,
) -> anyhow::Result<std::time::Duration> {
    // `inclusive = false`: a cycle that finishes within the same second it was
    // scheduled for must not immediately re-fire on that same occurrence.
    let next = schedule.find_next_occurrence(&now, false)?;
    Ok((next - now)
        .to_std()
        .unwrap_or(std::time::Duration::from_secs(0)))
}

#[cfg(test)]
mod scheduler_tests {
    use super::*;
    use chrono::NaiveDate;
    use std::str::FromStr;

    fn at(hour: u32, min: u32, sec: u32) -> chrono::DateTime<chrono::Utc> {
        NaiveDate::from_ymd_opt(2026, 1, 15)
            .unwrap()
            .and_hms_opt(hour, min, sec)
            .unwrap()
            .and_utc()
    }

    #[test]
    fn schedules_later_today_when_time_hasnt_passed() {
        let schedule = Cron::from_str("30 14 * * *").unwrap();

        let result = duration_until_next_from(&schedule, at(10, 0, 0)).unwrap();
        assert_eq!(result, std::time::Duration::from_secs(4 * 3600 + 1800));
    }

    #[test]
    fn schedules_tomorrow_when_time_already_passed_today() {
        let schedule = Cron::from_str("0 0 * * *").unwrap();

        let result = duration_until_next_from(&schedule, at(23, 0, 0)).unwrap();
        assert_eq!(result, std::time::Duration::from_secs(3600));
    }

    #[test]
    fn schedules_tomorrow_when_time_exactly_now() {
        let schedule = Cron::from_str("0 0 * * *").unwrap();

        let result = duration_until_next_from(&schedule, at(0, 0, 0)).unwrap();
        assert_eq!(result, std::time::Duration::from_secs(24 * 3600));
    }

    #[test]
    fn supports_sub_daily_schedules() {
        let schedule = Cron::from_str("0 */6 * * *").unwrap();

        let result = duration_until_next_from(&schedule, at(10, 0, 0)).unwrap();
        assert_eq!(result, std::time::Duration::from_secs(2 * 3600));
    }

    #[test]
    fn supports_day_of_week_schedules() {
        // 2026-01-15 is a Thursday; the next Monday 09:00 is 3 days + 23h out.
        let schedule = Cron::from_str("0 9 * * MON").unwrap();

        let result = duration_until_next_from(&schedule, at(10, 0, 0)).unwrap();
        assert_eq!(
            result,
            std::time::Duration::from_secs(3 * 24 * 3600 + 23 * 3600)
        );
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        sigterm.recv().await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

/// Runs one full connect -> sync -> close cycle against ProjectionLab.
async fn run_full_cycle(config: &Config) -> anyhow::Result<()> {
    let http_client = reqwest::Client::new();
    let profile_dir = PathBuf::from(&config.projectionlab.profile_dir);
    let pl_client = ProjectionLabClient::connect(
        &profile_dir,
        &config.projectionlab.email,
        &config.projectionlab.password,
        config.projectionlab.totp_secret.as_deref(),
        &config.projectionlab.api_key,
    )
    .await?;

    if let Err(e) = pl_client.clear_network_log().await {
        tracing::warn!(error = %e, "failed to clear network log");
    }

    let sync_result = run_sync_cycle(&http_client, &pl_client, config).await;

    // updateAccount()'s promise resolving doesn't guarantee ProjectionLab has
    // finished persisting the change (e.g. a debounced save or background sync
    // to their backend could still be in flight), so give it a moment before we
    // kill the browser out from under it.
    tracing::info!("pausing before closing the browser to let ProjectionLab finish persisting");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    match pl_client.network_requests_since_clear().await {
        Ok(urls) => tracing::info!(?urls, "network requests observed since updates began"),
        Err(e) => tracing::warn!(error = %e, "failed to read network log"),
    }

    if let Err(e) = pl_client.close().await {
        tracing::warn!(error = %e, "failed to close browser cleanly");
    }

    sync_result
}

/// Prints every account SimpleFIN reports and every account-like list found in
/// ProjectionLab's exportData(), so the user can read off ids for `[[mapping]]`
/// entries in config.toml.
async fn list_accounts(config: &Config) -> anyhow::Result<()> {
    let http_client = reqwest::Client::new();

    println!("=== SimpleFIN accounts ===");
    match simplefin::fetch_balances(&http_client, &config.simplefin.access_url).await {
        Ok(balances) => {
            if balances.is_empty() {
                println!("(none returned)");
            }
            for account in balances.values() {
                println!(
                    "  id={:<25} balance={:<12} name={}",
                    account.id, account.balance, account.name
                );
            }
        }
        Err(e) => println!("  failed to fetch: {e}"),
    }

    println!("\n=== ProjectionLab accounts ===");
    let profile_dir = PathBuf::from(&config.projectionlab.profile_dir);
    let pl_client = ProjectionLabClient::connect(
        &profile_dir,
        &config.projectionlab.email,
        &config.projectionlab.password,
        config.projectionlab.totp_secret.as_deref(),
        &config.projectionlab.api_key,
    )
    .await?;

    let export_result = pl_client.export_data().await;

    if let Err(e) = pl_client.close().await {
        tracing::warn!(error = %e, "failed to close browser cleanly");
    }

    let export = export_result?;
    let account_like_arrays = projectionlab::find_account_like_arrays(&export);

    if account_like_arrays.is_empty() {
        println!("  couldn't find an obvious accounts list; dumping full exportData() below.");
        println!("{}", serde_json::to_string_pretty(&export)?);
    } else {
        for (path, accounts) in &account_like_arrays {
            println!("  found at {path}:");
            for account in accounts {
                let id = account.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                let name = account
                    .get("name")
                    .or_else(|| account.get("title"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let balance = account.get("balance").map(|v| v.to_string()).unwrap_or_default();
                println!("    id={id:<25} balance={balance:<12} name={name}");
            }
        }
    }

    Ok(())
}

async fn run_sync_cycle(
    http_client: &reqwest::Client,
    pl_client: &ProjectionLabClient,
    config: &Config,
) -> anyhow::Result<()> {
    let balances =
        simplefin::fetch_balances(http_client, &config.simplefin.access_url).await?;

    let mut updated = 0;
    let mut skipped = 0;
    let mut errored = 0;

    for mapping in &config.mappings {
        let Some(account) = balances.get(&mapping.simplefin_account_id) else {
            tracing::warn!(
                simplefin_account_id = %mapping.simplefin_account_id,
                "no matching account returned by simplefin, skipping"
            );
            skipped += 1;
            continue;
        };

        match pl_client
            .update_account_balance(&mapping.projectionlab_account_id, &account.balance)
            .await
        {
            Ok(()) => {
                tracing::info!(
                    projectionlab_account_id = %mapping.projectionlab_account_id,
                    simplefin_account_name = %account.name,
                    balance = %account.balance,
                    "updated account balance"
                );
                updated += 1;
            }
            Err(e) => {
                tracing::error!(
                    projectionlab_account_id = %mapping.projectionlab_account_id,
                    error = %e,
                    "failed to update account balance"
                );
                errored += 1;
            }
        }
    }

    tracing::info!(updated, skipped, errored, "sync cycle complete");

    Ok(())
}
