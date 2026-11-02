use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{Client, Response, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio_util::task::TaskTracker;

use crate::oauth;
use crate::store::{Administration, BankConfiguration, Session, Store};

#[cfg(test)]
mod tests;

const MAX_RATE_LIMIT_RETRIES: usize = 2;
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct ExactClient {
    directory: PathBuf,
    http: Client,
    base_url: Url,
    stored_origin: String,
    client_id: String,
    client_secret: Option<String>,
    initial_administration: Option<Administration>,
    pinned_administration: Option<u32>,
    refreshes: TaskTracker,
}

impl ExactClient {
    pub async fn new(
        directory: PathBuf,
        http: Client,
        client_secret: Option<String>,
    ) -> Result<Self> {
        let store = Store::open(&directory).await?;
        let session = store.require_session().await?;
        Ok(Self {
            directory,
            http,
            base_url: Url::parse(&session.base_url).context("Invalid stored Exact URL")?,
            stored_origin: session.base_url,
            client_id: session.client_id,
            client_secret,
            initial_administration: session.administration,
            pinned_administration: None,
            refreshes: TaskTracker::new(),
        })
    }

    pub fn pin_administration(&mut self) -> Result<Administration> {
        let administration = self.initial_administration.clone().context(
            "No administration selected; run `exact-mcp administrations select <code>` before serving MCP",
        )?;
        self.pinned_administration = Some(administration.code);
        Ok(administration)
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    fn check_scope(&self, session: &Session) -> Result<()> {
        ensure!(
            session.base_url == self.stored_origin
                && session.client_id == self.client_id
                && self.pinned_administration.is_none_or(|code| {
                    session.administration.as_ref().map(|item| item.code) == Some(code)
                }),
            "Account or administration changed; restart the MCP server before querying"
        );
        Ok(())
    }

    pub async fn session(&self) -> Result<Session> {
        let store = Store::open(&self.directory).await?;
        let session = store.require_session().await?;
        self.check_scope(&session)?;
        Ok(session)
    }

    pub async fn bank_configuration(&self) -> Result<Option<BankConfiguration>> {
        self.pinned_administration
            .context("Pin an administration before accessing bank configuration")?;
        let store = Store::open(&self.directory).await?;
        let session = store.require_session().await?;
        self.check_scope(&session)?;
        store.bank_configuration(&session).await
    }

    pub async fn save_bank_configuration(&self, configuration: BankConfiguration) -> Result<()> {
        self.pinned_administration
            .context("Pin an administration before accessing bank configuration")?;
        let store = Store::open(&self.directory).await?;
        let session = store.require_session().await?;
        self.check_scope(&session)?;
        store.save_bank_configuration(&session, configuration).await
    }

    pub async fn clear_bank_configuration(&self) -> Result<bool> {
        self.pinned_administration
            .context("Pin an administration before accessing bank configuration")?;
        let store = Store::open(&self.directory).await?;
        let session = store.require_session().await?;
        self.check_scope(&session)?;
        store.clear_bank_configuration(&session).await
    }

    pub async fn administrations(&self) -> Result<Vec<Administration>> {
        let me: Vec<Me> = self
            .get_collection(
                self.base_url
                    .join("/api/v1/current/Me?$select=CurrentDivision")?,
            )
            .await?;
        let current = me
            .first()
            .context("Exact returned no current user")?
            .current_division;
        self.get_collection(self.base_url.join(&format!(
            "/api/v1/{current}/system/Divisions?$select=Code,Description"
        ))?)
        .await
    }

    pub async fn select_administration(
        &self,
        available: &[Administration],
        code: u32,
    ) -> Result<Administration> {
        let selected = available
            .iter()
            .find(|item| item.code == code)
            .with_context(|| {
                format!("Administration {code} is not accessible; selection was not changed")
            })?
            .clone();
        let store = Store::open(&self.directory).await?;
        let mut session = store.require_session().await?;
        self.check_scope(&session)?;
        session.administration = Some(selected.clone());
        store.save(&session).await?;
        Ok(selected)
    }

    pub(crate) async fn access_token(&self) -> Result<String> {
        let session = self.session().await?;
        if session.tokens.expires_at > oauth::now()?.saturating_add(10) {
            return Ok(session.tokens.access_token);
        }
        let secret = self
            .client_secret
            .as_deref()
            .filter(|secret| !secret.is_empty())
            .context("Access token expired; set EXACT_CLIENT_SECRET to allow automatic refresh")?
            .to_owned();
        let registration = self.refreshes.token();
        ensure!(!self.refreshes.is_closed(), "Exact client is shutting down");
        let client = self.clone();
        tokio::spawn(async move {
            let _registration = registration;
            let store = Store::open(&client.directory).await?;
            let mut session = store.require_session().await?;
            client.check_scope(&session)?;
            if session.tokens.expires_at > oauth::now()?.saturating_add(10) {
                return Ok(session.tokens.access_token);
            }
            session.tokens = oauth::refresh_tokens(
                &client.http,
                &client.base_url,
                &client.client_id,
                &secret,
                &session.tokens.refresh_token,
            )
            .await?;
            store.save(&session).await?;
            Ok(session.tokens.access_token)
        })
        .await
        .map_err(|_| anyhow::anyhow!("Token refresh task failed"))?
    }

    pub async fn shutdown(&self) {
        self.refreshes.close();
        self.refreshes.wait().await;
    }

    pub async fn get_invoice_pdf_bytes(
        &self,
        division: u32,
        attachment_id: uuid::Uuid,
        limit: usize,
    ) -> Result<Vec<u8>> {
        ensure!(
            self.pinned_administration == Some(division),
            "Attachment administration must match the selected administration"
        );
        ensure!(!self.refreshes.is_closed(), "Exact client is shutting down");
        let mut url = self.base_url.join("/docs/SysAttachment.aspx")?;
        url.query_pairs_mut()
            .append_pair("ID", &attachment_id.to_string())
            .append_pair("_Division_", &division.to_string())
            .append_pair("Download", "1");
        let mut response = self
            .get_response(url, "application/pdf", "Exact PDF download request failed")
            .await?;
        ensure!(
            response
                .content_length()
                .is_none_or(|size| size <= limit as u64),
            "Exact PDF download response exceeds the download size limit"
        );
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("Cannot read Exact PDF download response")?
        {
            ensure!(
                body.len().saturating_add(chunk.len()) <= limit,
                "Exact PDF download response exceeds the download size limit"
            );
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    pub(crate) async fn get_match_sets_page(
        &self,
        division: u32,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<u8>> {
        ensure!(
            self.pinned_administration == Some(division),
            "Matching administration must match the selected administration"
        );
        if let Some(cursor) = cursor {
            ensure!(
                crate::matching::valid_cursor(cursor),
                "Invalid matching export cursor"
            );
        }
        ensure!(!self.refreshes.is_closed(), "Exact client is shutting down");
        let mut url = self.base_url.join("/docs/XMLDownload.aspx")?;
        url.query_pairs_mut()
            .append_pair("Topic", "MatchSets")
            .append_pair("_Division_", &division.to_string());
        if let Some(cursor) = cursor {
            url.query_pairs_mut().append_pair("TSPaging", cursor);
        }
        let mut response = self
            .get_response(
                url,
                "application/xml",
                "Exact matching export request failed",
            )
            .await?;
        ensure!(
            response
                .content_length()
                .is_none_or(|size| size <= limit as u64),
            "Exact matching export exceeds the size limit; no partial matching returned"
        );
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("Cannot read Exact matching export")?
        {
            ensure!(
                body.len().saturating_add(chunk.len()) <= limit,
                "Exact matching export exceeds the size limit; no partial matching returned"
            );
            body.extend_from_slice(&chunk);
        }
        self.session().await?;
        Ok(body)
    }

    pub async fn save_invoice_pdf(
        &self,
        division: u32,
        attachment_id: uuid::Uuid,
        bytes: Vec<u8>,
    ) -> Result<PathBuf> {
        ensure!(
            self.pinned_administration == Some(division),
            "Download administration must match the selected administration"
        );
        ensure!(!self.refreshes.is_closed(), "Exact client is shutting down");
        let store = Store::open(&self.directory).await?;
        let session = store.require_session().await?;
        self.check_scope(&session)?;
        store.save_invoice_pdf(division, attachment_id, bytes).await
    }

    pub async fn get_collection<T: DeserializeOwned>(&self, url: Url) -> Result<Vec<T>> {
        self.read_collection(url, 0, None).await
    }

    pub async fn get_collection_window<T: DeserializeOwned>(
        &self,
        url: Url,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<T>> {
        self.read_collection(url, offset, Some(limit)).await
    }

    pub(crate) async fn try_for_each_page<T: DeserializeOwned, F: FnMut(Vec<T>) -> Result<()>>(
        &self,
        url: Url,
        mut visit: F,
    ) -> Result<()> {
        self.visit_pages(url, |rows| {
            visit(rows)?;
            Ok(true)
        })
        .await
    }

    async fn read_collection<T: DeserializeOwned>(
        &self,
        url: Url,
        mut offset: usize,
        limit: Option<usize>,
    ) -> Result<Vec<T>> {
        let mut results = Vec::new();
        self.visit_pages(url, |rows| {
            let skipped = offset.min(rows.len());
            offset -= skipped;
            let remaining = limit.map_or(usize::MAX, |limit| limit - results.len());
            results.extend(rows.into_iter().skip(skipped).take(remaining));
            Ok(!limit.is_some_and(|limit| results.len() == limit))
        })
        .await?;
        Ok(results)
    }

    async fn visit_pages<T: DeserializeOwned, F: FnMut(Vec<T>) -> Result<bool>>(
        &self,
        mut url: Url,
        mut visit: F,
    ) -> Result<()> {
        let resource_path = url.path().to_owned();
        let mut seen = HashSet::new();
        loop {
            ensure!(
                url.origin() == self.base_url.origin()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.fragment().is_none()
                    && url.path().starts_with("/api/v1/")
                    && url.path() == resource_path,
                "Refusing an Exact pagination URL outside this account's API resource"
            );
            ensure!(
                seen.insert(url.to_string()),
                "Exact returned a pagination loop"
            );
            let response = self
                .get_response(url.clone(), "application/json", "Exact API request failed")
                .await?;
            let page: Envelope<T> = response
                .json()
                .await
                .map_err(|_| anyhow::anyhow!("Invalid Exact API response"))?;
            let (rows, next) = match page.d {
                Page::Rows(rows) => (rows, None),
                Page::Paged { results, next } => (results, next),
            };
            if !visit(rows)? {
                return Ok(());
            }
            match next {
                Some(next) => url = url.join(&next).context("Invalid Exact pagination URL")?,
                None => return Ok(()),
            }
        }
    }

    async fn get_response(
        &self,
        url: Url,
        accept: &'static str,
        failure: &'static str,
    ) -> Result<Response> {
        let mut waited = Duration::ZERO;
        let mut delay = None;
        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            let response = self
                .send_get_after(url.clone(), accept, failure, delay)
                .await?;
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                bail!("Exact rejected authorization; run `exact-mcp auth login` again");
            }
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                delay = bounded_rate_limit_delay(
                    response.headers(),
                    SystemTime::now(),
                    attempt,
                    waited,
                );
                drop(response);
                waited += delay
                    .context("Exact API rate limit reached (HTTP 429); wait before trying again")?;
                continue;
            }
            ensure!(status.is_success(), "{failure} (HTTP {status})");
            return Ok(response);
        }
        bail!("Exact API rate limit reached (HTTP 429); wait before trying again")
    }

    async fn send_get_after(
        &self,
        url: Url,
        accept: &'static str,
        failure: &'static str,
        delay: Option<Duration>,
    ) -> Result<Response> {
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        ensure!(!self.refreshes.is_closed(), "Exact client is shutting down");
        let token = self.access_token().await?;
        ensure!(!self.refreshes.is_closed(), "Exact client is shutting down");
        self.http
            .get(url)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, accept)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!(failure))
    }
}

fn bounded_rate_limit_delay(
    headers: &HeaderMap,
    now: SystemTime,
    attempt: usize,
    waited: Duration,
) -> Option<Duration> {
    if attempt >= MAX_RATE_LIMIT_RETRIES {
        return None;
    }
    let delay = rate_limit_delay(headers, now, attempt);
    (delay <= MAX_RATE_LIMIT_WAIT.saturating_sub(waited)).then_some(delay)
}

fn rate_limit_delay(headers: &HeaderMap, now: SystemTime, attempt: usize) -> Duration {
    let mut delay = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| retry_after_delay(value, now));
    for (reset, remaining) in [
        ("x-ratelimit-reset", "x-ratelimit-remaining"),
        (
            "x-ratelimit-minutely-reset",
            "x-ratelimit-minutely-remaining",
        ),
    ] {
        if headers.get(remaining).is_some_and(|value| value != "0") {
            continue;
        }
        let reset_delay = headers
            .get(reset)
            .and_then(|value| value.to_str().ok())
            .and_then(decimal_integer)
            .and_then(|millis| UNIX_EPOCH.checked_add(Duration::from_millis(millis)))
            .map(|reset| reset.duration_since(now).unwrap_or(Duration::ZERO));
        if let Some(reset_delay) = reset_delay {
            delay = Some(delay.map_or(reset_delay, |delay| delay.max(reset_delay)));
        }
    }
    delay.unwrap_or(Duration::from_secs(1 + attempt as u64))
}

fn decimal_integer(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn retry_after_delay(value: &str, now: SystemTime) -> Option<Duration> {
    if let Some(seconds) = decimal_integer(value) {
        return Some(Duration::from_secs(seconds));
    }
    let timestamp = chrono::DateTime::parse_from_rfc2822(value)
        .map(|date| date.timestamp())
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(value, "%A, %d-%b-%y %H:%M:%S GMT")
                .map(|date| date.and_utc().timestamp())
        })
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(value, "%a %b %e %H:%M:%S %Y")
                .map(|date| date.and_utc().timestamp())
        })
        .ok()?;
    let seconds = u64::try_from(timestamp).unwrap_or(0);
    UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))
        .map(|date| date.duration_since(now).unwrap_or(Duration::ZERO))
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Me {
    current_division: u32,
}

#[derive(Deserialize)]
struct Envelope<T> {
    d: Page<T>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Page<T> {
    Rows(Vec<T>),
    Paged {
        results: Vec<T>,
        #[serde(rename = "__next")]
        next: Option<String>,
    },
}
