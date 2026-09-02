use std::collections::HashSet;

use anyhow::{Context, Result, bail, ensure};
use reqwest::Url;
use reqwest::blocking::Client;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::oauth;
use crate::store::{Administration, Session, Store};

pub fn administrations(
    client: &Client,
    store: &Store,
    session: &mut Session,
    client_secret: Option<&str>,
) -> Result<Vec<Administration>> {
    let base = Url::parse(&session.base_url).context("Invalid stored Exact URL")?;
    let me: Vec<Me> = get_collection(
        client,
        store,
        session,
        client_secret,
        base.join("/api/v1/current/Me?$select=CurrentDivision")?,
    )?;
    let current = me
        .first()
        .context("Exact returned no current user")?
        .current_division;
    get_collection(
        client,
        store,
        session,
        client_secret,
        base.join(&format!(
            "/api/v1/{current}/system/Divisions?$select=Code,Description"
        ))?,
    )
}

pub fn select_administration(
    store: &Store,
    session: &mut Session,
    available: &[Administration],
    code: u32,
) -> Result<Administration> {
    let selected = available
        .iter()
        .find(|administration| administration.code == code)
        .with_context(|| {
            format!("Administration {code} is not accessible; selection was not changed")
        })?
        .clone();
    session.administration = Some(selected.clone());
    store.save(session)?;
    Ok(selected)
}

fn access_token(
    client: &Client,
    store: &Store,
    session: &mut Session,
    client_secret: Option<&str>,
) -> Result<()> {
    if session.tokens.expires_at > oauth::now()?.saturating_add(10) {
        return Ok(());
    }
    let secret = client_secret
        .filter(|secret| !secret.is_empty())
        .context("Access token expired; set EXACT_CLIENT_SECRET to allow automatic refresh")?;
    let base = Url::parse(&session.base_url)?;
    session.tokens = oauth::refresh_tokens(
        client,
        &base,
        &session.client_id,
        secret,
        &session.tokens.refresh_token,
    )?;
    store.save(session)?;
    Ok(())
}

pub(crate) fn get_collection<T: DeserializeOwned>(
    client: &Client,
    store: &Store,
    session: &mut Session,
    client_secret: Option<&str>,
    mut url: Url,
) -> Result<Vec<T>> {
    let base = Url::parse(&session.base_url)?;
    let mut seen = HashSet::new();
    let mut results = Vec::new();
    loop {
        ensure!(
            url.origin() == base.origin()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
                && url.path().starts_with("/api/v1/"),
            "Refusing an Exact pagination URL outside this account's API origin"
        );
        ensure!(
            seen.insert(url.to_string()),
            "Exact returned a pagination loop"
        );
        access_token(client, store, session, client_secret)?;
        let response = client
            .get(url.clone())
            .bearer_auth(&session.tokens.access_token)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
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
            .map_err(|_| anyhow::anyhow!("Invalid Exact API response"))?;
        results.extend(page.d.results);
        match page.d.next {
            Some(next) => url = url.join(&next).context("Invalid Exact pagination URL")?,
            None => return Ok(results),
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
struct Page<T> {
    results: Vec<T>,
    #[serde(rename = "__next")]
    next: Option<String>,
}
