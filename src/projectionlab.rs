use std::path::Path;
use std::time::Duration;

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::Page;
use futures::StreamExt;
use totp_rs::{Algorithm, Secret, TOTP};

use crate::error::SyncError;

const APP_URL: &str = "https://app.projectionlab.com";
/// Polled after page loads / login submits to decide whether we're authenticated.
const PLUGIN_API_READY_POLL_ATTEMPTS: u32 = 20;
const PLUGIN_API_READY_POLL_INTERVAL_MS: u64 = 500;

pub struct ProjectionLabClient {
    browser: Browser,
    page: Page,
    api_key: String,
}

impl ProjectionLabClient {
    /// Launches a persistent headless Chrome profile, navigates to ProjectionLab,
    /// and logs in with email/password if there isn't already a valid session.
    pub async fn connect(
        profile_dir: &Path,
        email: &str,
        password: &str,
        totp_secret: Option<&str>,
        api_key: &str,
    ) -> Result<Self, SyncError> {
        let mut builder = BrowserConfig::builder()
            .user_data_dir(profile_dir)
            // Chromium's setuid sandbox needs privileges that Docker's default
            // seccomp profile denies, so it can't start inside the container even
            // now that we run as an unprivileged user. Harmless outside containers.
            .no_sandbox();

        if let Ok(executable) = std::env::var("CHROME_EXECUTABLE") {
            builder = builder.chrome_executable(executable);
        }

        let config = builder
            .build()
            .map_err(|e| SyncError::Browser(format!("building browser config: {e}")))?;

        let (browser, mut handler) = Browser::launch(config)
            .await
            .map_err(|e| SyncError::Browser(format!("launching chrome: {e}")))?;

        tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if let Err(e) = event {
                    tracing::debug!(error = %e, "chrome handler event error");
                }
            }
        });

        let page = browser
            .new_page(APP_URL)
            .await
            .map_err(|e| SyncError::Browser(format!("opening projectionlab: {e}")))?;

        let mut client = Self {
            browser,
            page,
            api_key: api_key.to_string(),
        };

        client.ensure_logged_in(email, password, totp_secret).await?;

        Ok(client)
    }

    async fn plugin_api_ready(&self) -> bool {
        self.page
            .evaluate("typeof window.projectionlabPluginAPI !== 'undefined'")
            .await
            .ok()
            .and_then(|v| v.into_value::<bool>().ok())
            .unwrap_or(false)
    }

    async fn wait_for_plugin_api(&self) -> bool {
        for _ in 0..PLUGIN_API_READY_POLL_ATTEMPTS {
            if self.plugin_api_ready().await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(PLUGIN_API_READY_POLL_INTERVAL_MS)).await;
        }
        false
    }

    async fn ensure_logged_in(
        &mut self,
        email: &str,
        password: &str,
        totp_secret: Option<&str>,
    ) -> Result<(), SyncError> {
        self.page
            .wait_for_navigation()
            .await
            .map_err(|e| SyncError::Browser(format!("waiting for initial load: {e}")))?;

        if self.wait_for_plugin_api().await {
            tracing::info!("reusing existing ProjectionLab session");
            return Ok(());
        }

        tracing::info!("no active session found, logging in to ProjectionLab");
        self.fill_and_submit_login(email, password).await?;

        if let Some(secret) = totp_secret {
            self.submit_otp(secret).await?;
        }

        if !self.wait_for_plugin_api().await {
            return Err(SyncError::Browser(
                "login did not result in an authenticated session (plugin API never became available, \
                 check whether an OTP step is required and totp_secret is configured)"
                    .to_string(),
            ));
        }

        tracing::info!("logged in to ProjectionLab successfully");
        Ok(())
    }

    /// Fills and submits the login form in a single JS round-trip.
    ///
    /// Earlier versions used chromiumoxide's find_element/click/type_str chain,
    /// which does a separate CDP call per step; ProjectionLab's SPA re-renders the
    /// DOM between those calls (e.g. hydrating in place after landing on the
    /// sign-up tab), so the node found in step 1 was sometimes gone by step 2
    /// ("Could not find node with given id"). Doing it all inside one `evaluate`
    /// call removes that race, and also lets us flip from the sign-up tab to the
    /// sign-in tab first if that's where we landed.
    async fn fill_and_submit_login(&self, email: &str, password: &str) -> Result<(), SyncError> {
        let script = format!(
            r#"
            (async () => {{
                const setNativeValue = (el, value) => {{
                    const proto = Object.getPrototypeOf(el);
                    const desc = Object.getOwnPropertyDescriptor(proto, 'value')
                        || Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value');
                    desc.set.call(el, value);
                    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                }};
                const clickByText = (candidates) => {{
                    const nodes = Array.from(document.querySelectorAll('button, a, [role=tab], [role=button]'));
                    const lower = candidates.map(c => c.toLowerCase());
                    const el = nodes.find(n => {{
                        const text = (n.textContent || '').trim().toLowerCase();
                        return lower.some(c => text === c || text.includes(c));
                    }});
                    if (el) {{ el.click(); return true; }}
                    return false;
                }};
                const sleep = (ms) => new Promise(r => setTimeout(r, ms));
                const findField = (selectors) => {{
                    for (const sel of selectors) {{
                        const el = document.querySelector(sel);
                        if (el) return el;
                    }}
                    return null;
                }};

                let emailField = null;
                let passwordField = null;
                // Long budget: cold headless launches can take a while to download and
                // hydrate the SPA bundle, and the cookie banner / sign-in tab may not
                // exist yet on early iterations.
                for (let i = 0; i < 60 && !(emailField && passwordField); i++) {{
                    const cookieAcceptBtn = document.querySelector('#onetrust-accept-btn-handler');
                    if (cookieAcceptBtn) cookieAcceptBtn.click();

                    emailField = findField(['input[type=email]', 'input[name=email]', "input[autocomplete='username']", 'input[placeholder*="email" i]']);
                    if (!emailField) {{
                        // If we're on the sign-up tab, switch to sign-in first...
                        clickByText(['sign in', 'log in']);
                        // ...then reveal the actual email/password fields (avoid the
                        // "with google" variant so we don't trigger an OAuth popup).
                        clickByText(['sign in with email', 'log in with email', 'continue with email']);
                    }}
                    passwordField = findField(['input[type=password]']);
                    if (!(emailField && passwordField)) await sleep(500);
                }}
                if (!emailField || !passwordField) {{
                    const inputs = Array.from(document.querySelectorAll('input')).map(el => ({{
                        type: el.type, name: el.name, id: el.id,
                        placeholder: el.placeholder, autocomplete: el.autocomplete,
                    }}));
                    const iframes = Array.from(document.querySelectorAll('iframe')).map(f => f.src);
                    return {{
                        ok: false,
                        reason: 'login-fields-not-found',
                        debug: {{
                            url: location.href,
                            title: document.title,
                            readyState: document.readyState,
                            bodyChildCount: document.body ? document.body.children.length : -1,
                            bodyTextSnippet: (document.body ? document.body.innerText : '').slice(0, 500),
                            inputs, iframes,
                        }},
                    }};
                }}

                setNativeValue(emailField, {email:?});
                setNativeValue(passwordField, {password:?});
                await sleep(100);

                let submitted = clickByText(['log in', 'sign in', 'continue']);
                if (!submitted) {{
                    const btn = document.querySelector('button[type=submit]');
                    if (btn) {{ btn.click(); submitted = true; }}
                }}

                return {{ ok: submitted, reason: submitted ? 'submitted' : 'submit-button-not-found' }};
            }})()
            "#,
            email = email,
            password = password,
        );

        #[derive(serde::Deserialize)]
        struct LoginResult {
            ok: bool,
            reason: String,
            #[serde(default)]
            debug: Option<serde_json::Value>,
        }

        let result: LoginResult = self
            .page
            .evaluate(script)
            .await
            .map_err(|e| SyncError::Browser(format!("submitting login form: {e}")))?
            .into_value()
            .map_err(|e| SyncError::Browser(format!("parsing login form result: {e}")))?;

        if !result.ok {
            if let Some(login_debug) = &result.debug {
                tracing::error!(debug = ?login_debug, "login form debug snapshot");
            }
            return Err(SyncError::Browser(format!(
                "login form submission failed: {}",
                result.reason
            )));
        }

        Ok(())
    }

    /// Generates the current TOTP code and submits it against ProjectionLab's
    /// OTP step, which follows the password step for accounts with 2FA enabled.
    async fn submit_otp(&self, totp_secret: &str) -> Result<(), SyncError> {
        // Give the OTP form a moment to render after the password submit navigation.
        let otp_field = 'wait_for_field: {
            for _ in 0..PLUGIN_API_READY_POLL_ATTEMPTS {
                if let Ok(el) = self
                    .page
                    .find_element("input[autocomplete='one-time-code'], input[type=tel], input[name*=otp i], input[name*=code i]")
                    .await
                {
                    break 'wait_for_field Some(el);
                }
                tokio::time::sleep(Duration::from_millis(PLUGIN_API_READY_POLL_INTERVAL_MS)).await;
            }
            None
        };

        let Some(otp_field) = otp_field else {
            tracing::debug!("no OTP field detected, assuming 2FA wasn't required this time");
            return Ok(());
        };

        let secret = Secret::Encoded(totp_secret.to_string())
            .to_bytes()
            .map_err(|e| SyncError::Browser(format!("decoding totp secret: {e:?}")))?;
        let totp = TOTP::new(Algorithm::SHA1, 6, 1, 30, secret)
            .map_err(|e| SyncError::Browser(format!("building totp generator: {e}")))?;
        let code = totp
            .generate_current()
            .map_err(|e| SyncError::Browser(format!("generating totp code: {e}")))?;

        otp_field
            .click()
            .await
            .map_err(|e| SyncError::Browser(e.to_string()))?
            .type_str(&code)
            .await
            .map_err(|e| SyncError::Browser(e.to_string()))?
            .press_key("Enter")
            .await
            .map_err(|e| SyncError::Browser(e.to_string()))?;

        tracing::info!("submitted TOTP code");
        Ok(())
    }

    /// Pushes a single account balance update via the Plugin API.
    pub async fn update_account_balance(
        &self,
        account_id: &str,
        balance: &str,
    ) -> Result<(), SyncError> {
        let script = format!(
            r#"
            (async () => {{
                await window.projectionlabPluginAPI.updateAccount(
                    {account_id:?},
                    {{ balance: Number({balance:?}) }},
                    {{ key: {api_key:?} }}
                );
                return true;
            }})()
            "#,
            account_id = account_id,
            balance = balance,
            api_key = self.api_key,
        );

        self.page
            .evaluate(script)
            .await
            .map_err(|e| SyncError::Browser(format!("updateAccount({account_id}) failed: {e}")))?;

        Ok(())
    }

    /// Clears the page's network resource-timing buffer, as a marker to diff
    /// against later via `network_requests_since_clear`.
    pub async fn clear_network_log(&self) -> Result<(), SyncError> {
        self.page
            .evaluate("performance.clearResourceTimings()")
            .await
            .map_err(|e| SyncError::Browser(format!("clearing network log: {e}")))?;
        Ok(())
    }

    /// Returns URLs of network requests observed since the last `clear_network_log`
    /// call, filtered to ones that look like backend/cloud-sync traffic (as opposed
    /// to static assets). Used to verify whether updateAccount() actually triggers
    /// a write to ProjectionLab's backend, versus only updating local browser state.
    pub async fn network_requests_since_clear(&self) -> Result<Vec<String>, SyncError> {
        let script = r#"
            performance.getEntriesByType('resource')
                .map(e => e.name)
                .filter(url => !/\.(js|css|png|jpg|svg|woff2?|ico)(\?|$)/i.test(url))
        "#;
        self.page
            .evaluate(script)
            .await
            .map_err(|e| SyncError::Browser(format!("reading network log: {e}")))?
            .into_value()
            .map_err(|e| SyncError::Browser(format!("parsing network log: {e}")))
    }

    /// Closes the browser cleanly. Since this runs as a one-shot process rather
    /// than a long-lived daemon, doing this explicitly (instead of relying on
    /// process exit / Drop) avoids leaving an orphaned Chrome process behind.
    pub async fn close(mut self) -> Result<(), SyncError> {
        self.browser
            .close()
            .await
            .map_err(|e| SyncError::Browser(format!("closing browser: {e}")))?;
        self.browser
            .wait()
            .await
            .map_err(|e| SyncError::Browser(format!("waiting for browser exit: {e}")))?;
        Ok(())
    }

    /// Pulls the full Current Finances export via the Plugin API, for inspecting
    /// account ids/names when building the `[[mapping]]` config.
    pub async fn export_data(&self) -> Result<serde_json::Value, SyncError> {
        let script = format!(
            r#"
            (async () => {{
                return await window.projectionlabPluginAPI.exportData({{ key: {api_key:?} }});
            }})()
            "#,
            api_key = self.api_key,
        );

        self.page
            .evaluate(script)
            .await
            .map_err(|e| SyncError::Browser(format!("exportData() failed: {e}")))?
            .into_value()
            .map_err(|e| SyncError::Browser(format!("parsing exportData() result: {e}")))
    }
}

