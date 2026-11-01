use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio_util::task::TaskTracker;

use crate::oauth;
use crate::store::{Administration, Session, Store};

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

    async fn read_collection<T: DeserializeOwned>(
        &self,
        mut url: Url,
        mut offset: usize,
        limit: Option<usize>,
    ) -> Result<Vec<T>> {
        let resource_path = url.path().to_owned();
        let mut seen = HashSet::new();
        let mut results = Vec::new();
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
            ensure!(!self.refreshes.is_closed(), "Exact client is shutting down");
            let token = self.access_token().await?;
            ensure!(!self.refreshes.is_closed(), "Exact client is shutting down");
            let response = self
                .http
                .get(url.clone())
                .bearer_auth(token)
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .context("Exact API request failed")?;
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                bail!("Exact rejected authorization; run `exact-mcp auth login` again");
            }
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                bail!("Exact API rate limit reached (HTTP 429); wait before trying again");
            }
            ensure!(
                status.is_success(),
                "Exact API request failed (HTTP {status})"
            );
            let page: Envelope<T> = response
                .json()
                .await
                .map_err(|_| anyhow::anyhow!("Invalid Exact API response"))?;
            let (rows, next) = match page.d {
                Page::Rows(rows) => (rows, None),
                Page::Paged { results, next } => (results, next),
            };
            let skipped = offset.min(rows.len());
            offset -= skipped;
            let remaining = limit.map_or(usize::MAX, |limit| limit - results.len());
            results.extend(rows.into_iter().skip(skipped).take(remaining));
            if limit.is_some_and(|limit| results.len() == limit) {
                return Ok(results);
            }
            match next {
                Some(next) => url = url.join(&next).context("Invalid Exact pagination URL")?,
                None => return Ok(results),
            }
        }
    }
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
