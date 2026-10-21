use croner::Cron;
use serde::Deserialize;
use std::path::Path;
use std::str::FromStr;

use crate::error::SyncError;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub simplefin: SimplefinConfig,
    pub projectionlab: ProjectionLabConfig,
    #[serde(default)]
    pub schedule: ScheduleConfig,
    #[serde(rename = "mapping", default)]
    pub mappings: Vec<AccountMapping>,
}

#[derive(Debug, Deserialize)]
pub struct SimplefinConfig {
    pub access_url: String,
}

#[derive(Debug, Deserialize)]
pub struct ProjectionLabConfig {
    pub email: String,
    pub password: String,
    pub api_key: String,
    #[serde(default)]
    pub totp_secret: Option<String>,
    #[serde(default = "default_profile_dir")]
    pub profile_dir: String,
}

fn default_profile_dir() -> String {
    "./.pl-profile".to_string()
}

#[derive(Debug, Deserialize)]
pub struct ScheduleConfig {
    /// Cron expression describing when to run the sync, evaluated in the
    /// container/host's local timezone. Standard 5-field crontab syntax
    /// ("min hour day-of-month month day-of-week"), with an optional leading
    /// seconds field, an optional trailing year field, and `@daily`-style
    /// nicknames also accepted.
    #[serde(default = "default_cron")]
    pub cron: String,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            cron: default_cron(),
        }
    }
}

fn default_cron() -> String {
    "0 0 * * *".to_string()
}

impl ScheduleConfig {
    pub fn parsed_cron(&self) -> Result<Cron, SyncError> {
        Cron::from_str(&self.cron).map_err(|e| {
            SyncError::Config(format!(
                "invalid schedule.cron {:?} (expected a cron expression like \"0 0 * * *\"): {e}",
                self.cron
            ))
        })
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct AccountMapping {
    pub simplefin_account_id: String,
    pub projectionlab_account_id: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, SyncError> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| SyncError::Config(format!("reading {}: {e}", path.display())))?;
        Self::from_str(&contents)
    }

    pub fn from_str(contents: &str) -> Result<Self, SyncError> {
        toml::from_str(contents).map_err(|e| SyncError::Config(e.to_string()))
    }

    /// Builds config entirely from environment variables, for docker-compose /
    /// k8s setups that inject config via `environment:`/`env:` rather than a
    /// mounted file. Triggered when `SIMPLEFIN_ACCESS_URL` is set; see
    /// `config.example.env` for the full variable list.
    pub fn from_env() -> Result<Self, SyncError> {
        fn require(name: &str) -> Result<String, SyncError> {
            std::env::var(name)
                .map_err(|_| SyncError::Config(format!("missing required env var {name}")))
        }

        let simplefin = SimplefinConfig {
            access_url: require("SIMPLEFIN_ACCESS_URL")?,
        };

        let projectionlab = ProjectionLabConfig {
            email: require("PROJECTIONLAB_EMAIL")?,
            password: require("PROJECTIONLAB_PASSWORD")?,
            api_key: require("PROJECTIONLAB_API_KEY")?,
            totp_secret: std::env::var("PROJECTIONLAB_TOTP_SECRET").ok(),
            profile_dir: std::env::var("PROJECTIONLAB_PROFILE_DIR")
                .unwrap_or_else(|_| default_profile_dir()),
        };

        let schedule = ScheduleConfig {
            cron: std::env::var("SCHEDULE_CRON").unwrap_or_else(|_| default_cron()),
        };

        let mappings = match std::env::var("MAPPINGS") {
            Ok(raw) => parse_mappings(&raw)?,
            Err(_) => Vec::new(),
        };

        Ok(Self {
            simplefin,
            projectionlab,
            schedule,
            mappings,
        })
    }
}

/// Parses `MAPPINGS` as comma-separated `simplefin_id:projectionlab_id` pairs,
/// e.g. `ACT-abc:pl-123,ACT-def:pl-456`.
fn parse_mappings(raw: &str) -> Result<Vec<AccountMapping>, SyncError> {
    raw.split(',')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (simplefin_account_id, projectionlab_account_id) =
                pair.split_once(':').ok_or_else(|| {
                    SyncError::Config(format!(
                        "invalid MAPPINGS entry {pair:?}, expected simplefin_id:projectionlab_id"
                    ))
                })?;
            Ok(AccountMapping {
                simplefin_account_id: simplefin_account_id.trim().to_string(),
                projectionlab_account_id: projectionlab_account_id.trim().to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_example_config() {
        let contents = include_str!("../config.example.toml");
        let config = Config::from_str(contents).expect("example config should parse");

        assert_eq!(config.mappings.len(), 1);
        assert_eq!(config.mappings[0].simplefin_account_id, "ACT-example-1");
        assert_eq!(
            config.mappings[0].projectionlab_account_id,
            "pl-account-id-1"
        );
    }

    #[test]
    fn defaults_apply_when_omitted() {
        let contents = r#"
[simplefin]
access_url = "https://user:pass@bridge.simplefin.org/simplefin"

[projectionlab]
email = "me@example.com"
password = "secret"
api_key = "key"
"#;
        let config = Config::from_str(contents).expect("should parse with defaults");
        assert_eq!(config.projectionlab.profile_dir, "./.pl-profile");
        assert_eq!(config.schedule.cron, "0 0 * * *");
        assert!(config.mappings.is_empty());
    }

    #[test]
    fn parses_five_field_cron() {
        let schedule = ScheduleConfig {
            cron: "30 14 * * *".to_string(),
        };
        let parsed = schedule.parsed_cron().unwrap();

        let now = chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
            .unwrap()
            .and_hms_opt(10, 0, 0)
            .unwrap()
            .and_utc();
        assert_eq!(
            parsed.find_next_occurrence(&now, false).unwrap(),
            chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
                .unwrap()
                .and_hms_opt(14, 30, 0)
                .unwrap()
                .and_utc()
        );
    }

    #[test]
    fn rejects_invalid_cron() {
        let schedule = ScheduleConfig {
            cron: "not-a-cron".to_string(),
        };
        assert!(schedule.parsed_cron().is_err());
    }

    #[test]
    fn parses_mappings_env_var() {
        let mappings = parse_mappings("ACT-abc:pl-123, ACT-def:pl-456").unwrap();
        assert_eq!(mappings.len(), 2);
        assert_eq!(mappings[0].simplefin_account_id, "ACT-abc");
        assert_eq!(mappings[0].projectionlab_account_id, "pl-123");
        assert_eq!(mappings[1].simplefin_account_id, "ACT-def");
        assert_eq!(mappings[1].projectionlab_account_id, "pl-456");
    }

    #[test]
    fn empty_mappings_env_var_is_empty_list() {
        assert!(parse_mappings("").unwrap().is_empty());
        assert!(parse_mappings("  ").unwrap().is_empty());
    }

    #[test]
    fn rejects_malformed_mappings_entry() {
        assert!(parse_mappings("ACT-abc-no-colon").is_err());
    }
}
