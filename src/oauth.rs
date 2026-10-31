use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use reqwest::Client;
use reqwest::Url;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, pem::PemObject};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::time::{Instant, timeout_at};
use tokio_rustls::TlsAcceptor;

use crate::store::Tokens;

pub fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

pub fn http_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("exact-mcp/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("Cannot initialize HTTPS client")
}

pub fn validate_base_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("Invalid Exact base URL")?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "Exact base URL must be an HTTPS origin, such as https://start.exactonline.nl"
    );
    Ok(url)
}

pub fn validate_redirect_uri(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("Invalid redirect URI")?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some_and(|host| {
                matches!(host, "localhost" | "127.0.0.1") || host.ends_with(".localhost")
            })
            && url.port().is_some_and(|port| port != 0)
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Redirect URI must use HTTPS on localhost, a .localhost hostname, or 127.0.0.1 with an explicit non-default port and no query/fragment"
    );
    Ok(url)
}

pub async fn load_tls_config(cert_file: &Path, key_file: &Path) -> Result<Arc<ServerConfig>> {
    let certificate_pem = tokio::fs::read(cert_file)
        .await
        .context("Cannot open TLS certificate file")?;
    let certificates = CertificateDer::pem_slice_iter(&certificate_pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("Cannot parse TLS certificate PEM")?;
    let key_pem = tokio::fs::read(key_file)
        .await
        .context("Cannot open TLS private key file")?;
    let key = PrivateKeyDer::from_pem_slice(&key_pem).context("Cannot read TLS private key PEM")?;
    tls_config(certificates, key)
}

pub fn self_signed_tls_config(redirect: &Url) -> Result<Arc<ServerConfig>> {
    let host = redirect.host_str().context("Missing callback hostname")?;
    let generated = rcgen::generate_simple_self_signed(vec![host.to_owned()])
        .context("Cannot generate self-signed callback certificate")?;
    tls_config(
        vec![generated.cert.der().clone()],
        PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der()).into(),
    )
}