/// Best-effort walk of an arbitrary JSON export looking for arrays of objects that
/// look like accounts (have an "id" plus a "name"/"title" field). ProjectionLab's
/// exportData() shape isn't publicly documented, so rather than hardcode one path
/// (e.g. `currentFinances.accounts`) we scan the whole tree and surface whatever
/// matches, alongside the raw JSON for anything this misses.
pub fn find_account_like_arrays(value: &serde_json::Value) -> Vec<(String, Vec<serde_json::Value>)> {
    let mut found = Vec::new();
    walk_for_accounts(value, "$", &mut found);
    found
}

fn walk_for_accounts(
    value: &serde_json::Value,
    path: &str,
    found: &mut Vec<(String, Vec<serde_json::Value>)>,
) {
    match value {
        serde_json::Value::Array(items) => {
            let looks_like_accounts = !items.is_empty()
                && items.iter().all(|item| {
                    item.as_object().is_some_and(|obj| {
                        obj.contains_key("id")
                            && (obj.contains_key("name") || obj.contains_key("title"))
                    })
                });
            if looks_like_accounts {
                found.push((path.to_string(), items.clone()));
            } else {
                for (i, item) in items.iter().enumerate() {
                    walk_for_accounts(item, &format!("{path}[{i}]"), found);
                }
            }
        }
        serde_json::Value::Object(obj) => {
            for (key, val) in obj {
                walk_for_accounts(val, &format!("{path}.{key}"), found);
            }
        }
        _ => {}
    }
}
