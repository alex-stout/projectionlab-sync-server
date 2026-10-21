use std::collections::HashMap;

use base64::Engine;
use reqwest::Client;
use serde::Deserialize;

use crate::error::SyncError;

/// Exchanges a SimpleFIN setup token (the base64 blob you get from your bank's
/// SimpleFIN `/create` page) for a permanent access URL, per the claim step of
/// the SimpleFIN protocol: https://www.simplefin.org/protocol.html
pub async fn claim_access_url(client: &Client, setup_token: &str) -> Result<String, SyncError> {
    let claim_url_bytes = base64::engine::general_purpose::STANDARD
        .decode(setup_token.trim())
        .map_err(|e| SyncError::InvalidAccessUrl(format!("decoding setup token: {e}")))?;
    let claim_url = String::from_utf8(claim_url_bytes)
        .map_err(|e| SyncError::InvalidAccessUrl(format!("setup token is not valid utf-8: {e}")))?;

    let response = client
        .post(&claim_url)
        .header("Content-Length", "0")
        .send()
        .await?
        .error_for_status()?;

    let access_url = response.text().await?;
    Ok(access_url.trim().to_string())
}

#[derive(Debug, Deserialize)]
pub struct AccountSet {
    #[serde(rename = "errlist", default)]
    pub errors: Vec<SimplefinError>,
    #[serde(default)]
    pub accounts: Vec<SimplefinAccount>,
}

#[derive(Debug, Deserialize)]
pub struct SimplefinError {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub msg: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SimplefinAccount {
    pub id: String,
    pub name: String,
    pub balance: String,
    #[serde(rename = "balance-date")]
    #[allow(dead_code)]
    pub balance_date: Option<i64>,
}

/// Splits a SimpleFIN access URL like `https://user:pass@host/simplefin`
/// into (base_url_without_credentials, username, password).
pub fn split_access_url(access_url: &str) -> Result<(String, String, String), SyncError> {
    let url = reqwest::Url::parse(access_url)
        .map_err(|e| SyncError::InvalidAccessUrl(format!("{access_url}: {e}")))?;

    let username = url.username().to_string();
    let password = url.password().unwrap_or("").to_string();

    if username.is_empty() {
        return Err(SyncError::InvalidAccessUrl(
            "access url is missing basic auth credentials".to_string(),
        ));
    }

    let mut base = url.clone();
    base.set_username("").ok();
    base.set_password(None).ok();

    Ok((base.to_string(), username, password))
}

/// Fetches balances-only account data from the SimpleFIN Bridge `/accounts` endpoint
/// and returns a map of account id -> balance (as reported by SimpleFIN, string form).
pub async fn fetch_balances(
    client: &Client,
    access_url: &str,
) -> Result<HashMap<String, SimplefinAccount>, SyncError> {
    let (base_url, username, password) = split_access_url(access_url)?;
    let accounts_url = format!("{}/accounts", base_url.trim_end_matches('/'));

    let response = client
        .get(&accounts_url)
        .basic_auth(username, Some(password))
        .query(&[("version", "2"), ("balances-only", "1")])
        .send()
        .await?
        .error_for_status()?;

    let account_set: AccountSet = response.json().await?;

    for err in &account_set.errors {
        tracing::warn!(
            code = ?err.code,
            msg = ?err.msg,
            "simplefin reported an error"
        );
    }

    Ok(account_set
        .accounts
        .into_iter()
        .map(|a| (a.id.clone(), a))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_credentials_out_of_access_url() {
        let (base, user, pass) =
            split_access_url("https://user123:tok456@bridge.simplefin.org/simplefin").unwrap();
        assert_eq!(base, "https://bridge.simplefin.org/simplefin");
        assert_eq!(user, "user123");
        assert_eq!(pass, "tok456");
    }

    #[test]
    fn rejects_url_without_credentials() {
        let result = split_access_url("https://bridge.simplefin.org/simplefin");
        assert!(result.is_err());
    }

    /// Hits SimpleFIN's real public demo bridge. Not run by default (needs network);
    /// `cargo test -- --ignored` to confirm the schema still matches the live API.
    #[tokio::test]
    #[ignore]
    async fn fetches_balances_from_live_demo_bridge() {
        let client = Client::new();
        let balances = fetch_balances(
            &client,
            "https://demo:demo@beta-bridge.simplefin.org/simplefin",
        )
        .await
        .expect("demo bridge should respond");

        assert!(!balances.is_empty());
        assert!(balances.values().any(|a| a.name.contains("Checking")));
    }

    #[test]
    fn parses_account_set_json() {
        let json = r#"
        {
            "errlist": [],
            "accounts": [
                {
                    "id": "ACT-123",
                    "name": "Checking",
                    "currency": "USD",
                    "balance": "1234.56",
                    "balance-date": 1700000000
                }
            ]
        }
        "#;

        let account_set: AccountSet = serde_json::from_str(json).unwrap();
        assert_eq!(account_set.accounts.len(), 1);
        assert_eq!(account_set.accounts[0].id, "ACT-123");
        assert_eq!(account_set.accounts[0].balance, "1234.56");
    }
}