fn tls_config(
    certificates: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<ServerConfig>> {
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(certificates, key)
            .context("Invalid TLS certificate/key pair")?;
    Ok(Arc::new(config))
}

pub fn random_state() -> Result<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("Cannot generate OAuth state"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn authorization_url(
    base: &Url,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
) -> Result<Url> {
    let mut url = base.join("/api/oauth2/auth")?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("state", state);
    Ok(url)
}

pub async fn listen(redirect: &Url) -> Result<TcpListener> {
    let port = redirect.port().context("Missing callback port")?;
    TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .with_context(|| {
            format!("Cannot listen on localhost:{port}; choose a free registered redirect URI")
        })
}

pub async fn wait_for_code(
    listener: &TcpListener,
    tls_config: &Arc<ServerConfig>,
    redirect: &Url,
    state: &str,
    timeout: Duration,
) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let acceptor = TlsAcceptor::from(Arc::clone(tls_config));
    while let Ok(connection) = timeout_at(deadline, listener.accept()).await {
        let (stream, _) = connection.context("Cannot accept OAuth callback")?;
        let connection_deadline = deadline.min(Instant::now() + Duration::from_secs(2));
        let received = timeout_at(connection_deadline, async {
            let mut stream = acceptor.accept(stream).await?;
            let callback = read_request_target(&mut stream)
                .await
                .map(|target| parse_callback(&target, redirect, state))
                .unwrap_or(Callback::Invalid);
            Ok::<_, std::io::Error>((stream, callback))
        })
        .await;
        let Ok(Ok((mut stream, callback))) = received else {
            continue;
        };
        let (status, body) = match &callback {
            Callback::Code(_) => (
                "200 OK",
                "Authorization received. Return to the terminal to check the login result.",
            ),
            Callback::Denied => (
                "403 Forbidden",
                "Authorization was denied. Return to the terminal.",
            ),
            Callback::Unrelated => ("404 Not Found", "Not found"),
            Callback::Invalid => ("400 Bad Request", "Invalid OAuth callback"),
        };
        let _ = timeout_at(
            deadline.min(Instant::now() + Duration::from_secs(2)),
            async {
                reply(&mut stream, status, body).await?;
                stream.shutdown().await?;
                Ok::<_, anyhow::Error>(())
            },
        )
        .await;
        match callback {
            Callback::Code(code) => return Ok(code),
            Callback::Denied => bail!("Exact authorization was denied; no session was changed"),
            Callback::Unrelated | Callback::Invalid => {}
        }
    }
    bail!("Timed out waiting for Exact authorization; run `exact-mcp auth login` again")
}

async fn read_request_target(stream: impl AsyncRead + Unpin) -> Result<String> {
    let mut reader = BufReader::new(stream.take(8192));
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let parts: Vec<_> = line.split_whitespace().collect();
    ensure!(
        parts.len() == 3 && parts[0] == "GET" && matches!(parts[2], "HTTP/1.0" | "HTTP/1.1"),
        "Invalid callback request"
    );
    let target = parts[1].to_owned();
    loop {
        line.clear();
        ensure!(
            reader.read_line(&mut line).await? > 0,
            "Incomplete or oversized callback request"
        );
        if line == "\r\n" {
            return Ok(target);
        }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum Callback {
    Code(String),
    Denied,
    Unrelated,
    Invalid,
}

pub(crate) fn parse_callback(target: &str, redirect: &Url, state: &str) -> Callback {
    if !target.starts_with('/') || target.starts_with("//") {
        return Callback::Invalid;
    }
    let Ok(url) = redirect.join(target) else {
        return Callback::Invalid;
    };
    if url.origin() != redirect.origin() || url.fragment().is_some() {
        return Callback::Invalid;
    }
    if url.path() != redirect.path() {
        return Callback::Unrelated;
    }
    let mut parameters = HashMap::new();
    for (key, value) in url.query_pairs() {
        if parameters
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return Callback::Invalid;
        }
    }
    if parameters.get("state").map(String::as_str) != Some(state) {
        return Callback::Invalid;
    }
    match (parameters.get("code"), parameters.get("error")) {
        (Some(code), None) if !code.is_empty() => Callback::Code(code.clone()),
        (None, Some(_)) => Callback::Denied,
        _ => Callback::Invalid,
    }
}

async fn reply(stream: &mut (impl AsyncWrite + Unpin), status: &str, body: &str) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

pub async fn exchange_code(
    client: &Client,
    base: &Url,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
    code: &str,
) -> Result<Tokens> {
    request_tokens(
        client,
        base,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("redirect_uri", redirect_uri),
            ("code", code),
        ],
    )
    .await
}

pub async fn refresh_tokens(
    client: &Client,
    base: &Url,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<Tokens> {
    request_tokens(
        client,
        base,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("refresh_token", refresh_token),
        ],
    )
    .await
}

async fn request_tokens(client: &Client, base: &Url, form: &[(&str, &str)]) -> Result<Tokens> {
    let started_at = now()?;
    let response = client
        .post(base.join("/api/oauth2/token")?)
        .form(form)
        .send().await
        .context("Token request failed; not retried because Exact may have consumed the code/refresh token")?;
    let status = response.status();
    if !status.is_success() {
        bail!(
            "Exact token request failed (HTTP {status}); verify app credentials or run `exact-mcp auth login` again"
        );
    }
    let response: TokenResponse = response
        .json()
        .await
        .map_err(|_| anyhow::anyhow!("Invalid token response; a new login may be required"))?;
    ensure!(
        response.token_type.eq_ignore_ascii_case("bearer")
            && !response.access_token.is_empty()
            && !response.refresh_token.is_empty(),
        "Incomplete or unsupported token response; a new login may be required"
    );
    let expires_in = match response.expires_in {
        ExpiresIn::Number(seconds) => seconds,
        ExpiresIn::Text(seconds) => seconds
            .parse::<u64>()
            .map_err(|_| anyhow::anyhow!("Invalid token lifetime; a new login may be required"))?,
    };
    ensure!(
        expires_in > 10,
        "Token lifetime is too short; a new login may be required"
    );
    Ok(Tokens {
        access_token: response.access_token,
        refresh_token: response.refresh_token,
        expires_at: started_at
            .checked_add(expires_in)
            .context("Invalid token lifetime")?,
    })
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    token_type: String,
    expires_in: ExpiresIn,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ExpiresIn {
    Number(u64),
    Text(String),
}
