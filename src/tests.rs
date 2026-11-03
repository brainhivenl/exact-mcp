use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use reqwest::Client;
use reqwest::Url;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, StreamOwned};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::exact;
use crate::oauth::{self, Callback};
use crate::store::{Administration, Session, Store, Tokens};

fn tempdir() -> std::io::Result<TempDir> {
    let directory = tempfile::tempdir()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(directory)
}

async fn tls_configs() -> (Arc<ServerConfig>, Arc<ClientConfig>) {
    let directory = tempdir().unwrap();
    let generated = rcgen::generate_simple_self_signed(vec!["exact-mcp.localhost".into()]).unwrap();
    let cert_path = directory.path().join("cert.pem");
    let key_path = directory.path().join("key.pem");
    std::fs::write(&cert_path, generated.cert.pem()).unwrap();
    std::fs::write(&key_path, generated.signing_key.serialize_pem()).unwrap();
    let server = oauth::load_tls_config(&cert_path, &key_path).await.unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(generated.cert.der().clone()).unwrap();
    let client =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    (server, Arc::new(client))
}

fn tls_stream(
    address: SocketAddr,
    config: Arc<ClientConfig>,
) -> StreamOwned<ClientConnection, TcpStream> {
    let socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let connection =
        ClientConnection::new(config, ServerName::try_from("exact-mcp.localhost").unwrap())
            .unwrap();
    StreamOwned::new(connection, socket)
}

struct Request {
    target: String,
    headers: HashMap<String, String>,
    body: String,
}

#[derive(Clone)]
struct Response {
    status: &'static str,
    body: String,
    location: Option<String>,
}

impl Response {
    fn json(value: Value) -> Self {
        Self {
            status: "200 OK",
            body: value.to_string(),
            location: None,
        }
    }
}

struct Server {
    base: Url,
    handle: JoinHandle<Vec<Request>>,
}

impl Server {
    fn new(responses: Vec<Response>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "Mock request timed out");
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("Mock listener failed: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(&stream);
                let mut target = String::new();
                reader.read_line(&mut target).unwrap();
                let mut headers = HashMap::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap();
                    headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
                }
                let length = headers
                    .get("content-length")
                    .map(|value| value.parse().unwrap())
                    .unwrap_or(0);
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                requests.push(Request {
                    target,
                    headers,
                    body: String::from_utf8(body).unwrap(),
                });
                let location = response
                    .location
                    .map(|url| format!("Location: {url}\r\n"))
                    .unwrap_or_default();
                write!(
                    stream,
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{location}Connection: close\r\n\r\n{}",
                    response.status,
                    response.body.len(),
                    response.body
                ).unwrap();
            }
            requests
        });
        Self { base, handle }
    }

    fn finish(self) -> Vec<Request> {
        self.handle.join().unwrap()
    }
}

fn client() -> Client {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
}

fn session(base: &Url, expired: bool) -> Session {
    Session {
        base_url: base.to_string(),
        client_id: "test-client".into(),
        redirect_uri: "https://exact-mcp.localhost:8400/callback".into(),
        tokens: Tokens {
            access_token: "old-access".into(),
            refresh_token: "old-refresh".into(),
            expires_at: if expired {
                0
            } else {
                oauth::now().unwrap() + 3600
            },
        },
        administration: Some(Administration {
            code: 42,
            description: "Selected company".into(),
        }),
    }
}

fn tokens_response(expires_in: Value) -> Response {
    Response::json(json!({
        "access_token": "new-access",
        "refresh_token": "new-refresh",
        "token_type": "bearer",
        "expires_in": expires_in
    }))
}

fn me_response() -> Response {
    Response::json(json!({"d": {"results": [{"CurrentDivision": 7}]}}))
}

fn divisions_response() -> Response {
    Response::json(json!({"d": {"results": [
        {"Code": 7, "Description": "First company"},
        {"Code": 42, "Description": "Selected company"}
    ]}}))
}

#[test]
fn validates_https_origins_and_loopback_redirects() {
    for value in [
        "https://start.exactonline.nl",
        "https://start.exactonline.be/",
    ] {
        assert!(oauth::validate_base_url(value).is_ok(), "{value}");
    }
    for value in [
        "http://start.exactonline.nl",
        "https://user:secret@start.exactonline.nl",
        "https://start.exactonline.nl/path",
        "https://start.exactonline.nl?x=1",
        "https://start.exactonline.nl#fragment",
    ] {
        assert!(oauth::validate_base_url(value).is_err(), "{value}");
    }
    for value in [
        "https://exact-mcp.localhost:8400/callback",
        "https://localhost:8400/callback",
        "https://127.0.0.1:9000/oauth",
    ] {
        assert!(oauth::validate_redirect_uri(value).is_ok(), "{value}");
    }
    for value in [
        "http://localhost:8400/callback",
        "http://exact-mcp.localhost:8400/callback",
        "https://example.com:8400/callback",
        "https://localhost.example.com:8400/callback",
        "https://notlocalhost:8400/callback",
        "https://0.0.0.0:8400/callback",
        "https://localhost/callback",
        "https://localhost:0/callback",
        "https://localhost:8400/callback?state=x",
        "https://user@localhost:8400/callback",
        "https://localhost:8400/callback#fragment",
    ] {
        assert!(oauth::validate_redirect_uri(value).is_err(), "{value}");
    }
}

#[test]
fn authorization_url_encodes_parameters_and_uses_random_state() {
    let state = oauth::random_state().unwrap();
    assert_eq!(state.len(), 64);
    assert_ne!(state, oauth::random_state().unwrap());
    let redirect = "https://exact-mcp.localhost:8400/callback";
    let url = oauth::authorization_url(
        &Url::parse("https://start.exactonline.nl").unwrap(),
        "client+id",
        redirect,
        &state,
    )
    .unwrap();
    assert_eq!(url.path(), "/api/oauth2/auth");
    let parameters: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(parameters["client_id"], "client+id");
    assert_eq!(parameters["response_type"], "code");
    assert_eq!(parameters["redirect_uri"], redirect);
    assert_eq!(parameters["state"], state);
    assert!(!parameters.contains_key("client_secret"));
}

#[test]
fn callback_checks_state_path_errors_and_duplicate_parameters() {
    let redirect = Url::parse("https://exact-mcp.localhost:8400/callback").unwrap();
    let cases = [
        (
            "/callback?state=expected&code=a%2Bb",
            Callback::Code("a+b".into()),
        ),
        ("/favicon.ico", Callback::Unrelated),
        ("/callback?code=x", Callback::Invalid),
        ("/callback?state=wrong&code=x", Callback::Invalid),
        ("/callback?state=expected&code=", Callback::Invalid),
        (
            "/callback?state=expected&state=wrong&code=x",
            Callback::Invalid,
        ),
        ("/callback?state=expected&code=x&code=y", Callback::Invalid),
        (
            "/callback?state=expected&code=x&error=access_denied",
            Callback::Invalid,
        ),
        (
            "/callback?state=expected&error=access_denied",
            Callback::Denied,
        ),
        (
            "/callback?state=expected&error=anything&error_description=secret",
            Callback::Denied,
        ),
        (
            "//example.com/callback?state=expected&code=x",
            Callback::Invalid,
        ),
        (
            "https://example.com/callback?state=expected&code=x",
            Callback::Invalid,
        ),
        (
            "/callback?state=expected&code=x#fragment",
            Callback::Invalid,
        ),
    ];
    for (target, expected) in cases {
        assert_eq!(
            oauth::parse_callback(target, &redirect, "expected"),
            expected,
            "{target}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_callback_ignores_noise_and_returns_only_a_valid_code() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let address = listener.local_addr().unwrap();
    let redirect = Url::parse(&format!(
        "https://exact-mcp.localhost:{}/callback",
        address.port()
    ))
    .unwrap();
    let (server_config, client_config) = tls_configs().await;
    let handle = tokio::spawn(async move {
        oauth::wait_for_code(
            &listener,
            &server_config,
            &redirect,
            "expected",
            Duration::from_secs(3),
        )
        .await
    });
    let mut plaintext = TcpStream::connect(address).unwrap();
    plaintext
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    plaintext.write_all(b"GET /callback?state=expected&code=plaintext HTTP/1.1\r\nHost: exact-mcp.localhost\r\n\r\n").unwrap();
    let mut rejected = Vec::new();
    let _ = plaintext.read_to_end(&mut rejected);
    assert!(!String::from_utf8_lossy(&rejected).contains("200 OK"));
    drop(plaintext);
    for (target, status) in [
        ("/favicon.ico", "404"),
        ("/callback?state=wrong&code=bad", "400"),
        ("/callback?state=expected&code=good", "200"),
    ] {
        let mut stream = tls_stream(address, Arc::clone(&client_config));
        stream
            .sock
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write!(stream, "GET {target} HTTP/1.1\r\nHost: {address}\r\n\r\n").unwrap();
        stream.flush().unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status}")),
            "{response}"
        );
        assert!(!response.contains("code=good"));
    }
    assert_eq!(handle.await.unwrap().unwrap(), "good");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_callback_waits_for_complete_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let address = listener.local_addr().unwrap();
    let redirect = Url::parse(&format!(
        "https://exact-mcp.localhost:{}/callback",
        address.port()
    ))
    .unwrap();
    let (server_config, client_config) = tls_configs().await;
    let handle = tokio::spawn(async move {
        oauth::wait_for_code(
            &listener,
            &server_config,
            &redirect,
            "expected",
            Duration::from_secs(3),
        )
        .await
    });
    let mut stream = tls_stream(address, Arc::clone(&client_config));
    stream
        .sock
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    stream
        .write_all(b"GET /callback?state=expected&code=good HTTP/1.1\r\nHost: localhost\r\n")
        .unwrap();
    stream.flush().unwrap();
    let error = stream.read(&mut [0; 1]).unwrap_err();
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    stream.write_all(b"\r\n").unwrap();
    stream.flush().unwrap();
    stream
        .sock
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(handle.await.unwrap().unwrap(), "good");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_signed_callback_works_with_an_explicit_local_certificate_exception() {
    let redirect =
        oauth::validate_redirect_uri("https://exact-mcp.localhost:8400/callback").unwrap();
    let tls_config = oauth::self_signed_tls_config(&redirect).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let address = listener.local_addr().unwrap();
    let redirect = Url::parse(&format!(
        "https://exact-mcp.localhost:{}/callback",
        address.port()
    ))
    .unwrap();
    let callback = redirect.join("?state=expected&code=test-code").unwrap();
    let handle = tokio::spawn(async move {
        oauth::wait_for_code(
            &listener,
            &tls_config,
            &redirect,
            "expected",
            Duration::from_secs(5),
        )
        .await
    });
    let strict = Client::builder()
        .no_proxy()
        .resolve("exact-mcp.localhost", address)
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    assert!(strict.get(callback.clone()).send().await.is_err());
    let local_exception = Client::builder()
        .no_proxy()
        .resolve("exact-mcp.localhost", address)
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let response = local_exception.get(callback).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(handle.await.unwrap().unwrap(), "test-code");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_config_rejects_missing_malformed_and_mismatched_keys() {
    let directory = tempdir().unwrap();
    let cert_path = directory.path().join("cert.pem");
    let key_path = directory.path().join("key.pem");
    assert!(oauth::load_tls_config(&cert_path, &key_path).await.is_err());
    std::fs::write(&cert_path, "not a certificate").unwrap();
    std::fs::write(&key_path, "private-key-content").unwrap();
    let error = oauth::load_tls_config(&cert_path, &key_path)
        .await
        .err()
        .unwrap();
    assert!(!format!("{error:#}").contains("private-key-content"));
    let first = rcgen::generate_simple_self_signed(vec!["exact-mcp.localhost".into()]).unwrap();
    let second = rcgen::generate_simple_self_signed(vec!["exact-mcp.localhost".into()]).unwrap();
    std::fs::write(&cert_path, first.cert.pem()).unwrap();
    std::fs::write(&key_path, second.signing_key.serialize_pem()).unwrap();
    assert!(oauth::load_tls_config(&cert_path, &key_path).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_callback_handles_authorization_denial() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let address = listener.local_addr().unwrap();
    let redirect = Url::parse(&format!(
        "https://exact-mcp.localhost:{}/callback",
        address.port()
    ))
    .unwrap();
    let (server_config, client_config) = tls_configs().await;
    let handle = tokio::spawn(async move {
        oauth::wait_for_code(
            &listener,
            &server_config,
            &redirect,
            "expected",
            Duration::from_secs(3),
        )
        .await
    });
    let mut stream = tls_stream(address, client_config);
    stream.write_all(b"GET /callback?state=expected&error=access_denied HTTP/1.1\r\nHost: exact-mcp.localhost\r\n\r\n").unwrap();
    stream.flush().unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
    assert!(
        handle
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("denied")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn callback_times_out_without_requests() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let redirect = Url::parse("https://exact-mcp.localhost:8400/callback").unwrap();
    let (server_config, _) = tls_configs().await;
    let error = oauth::wait_for_code(
        &listener,
        &server_config,
        &redirect,
        "state",
        Duration::from_millis(25),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("Timed out"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_exchange_sends_form_and_accepts_numeric_or_string_expiry() {
    for expires in [json!(600), json!("600")] {
        let server = Server::new(vec![tokens_response(expires)]);
        let started = oauth::now().unwrap();
        let tokens = oauth::exchange_code(
            &client(),
            &server.base,
            "client+id",
            "client&secret",
            "https://exact-mcp.localhost:8400/callback",
            "code+value",
        )
        .await
        .unwrap();
        assert_eq!(tokens.access_token, "new-access");
        assert_eq!(tokens.refresh_token, "new-refresh");
        assert!(tokens.expires_at >= started + 600);
        let requests = server.finish();
        assert!(requests[0].target.starts_with("POST /api/oauth2/token "));
        let form: HashMap<_, _> =
            reqwest::Url::parse(&format!("http://localhost/?{}", requests[0].body))
                .unwrap()
                .query_pairs()
                .into_owned()
                .collect();
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["client_id"], "client+id");
        assert_eq!(form["client_secret"], "client&secret");
        assert_eq!(form["code"], "code+value");
        assert_eq!(
            form["redirect_uri"],
            "https://exact-mcp.localhost:8400/callback"
        );
        assert!(
            requests[0].headers["content-type"].starts_with("application/x-www-form-urlencoded")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_token_responses_and_server_errors_do_not_leak_secrets() {
    let responses = [
        Response {
            status: "400 Bad Request",
            body: "client-secret access-token".into(),
            location: None,
        },
        Response::json(json!({"access_token": "access-token", "expires_in": 600})),
        Response::json(
            json!({"access_token": "access-token", "refresh_token": "refresh-token", "token_type": "basic", "expires_in": 600}),
        ),
        tokens_response(json!("client-secret")),
        tokens_response(json!(0)),
    ];
    for response in responses {
        let server = Server::new(vec![response]);
        let error = oauth::exchange_code(
            &client(),
            &server.base,
            "id",
            "client-secret",
            "redirect",
            "code",
        )
        .await;
        let error = format!("{:#}", error.err().expect("Expected token failure"));
        for secret in ["client-secret", "access-token", "refresh-token"] {
            assert!(!error.contains(secret), "{error}");
        }
        assert_eq!(server.finish().len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_round_trips_and_atomically_replaces_tokens_and_selection() {
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    assert!(store.load().await.unwrap().is_none());
    let mut session = session(&Url::parse("https://start.exactonline.nl").unwrap(), false);
    store.save(&session).await.unwrap();
    session.tokens.access_token = "replacement".into();
    store.save(&session).await.unwrap();
    let stored = store.require_session().await.unwrap();
    assert_eq!(stored.tokens.access_token, "replacement");
    assert_eq!(stored.administration.unwrap().code, 42);
    let raw = std::fs::read_to_string(directory.path().join("session.json")).unwrap();
    assert!(!raw.contains("old-access"));
    assert!(!raw.contains("client-secret"));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(directory.path().join("session.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_rejects_public_permissions_and_symbolic_links() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = tempdir().unwrap();
    let path = root.path().join("private");
    let store = Store::open(&path).await.unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let session = session(&Url::parse("https://start.exactonline.nl").unwrap(), false);
    store.save(&session).await.unwrap();
    let file = path.join("session.json");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.load().await.is_err());
    std::fs::remove_file(&file).unwrap();
    symlink(root.path().join("elsewhere"), &file).unwrap();
    assert!(store.load().await.is_err());
    drop(store);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Store::open(&path).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listing_handles_pagination_without_selecting_the_current_division() {
    let server = Server::new(vec![
        me_response(),
        Response::json(json!({"d": {
            "results": [{"Code": 7, "Description": "First company"}],
            "__next": "/api/v1/7/system/Divisions?$skiptoken=page2"
        }})),
        Response::json(
            json!({"d": {"results": [{"Code": 42, "Description": "Selected company"}]}}),
        ),
    ]);
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    let mut session = session(&server.base, false);
    session.administration = None;
    store.save(&session).await.unwrap();
    drop(store);
    let client = exact::ExactClient::new(directory.path().into(), client(), None)
        .await
        .unwrap();
    let available = client.administrations().await.unwrap();
    assert_eq!(
        available.iter().map(|item| item.code).collect::<Vec<_>>(),
        [7, 42]
    );
    assert!(client.session().await.unwrap().administration.is_none());
    let requests = server.finish();
    assert!(
        requests[0]
            .target
            .contains("/api/v1/current/Me?$select=CurrentDivision")
    );
    assert!(
        requests[1]
            .target
            .contains("/api/v1/7/system/Divisions?$select=Code,Description")
    );
    assert!(requests[2].target.contains("$skiptoken=page2"));
    assert!(
        requests
            .iter()
            .all(|request| request.headers["authorization"] == "Bearer old-access")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selection_requires_an_accessible_code_and_preserves_previous_selection_on_failure() {
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    let session = session(&Url::parse("https://start.exactonline.nl").unwrap(), false);
    store.save(&session).await.unwrap();
    drop(store);
    let client = exact::ExactClient::new(directory.path().into(), client(), None)
        .await
        .unwrap();
    let available = vec![Administration {
        code: 7,
        description: "First company".into(),
    }];
    assert!(client.select_administration(&available, 999).await.is_err());
    assert_eq!(
        client.session().await.unwrap().administration.unwrap().code,
        42
    );
    let selected = client.select_administration(&available, 7).await.unwrap();
    assert_eq!(selected.code, 7);
    assert_eq!(
        client.session().await.unwrap().administration.unwrap().code,
        7
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_rotates_and_persists_tokens_before_api_calls() {
    let server = Server::new(vec![
        tokens_response(json!(600)),
        me_response(),
        divisions_response(),
    ]);
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    store.save(&session(&server.base, true)).await.unwrap();
    drop(store);
    let client = exact::ExactClient::new(
        directory.path().into(),
        client(),
        Some("client-secret".into()),
    )
    .await
    .unwrap();
    client.administrations().await.unwrap();
    let stored = client.session().await.unwrap();
    assert_eq!(stored.tokens.access_token, "new-access");
    assert_eq!(stored.tokens.refresh_token, "new-refresh");
    assert_eq!(stored.administration.unwrap().code, 42);
    let requests = server.finish();
    assert!(requests[0].target.starts_with("POST /api/oauth2/token "));
    assert!(requests[0].body.contains("refresh_token=old-refresh"));
    assert!(requests[0].body.contains("grant_type=refresh_token"));
    assert!(
        requests[1..]
            .iter()
            .all(|request| request.headers["authorization"] == "Bearer new-access")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_commands_reload_after_locking_and_refresh_only_once() {
    let server = Server::new(vec![tokens_response(json!(600))]);
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    store.save(&session(&server.base, true)).await.unwrap();
    drop(store);
    let first = exact::ExactClient::new(
        directory.path().into(),
        client(),
        Some("client-secret".into()),
    )
    .await
    .unwrap();
    let second = exact::ExactClient::new(
        directory.path().into(),
        client(),
        Some("client-secret".into()),
    )
    .await
    .unwrap();
    let (first, second) = tokio::join!(first.access_token(), second.access_token());
    assert_eq!(first.unwrap(), "new-access");
    assert_eq!(second.unwrap(), "new-access");
    let requests = server.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].target.starts_with("POST"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_token_without_secret_fails_without_contacting_exact() {
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    store
        .save(&session(&Url::parse("http://127.0.0.1:1").unwrap(), true))
        .await
        .unwrap();
    drop(store);
    let client = exact::ExactClient::new(directory.path().into(), client(), None)
        .await
        .unwrap();
    let error = client.administrations().await.unwrap_err();
    assert!(error.to_string().contains("EXACT_CLIENT_SECRET"));
    assert_eq!(
        client.session().await.unwrap().tokens.refresh_token,
        "old-refresh"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_refresh_does_not_overwrite_the_saved_session_or_retry() {
    let server = Server::new(vec![Response {
        status: "400 Bad Request",
        body: "old-refresh client-secret invalid_grant".into(),
        location: None,
    }]);
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    store.save(&session(&server.base, true)).await.unwrap();
    drop(store);
    let client = exact::ExactClient::new(
        directory.path().into(),
        client(),
        Some("client-secret".into()),
    )
    .await
    .unwrap();
    let error = client.administrations().await.unwrap_err();
    assert!(!format!("{error:#}").contains("old-refresh"));
    assert_eq!(
        client.session().await.unwrap().tokens.refresh_token,
        "old-refresh"
    );
    assert_eq!(server.finish().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pagination_rejects_other_origins_non_api_paths_and_cycles() {
    for next in [
        "http://127.0.0.1:1/api/v1/stolen",
        "/api/v1/999/system/Divisions",
        "/outside-api",
        "/api/v1/current/Me?$select=CurrentDivision",
    ] {
        let server = Server::new(vec![Response::json(
            json!({"d": {"results": [{"CurrentDivision": 7}], "__next": next}}),
        )]);
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).await.unwrap();
        store.save(&session(&server.base, false)).await.unwrap();
        drop(store);
        let client = exact::ExactClient::new(directory.path().into(), client(), None)
            .await
            .unwrap();
        let error = client.administrations().await.unwrap_err();
        assert!(error.to_string().contains("pagination"), "{error}");
        assert_eq!(server.finish().len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_errors_and_redirects_are_not_retried_or_followed() {
    for status in ["401 Unauthorized", "403 Forbidden", "302 Found"] {
        let server = Server::new(vec![Response {
            status,
            body: "old-access financial-data".into(),
            location: Some("http://127.0.0.1:1/api/v1/stolen".into()),
        }]);
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).await.unwrap();
        store.save(&session(&server.base, false)).await.unwrap();
        drop(store);
        let client =
            exact::ExactClient::new(directory.path().into(), client(), Some("secret".into()))
                .await
                .unwrap();
        let error = client.administrations().await.unwrap_err();
        assert!(!format!("{error:#}").contains("old-access"));
        assert!(!format!("{error:#}").contains("financial-data"));
        assert_eq!(server.finish().len(), 1);
    }
}

const COMPANY_ID: &str = "11111111-1111-1111-1111-111111111111";

fn company() -> Value {
    json!({
        "ID": COMPANY_ID, "Code": "1001", "Name": "Company X",
        "IsSupplier": true, "IsSales": true
    })
}

fn company_response() -> Response {
    Response::json(json!({"d": {"results": [company()]}}))
}

fn open_item(hid: i64, currency: &str, amount: Value) -> Value {
    json!({
        "HID": hid, "AccountId": COMPANY_ID, "Amount": amount,
        "AmountInTransit": 0, "CurrencyCode": currency,
        "DueDate": "/Date(946684800000)/", "InvoiceDate": "2000-01-01T00:00:00",
        "InvoiceNumber": 2026001, "EntryNumber": 42, "JournalCode": "70",
        "YourRef": "SUPPLIER-001", "Description": "Invoice"
    })
}

fn open_items_args() -> crate::financial::GetOpenItems {
    serde_json::from_value(json!({"account_id": COMPANY_ID, "side": "payable"})).unwrap()
}

async fn financial_backend(base: &Url) -> (TempDir, crate::financial::Backend) {
    let directory = tempdir().unwrap();
    {
        let store = Store::open(directory.path()).await.unwrap();
        store.save(&session(base, false)).await.unwrap();
    }
    let backend = crate::financial::Backend::new(directory.path().to_owned())
        .await
        .unwrap();
    (directory, backend)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn company_search_escapes_odata_and_reports_ambiguous_truncated_matches() {
    let server = Server::new(vec![Response::json(
        json!({"d": {"results": [company(), company(), company()]}}),
    )]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = backend
        .search_accounts(crate::financial::SearchAccounts {
            query: "  O'Neill & Sons  ".into(),
            limit: Some(2),
            ..Default::default()
        })
        .await
        .unwrap();
    let result = serde_json::to_value(result).unwrap();
    assert_eq!(result["accounts"].as_array().unwrap().len(), 2);
    assert_eq!(result["has_more"], true);
    assert_eq!(result["administration"]["Code"], 42);
    assert_eq!(result["accounts"][0]["id"], COMPANY_ID);
    assert_eq!(result["accounts"][0]["sales_allowed"], true);
    let requests = server.finish();
    let target = requests[0].target.split_whitespace().nth(1).unwrap();
    let url = server_url(target);
    let parameters: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(url.path(), "/api/v1/42/crm/Accounts");
    assert_eq!(parameters["$filter"], "substringof('O''Neill & Sons',Name)");
    assert_eq!(parameters["$orderby"], "Name,Code,ID");
    assert_eq!(result["next_offset"], 2);
    assert!(requests[0].target.starts_with("GET "));
}

fn server_url(target: &str) -> Url {
    Url::parse("http://localhost")
        .unwrap()
        .join(target)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outstanding_items_sum_decimal_amounts_and_keep_currencies_and_transit_separate() {
    let mut first = open_item(1, "EUR", json!(0.1));
    first["AmountInTransit"] = json!(0.05);
    let mut second = open_item(2, "EUR", json!(0.2));
    second["DueDate"] = json!(chrono::Local::now().date_naive().to_string());
    second["AmountInTransit"] = Value::Null;
    let mut credit = open_item(3, "EUR", json!(-0.1));
    credit["HID"] = json!("3");
    credit["DueDate"] = Value::Null;
    let server = Server::new(vec![
        company_response(),
        Response::json(json!({"d": {"results": [first, second], "__next": "?page=2"}})),
        Response::json(json!({"d": {"results": [credit, open_item(4, "USD", json!(10))]}})),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = backend
        .get_open_items(crate::financial::GetOpenItems {
            account_id: COMPANY_ID.into(),
            side: crate::financial::Side::Payable,
            ..open_items_args()
        })
        .await
        .unwrap();
    let result = serde_json::to_value(result).unwrap();
    assert_eq!(result["items"].as_array().unwrap().len(), 4);
    let euros = &result["totals_by_currency"]["EUR"];
    assert_eq!(euros["net_open_amount"], "0.2");
    assert_eq!(euros["positive_open_amount"], "0.3");
    assert_eq!(euros["negative_open_amount"], "-0.1");
    assert_eq!(euros["overdue_net_amount"], "0.1");
    assert_eq!(euros["known_in_transit_amount"], "0.05");
    assert_eq!(euros["items_with_unknown_in_transit"], 1);
    assert_eq!(euros["items_without_due_date"], 1);
    assert_eq!(result["totals_by_currency"]["USD"]["net_open_amount"], "10");
    assert_eq!(result["items"][0]["your_ref"], "SUPPLIER-001");
    assert_eq!(result["items"][1]["overdue"], false);
    assert_eq!(result["items"][2]["overdue"], Value::Null);
    assert_eq!(result["items"][1]["days_overdue"], 0);
    assert_eq!(result["items"][2]["days_overdue"], Value::Null);
    let today =
        chrono::NaiveDate::parse_from_str(result["as_of_date"].as_str().unwrap(), "%Y-%m-%d")
            .unwrap();
    assert_eq!(
        result["items"][0]["days_overdue"],
        today
            .signed_duration_since(chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap())
            .num_days()
    );
    assert_eq!(
        euros["aging_by_due_date"]["current"]["net_open_amount"],
        "0.2"
    );
    assert_eq!(
        euros["aging_by_due_date"]["overdue_over_90_days"]["net_open_amount"],
        "0.1"
    );
    assert_eq!(
        euros["aging_by_due_date"]["unknown_due_date"]["negative_open_amount"],
        "-0.1"
    );
    assert_eq!(result["side"], "payable");
    assert_eq!(result["amount_basis"], "exact_report_currency");
    assert!(result["retrieved_at"].as_str().is_some());
    let requests = server.finish();
    assert!(
        requests
            .iter()
            .all(|request| request.target.starts_with("GET "))
    );
    let target = requests[1].target.split_whitespace().nth(1).unwrap();
    let url = server_url(target);
    assert_eq!(
        url.path(),
        "/api/v1/42/read/financial/PayablesListByAccount"
    );
    let parameters: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(parameters["accountId"], format!("guid'{COMPANY_ID}'"));
    assert!(requests[2].target.contains("?page=2"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receivables_use_their_own_endpoint_and_return_a_complete_empty_report() {
    let server = Server::new(vec![
        company_response(),
        Response::json(json!({"d": {"results": []}})),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = backend
        .get_open_items(crate::financial::GetOpenItems {
            account_id: COMPANY_ID.into(),
            side: crate::financial::Side::Receivable,
            ..open_items_args()
        })
        .await
        .unwrap();
    let result = serde_json::to_value(result).unwrap();
    assert_eq!(result["side"], "receivable");
    assert_eq!(result["items"], json!([]));
    assert_eq!(result["totals_by_currency"], json!({}));
    assert_eq!(result["account"]["id"], COMPANY_ID);
    let requests = server.finish();
    assert!(
        requests[1]
            .target
            .contains("/42/read/financial/ReceivablesListByAccount?")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn financial_queries_reject_invalid_inputs_before_network_access() {
    let (_directory, backend) = financial_backend(&Url::parse("http://127.0.0.1:1").unwrap()).await;
    for (query, limit) in [
        ("".to_string(), None),
        (" ".to_string(), None),
        ("x".repeat(101), None),
        ("x".into(), Some(0)),
        ("x".into(), Some(51)),
    ] {
        let error = backend
            .search_accounts(crate::financial::SearchAccounts {
                query,
                limit,
                ..Default::default()
            })
            .await
            .err()
            .unwrap();
        assert!(!error.to_string().contains("request failed"));
    }
    let error = backend
        .get_open_items(crate::financial::GetOpenItems {
            account_id: "company name' or true".into(),
            side: crate::financial::Side::Payable,
            ..open_items_args()
        })
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("UUID"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn financial_server_requires_selection_and_rejects_scope_changes() {
    let directory = tempdir().unwrap();
    let mut session = session(&Url::parse("http://127.0.0.1:1").unwrap(), false);
    {
        let store = Store::open(directory.path()).await.unwrap();
        session.administration = None;
        store.save(&session).await.unwrap();
    }
    assert!(
        crate::financial::Backend::new(directory.path().to_owned())
            .await
            .is_err()
    );
    {
        let store = Store::open(directory.path()).await.unwrap();
        session.administration = Some(Administration {
            code: 42,
            description: "Selected company".into(),
        });
        store.save(&session).await.unwrap();
    }
    let backend = crate::financial::Backend::new(directory.path().to_owned())
        .await
        .unwrap();
    for change in ["division", "client", "origin", "login"] {
        let store = Store::open(directory.path()).await.unwrap();
        let mut changed = self::session(&Url::parse("http://127.0.0.1:1").unwrap(), false);
        match change {
            "division" => changed.administration.as_mut().unwrap().code = 99,
            "client" => changed.client_id = "another-client".into(),
            "origin" => changed.base_url = "http://127.0.0.1:2/".into(),
            "login" => changed.administration = None,
            _ => unreachable!(),
        }
        store.save(&changed).await.unwrap();
        drop(store);
        let error = backend
            .search_accounts(crate::financial::SearchAccounts {
                query: "X".into(),
                limit: None,
                ..Default::default()
            })
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("restart"));
        let error = backend
            .get_outstanding_balances(serde_json::from_value(json!({"side": "payable"})).unwrap())
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("restart"));
        let error = backend
            .get_invoice_details(invoice_args("payable"))
            .await
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("restart"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outstanding_items_reject_wrong_accounts_duplicates_and_missing_currencies() {
    let mut wrong_account = open_item(1, "EUR", json!(10));
    wrong_account["AccountId"] = json!("22222222-2222-2222-2222-222222222222");
    for records in [
        vec![wrong_account],
        vec![open_item(1, "EUR", json!(1)), open_item(1, "EUR", json!(1))],
        vec![open_item(1, "", json!(1))],
    ] {
        let server = Server::new(vec![
            company_response(),
            Response::json(json!({"d": {"results": records}})),
        ]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .get_open_items(crate::financial::GetOpenItems {
                    account_id: COMPANY_ID.into(),
                    side: crate::financial::Side::Payable,
                    ..open_items_args()
                })
                .await
                .is_err()
        );
        assert_eq!(server.finish().len(), 2);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_account_is_not_reported_as_having_zero_debt() {
    let server = Server::new(vec![Response::json(json!({"d": {"results": []}}))]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let error = backend
        .get_open_items(crate::financial::GetOpenItems {
            account_id: COMPANY_ID.into(),
            side: crate::financial::Side::Payable,
            ..open_items_args()
        })
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("Account not found"));
    assert_eq!(server.finish().len(), 1);
}

async fn read_async_request(stream: &mut tokio::net::TcpStream) -> Request {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let mut reader = tokio::io::BufReader::new(stream);
    let mut target = String::new();
    reader.read_line(&mut target).await.unwrap();
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).await.unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = headers
        .get("content-length")
        .map(|value| value.parse().unwrap())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.unwrap();
    Request {
        target,
        headers,
        body: String::from_utf8(body).unwrap(),
    }
}

async fn write_async_response(stream: &mut tokio::net::TcpStream, body: &str) {
    use tokio::io::AsyncWriteExt;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    stream.shutdown().await.unwrap();
}

async fn async_exact_client(base: &Url, expired: bool) -> (TempDir, exact::ExactClient) {
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    store.save(&session(base, expired)).await.unwrap();
    drop(store);
    let client = exact::ExactClient::new(
        directory.path().into(),
        client(),
        Some("client-secret".into()),
    )
    .await
    .unwrap();
    (directory, client)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_reads_overlap_and_do_not_hold_the_session_lock() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let (directory, client) = async_exact_client(&base, false).await;
    let (started, entered) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let first_request = read_async_request(&mut first).await;
        started.send(()).unwrap();
        let (mut second, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let second_request = read_async_request(&mut second).await;
        let body = "{\"d\":{\"results\":[]}}";
        write_async_response(&mut first, body).await;
        write_async_response(&mut second, body).await;
        [first_request, second_request]
    });
    let url = base.join("/api/v1/42/crm/Accounts").unwrap();
    let request_client = client.clone();
    let first_url = url.clone();
    let first =
        tokio::spawn(async move { request_client.get_collection::<Value>(first_url).await });
    entered.await.unwrap();
    let store = tokio::time::timeout(Duration::from_millis(500), Store::open(directory.path()))
        .await
        .unwrap()
        .unwrap();
    drop(store);
    let second = client.get_collection::<Value>(url).await.unwrap();
    assert!(second.is_empty());
    assert!(first.await.unwrap().unwrap().is_empty());
    assert!(
        server
            .await
            .unwrap()
            .iter()
            .all(|request| request.target.starts_with("GET"))
    );
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_an_api_read_drops_the_pending_http_request() {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let (_directory, client) = async_exact_client(&base, false).await;
    let (started, entered) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_async_request(&mut socket).await;
        started.send(()).unwrap();
        let read = tokio::time::timeout(Duration::from_secs(1), socket.read(&mut [0; 1]))
            .await
            .unwrap();
        assert!(
            matches!(read, Ok(0))
                || read
                    .err()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::ConnectionReset)
        );
    });
    let request_client = client.clone();
    let request = tokio::spawn(async move {
        request_client
            .get_collection::<Value>(base.join("/api/v1/42/crm/Accounts").unwrap())
            .await
    });
    entered.await.unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    server.await.unwrap();
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_query_still_persists_rotated_tokens_and_shutdown_drains_refresh() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let (directory, client) = async_exact_client(&base, true).await;
    let (started, entered) = tokio::sync::oneshot::channel();
    let (release, continue_refresh) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_async_request(&mut socket).await;
        assert!(request.target.starts_with("POST /api/oauth2/token"));
        started.send(()).unwrap();
        continue_refresh.await.unwrap();
        write_async_response(&mut socket, &tokens_response(json!(600)).body).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        request
    });
    let request_client = client.clone();
    let request = tokio::spawn(async move {
        request_client
            .get_collection::<Value>(base.join("/api/v1/42/crm/Accounts").unwrap())
            .await
    });
    entered.await.unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    let shutdown_client = client.clone();
    let mut shutdown = tokio::spawn(async move { shutdown_client.shutdown().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut shutdown)
            .await
            .is_err()
    );
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .unwrap()
        .unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    let saved = store.require_session().await.unwrap();
    assert_eq!(saved.tokens.access_token, "new-access");
    assert_eq!(saved.tokens.refresh_token, "new-refresh");
    assert_eq!(saved.administration.unwrap().code, 42);
    assert!(
        server
            .await
            .unwrap()
            .body
            .contains("refresh_token=old-refresh")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn lock_contention_yields_to_the_runtime_and_cancelled_waiters_do_not_hold_locks() {
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    let path = directory.path().to_owned();
    let waiter = tokio::spawn(async move { Store::open(&path).await });
    let mut waiter = waiter;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut waiter)
            .await
            .is_err()
    );
    waiter.abort();
    assert!(waiter.await.err().unwrap().is_cancelled());
    drop(store);
    let store = tokio::time::timeout(Duration::from_secs(1), Store::open(directory.path()))
        .await
        .unwrap()
        .unwrap();
    drop(store);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn administration_selection_preserves_tokens_rotated_since_client_startup() {
    let directory = tempdir().unwrap();
    let base = Url::parse("http://127.0.0.1:1").unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    store.save(&session(&base, false)).await.unwrap();
    drop(store);
    let client = exact::ExactClient::new(directory.path().into(), client(), None)
        .await
        .unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    let mut updated = store.require_session().await.unwrap();
    updated.tokens.refresh_token = "already-rotated".into();
    store.save(&updated).await.unwrap();
    drop(store);
    client
        .select_administration(
            &[Administration {
                code: 7,
                description: "Other administration".into(),
            }],
            7,
        )
        .await
        .unwrap();
    let stored = client.session().await.unwrap();
    assert_eq!(stored.tokens.refresh_token, "already-rotated");
    assert_eq!(stored.administration.unwrap().code, 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_prevents_a_new_refresh_from_starting() {
    let (_directory, client) =
        async_exact_client(&Url::parse("http://127.0.0.1:1").unwrap(), true).await;
    client.shutdown().await;
    let error = client.access_token().await.unwrap_err();
    assert!(error.to_string().contains("shutting down"));
    assert_eq!(
        client.session().await.unwrap().tokens.refresh_token,
        "old-refresh"
    );
    tokio::time::timeout(Duration::from_secs(1), client.shutdown())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_search_accepts_exacts_unwrapped_collection_response() {
    for rows in [vec![company()], vec![]] {
        let expected_count = rows.len();
        let server = Server::new(vec![Response::json(json!({"d": rows}))]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let result = backend
            .search_accounts(crate::financial::SearchAccounts {
                query: "Company".into(),
                limit: None,
                ..Default::default()
            })
            .await
            .unwrap();
        let result = serde_json::to_value(result).unwrap();
        assert_eq!(result["accounts"].as_array().unwrap().len(), expected_count);
        assert_eq!(server.finish().len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unwrapped_final_page_is_combined_with_wrapped_paginated_rows() {
    let server = Server::new(vec![
        company_response(),
        Response::json(
            json!({"d": {"results": [open_item(1, "EUR", json!(1))], "__next": "?page=2"}}),
        ),
        Response::json(json!({"d": [open_item(2, "EUR", json!(2))]})),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = backend
        .get_open_items(crate::financial::GetOpenItems {
            account_id: COMPANY_ID.into(),
            side: crate::financial::Side::Payable,
            ..open_items_args()
        })
        .await
        .unwrap();
    let result = serde_json::to_value(result).unwrap();
    assert_eq!(result["items"].as_array().unwrap().len(), 2);
    assert_eq!(result["totals_by_currency"]["EUR"]["net_open_amount"], "3");
    assert_eq!(server.finish().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_search_supports_exact_codes_and_uuids() {
    for (field, query, filter) in [
        ("code", "  A'1001  ", "trim(Code) eq 'A''1001'".to_owned()),
        ("code", "1001", "trim(Code) eq '1001'".to_owned()),
        ("id", COMPANY_ID, format!("ID eq guid'{COMPANY_ID}'")),
    ] {
        let code = if field == "code" {
            query.trim()
        } else {
            "1001"
        };
        let mut account = company();
        account["Code"] = json!(format!("{code:>18}"));
        let server = Server::new(vec![Response::json(json!({"d": [account]}))]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let args = serde_json::from_value(json!({"query": query, "match_by": field})).unwrap();
        let result = serde_json::to_value(backend.search_accounts(args).await.unwrap()).unwrap();
        assert_eq!(result["match_by"], field);
        assert_eq!(result["accounts"][0]["code"], format!("{code:>18}"));
        assert_eq!(result["offset"], 0);
        assert_eq!(result["has_more"], false);
        assert_eq!(result["next_offset"], Value::Null);
        let requests = server.finish();
        let url = server_url(requests[0].target.split_whitespace().nth(1).unwrap());
        let parameters: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(parameters["$filter"], filter);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_search_paginates_beyond_fifty_using_exact_next_links() {
    let accounts: Vec<_> = (0..65)
        .map(|index| {
            let mut account = company();
            account["Code"] = json!(format!("{index:04}"));
            account
        })
        .collect();
    for (offset, limit, count, next) in [
        (0, 50, 50, Some(50)),
        (50, 10, 10, Some(60)),
        (60, 10, 5, None),
        (70, 10, 0, None),
    ] {
        let server = Server::new(vec![
            Response::json(json!({"d": {"results": accounts[..40], "__next": "?page=2"}})),
            Response::json(json!({"d": accounts[40..] })),
        ]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let args =
            serde_json::from_value(json!({"query": "Company", "offset": offset, "limit": limit}))
                .unwrap();
        let result = serde_json::to_value(backend.search_accounts(args).await.unwrap()).unwrap();
        assert_eq!(result["accounts"].as_array().unwrap().len(), count);
        assert_eq!(result["next_offset"], json!(next));
        assert_eq!(result["has_more"], next.is_some());
        if count > 0 {
            assert_eq!(result["accounts"][0]["code"], format!("{offset:04}"));
        }
        let requests = server.finish();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].target.contains("?page=2"));
        assert!(!requests[0].target.contains("$skip"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_search_stops_after_finding_one_extra_match() {
    let server = Server::new(vec![Response::json(json!({"d": {
        "results": [company(), company()], "__next": "?page=2"
    }}))]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let args = serde_json::from_value(json!({"query": "Company", "limit": 1})).unwrap();
    let result = serde_json::to_value(backend.search_accounts(args).await.unwrap()).unwrap();
    assert_eq!(result["next_offset"], 1);
    assert_eq!(server.finish().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_modes_offsets_and_due_date_ranges_are_validated_before_network_access() {
    let (_directory, backend) = financial_backend(&Url::parse("http://127.0.0.1:1").unwrap()).await;
    for args in [
        json!({"query": "not-a-uuid", "match_by": "id"}),
        json!({"query": "Company", "offset": u32::MAX}),
    ] {
        let error = backend
            .search_accounts(serde_json::from_value(args).unwrap())
            .await
            .err()
            .unwrap();
        assert!(!error.to_string().contains("request failed"));
    }
    for args in [
        json!({"query": "Company", "match_by": "unknown"}),
        json!({"query": "Company", "offset": -1}),
    ] {
        assert!(serde_json::from_value::<crate::financial::SearchAccounts>(args).is_err());
    }
    let args = serde_json::from_value(json!({
        "account_id": COMPANY_ID, "side": "payable",
        "due_date_from": "2026-02-01", "due_date_to": "2026-01-01"
    }))
    .unwrap();
    let error = backend.get_open_items(args).await.err().unwrap();
    assert!(error.to_string().contains("due_date_from"));
    for date in ["2026-02-30", "tomorrow", "2026-01-01T00:00:00Z"] {
        assert!(
            serde_json::from_value::<crate::financial::GetOpenItems>(json!({
                "account_id": COMPANY_ID, "side": "payable", "due_date_to": date
            }))
            .is_err()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outstanding_item_date_filters_and_summaries_cover_all_matching_pages() {
    let mut first = open_item(1, "EUR", json!(10));
    first["DueDate"] = json!("2000-01-01");
    let mut second = open_item(2, "EUR", json!(-2));
    second["DueDate"] = json!("2000-01-02");
    let mut third = open_item(3, "USD", json!(7));
    third["DueDate"] = json!("2000-01-02");
    let mut unknown = open_item(4, "EUR", json!(100));
    unknown["DueDate"] = Value::Null;
    let mut future = open_item(5, "EUR", json!(200));
    future["DueDate"] = json!("9999-01-01");
    let mut today = open_item(6, "EUR", json!(300));
    today["DueDate"] = json!(chrono::Local::now().date_naive().to_string());
    for (filters, count, euros, dollars) in [
        (json!({}), 6, Some("608"), Some("7")),
        (
            json!({"due_date_from": "2000-01-02", "due_date_to": "2000-01-02"}),
            2,
            Some("-2"),
            Some("7"),
        ),
        (json!({"due_date_to": "2000-01-01"}), 1, Some("10"), None),
        (json!({"due_date_from": "9999-01-01"}), 1, Some("200"), None),
        (json!({"overdue_only": true}), 3, Some("8"), Some("7")),
        (
            json!({"overdue_only": true, "due_date_from": "9999-01-01"}),
            0,
            None,
            None,
        ),
    ] {
        for summary_only in [false, true] {
            let server = Server::new(vec![
                company_response(),
                Response::json(json!({"d": {"results": [first, second], "__next": "?page=2"}})),
                Response::json(json!({"d": [third, unknown, future, today]})),
            ]);
            let (_directory, backend) = financial_backend(&server.base).await;
            let mut args = filters.clone();
            args["account_id"] = json!(COMPANY_ID);
            args["side"] = json!("payable");
            args["summary_only"] = json!(summary_only);
            let result = serde_json::to_value(
                backend
                    .get_open_items(serde_json::from_value(args).unwrap())
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(result["source_item_count"], 6);
            assert_eq!(result["matched_item_count"], count);
            assert_eq!(result["summary_only"], summary_only);
            for totals in result["totals_by_currency"].as_object().unwrap().values() {
                let aging = totals["aging_by_due_date"].as_object().unwrap();
                assert_eq!(
                    aging
                        .values()
                        .map(|bucket| bucket["item_count"].as_u64().unwrap())
                        .sum::<u64>(),
                    totals["item_count"].as_u64().unwrap()
                );
                for field in [
                    "net_open_amount",
                    "positive_open_amount",
                    "negative_open_amount",
                ] {
                    let sum: rust_decimal::Decimal = aging
                        .values()
                        .map(|bucket| {
                            bucket[field]
                                .as_str()
                                .unwrap()
                                .parse::<rust_decimal::Decimal>()
                                .unwrap()
                        })
                        .sum();
                    assert_eq!(
                        sum,
                        totals[field]
                            .as_str()
                            .unwrap()
                            .parse::<rust_decimal::Decimal>()
                            .unwrap()
                    );
                }
                if filters.get("overdue_only") == Some(&json!(true))
                    || filters.get("due_date_from").is_some()
                    || filters.get("due_date_to").is_some()
                {
                    assert!(!aging.contains_key("unknown_due_date"));
                }
            }
            assert_eq!(result["filters"]["due_date_from"], filters["due_date_from"]);
            assert_eq!(result["filters"]["due_date_to"], filters["due_date_to"]);
            assert_eq!(
                result["filters"]["overdue_only"],
                filters.get("overdue_only").unwrap_or(&json!(false)).clone()
            );
            assert_eq!(
                result["totals_by_currency"]["EUR"]["net_open_amount"],
                json!(euros)
            );
            assert_eq!(
                result["totals_by_currency"]["USD"]["net_open_amount"],
                json!(dollars)
            );
            if summary_only {
                assert!(result.get("items").is_none());
            } else {
                assert_eq!(result["items"].as_array().unwrap().len(), count);
                let today = chrono::NaiveDate::parse_from_str(
                    result["as_of_date"].as_str().unwrap(),
                    "%Y-%m-%d",
                )
                .unwrap();
                for item in result["items"].as_array().unwrap() {
                    let days = item["due_date"].as_str().map(|date| {
                        today
                            .signed_duration_since(
                                chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap(),
                            )
                            .num_days()
                            .max(0)
                    });
                    assert_eq!(item["days_overdue"], json!(days));
                }
            }
            assert_eq!(server.finish().len(), 3);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filtered_summaries_still_reject_invalid_excluded_rows() {
    let server = Server::new(vec![
        company_response(),
        Response::json(
            json!({"d": [open_item(1, "EUR", json!(10)), open_item(1, "EUR", json!(10))]}),
        ),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let args = serde_json::from_value(json!({
        "account_id": COMPANY_ID, "side": "payable",
        "due_date_from": "9999-01-01", "summary_only": true
    }))
    .unwrap();
    let error = backend.get_open_items(args).await.err().unwrap();
    assert!(error.to_string().contains("duplicate"));
    assert_eq!(server.finish().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outstanding_report_currency_and_purchase_references_are_explicit() {
    let mut item = open_item(1, "EUR", json!(12.34));
    item["InvoiceNumber"] = json!(12345);
    item["EntryNumber"] = json!(12345);
    item["YourRef"] = json!("SUPPLIER-FX-001");
    let server = Server::new(vec![
        company_response(),
        Response::json(json!({"d": [item]})),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result =
        serde_json::to_value(backend.get_open_items(open_items_args()).await.unwrap()).unwrap();
    assert_eq!(result["amount_basis"], "exact_report_currency");
    assert_eq!(result["items"][0]["amount"], "12.34");
    assert_eq!(result["items"][0]["currency"], "EUR");
    assert_eq!(result["items"][0]["invoice_number"], 12345);
    assert_eq!(result["items"][0]["entry_number"], 12345);
    assert_eq!(result["items"][0]["your_ref"], "SUPPLIER-FX-001");
    assert_eq!(
        result["totals_by_currency"]["EUR"]["net_open_amount"],
        "12.34"
    );
    let interpretation = result["interpretation"].as_str().unwrap();
    assert!(interpretation.contains("rather than the original invoice currency"));
    assert!(interpretation.contains("this server does not convert currencies"));
    assert!(interpretation.contains("invoice_number is Exact's entry number"));
    assert_eq!(server.finish().len(), 2);
}

fn balance_item(
    id: i64,
    account_id: &str,
    code: &str,
    name: &str,
    currency: &str,
    amount: Value,
) -> Value {
    json!({
        "HID": id, "AccountId": account_id, "AccountCode": code, "AccountName": name,
        "Amount": amount, "AmountInTransit": 0, "CurrencyCode": currency, "DueDate": "2000-01-01"
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outstanding_balances_group_all_pages_before_paginating_account_summaries() {
    const SECOND_ID: &str = "22222222-2222-2222-2222-222222222222";
    const THIRD_ID: &str = "33333333-3333-3333-3333-333333333333";
    let mut positive = balance_item(1, COMPANY_ID, "  1001", "Alpha", "EUR", json!(0.1));
    positive["AmountInTransit"] = json!(0.02);
    let mut credit = balance_item(2, COMPANY_ID, "  1001", "Alpha", "EUR", json!(-0.1));
    credit["AmountInTransit"] = Value::Null;
    credit["HID"] = json!("2");
    let beta = balance_item(3, SECOND_ID, "1002", "Beta", "USD", json!(2.5));
    let beta_euros = balance_item(6, SECOND_ID, "1002", "Beta", "EUR", json!(1.75));
    let mut future = balance_item(4, THIRD_ID, "1003", "Gamma", "EUR", json!(4));
    future["DueDate"] = json!("9999-01-01");
    let mut unknown = balance_item(5, THIRD_ID, "1003", "Gamma", "EUR", json!(5));
    unknown["DueDate"] = Value::Null;
    for (offset, expected_name, next) in [
        (0, Some("Alpha"), Some(1)),
        (1, Some("Beta"), Some(2)),
        (2, Some("Gamma"), None),
        (3, None, None),
    ] {
        let server = Server::new(vec![
            Response::json(json!({"d": {"results": [beta, positive], "__next": "?page=2"}})),
            Response::json(json!({"d": [credit, future, unknown, beta_euros]})),
        ]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let args = serde_json::from_value(json!({"side": "payable", "limit": 1, "offset": offset}))
            .unwrap();
        let result =
            serde_json::to_value(backend.get_outstanding_balances(args).await.unwrap()).unwrap();
        assert_eq!(result["administration"]["Code"], 42);
        assert_eq!(result["side"], "payable");
        assert_eq!(result["amount_basis"], "exact_report_currency");
        assert_eq!(result["source_item_count"], 6);
        assert_eq!(result["matched_item_count"], 6);
        assert_eq!(result["source_account_count"], 3);
        assert_eq!(result["matched_account_count"], 3);
        assert_eq!(result["offset"], offset);
        assert_eq!(result["next_offset"], json!(next));
        assert_eq!(result["has_more"], next.is_some());
        let euros = &result["totals_by_currency"]["EUR"];
        assert_eq!(euros["net_open_amount"], "10.75");
        assert_eq!(euros["positive_open_amount"], "10.85");
        assert_eq!(euros["negative_open_amount"], "-0.1");
        assert_eq!(euros["overdue_net_amount"], "1.75");
        assert_eq!(euros["known_in_transit_amount"], "0.02");
        assert_eq!(euros["items_with_unknown_in_transit"], 1);
        assert_eq!(euros["items_without_due_date"], 1);
        assert_eq!(
            result["totals_by_currency"]["USD"]["net_open_amount"],
            "2.5"
        );
        if let Some(name) = expected_name {
            assert_eq!(result["accounts"].as_array().unwrap().len(), 1);
            assert_eq!(result["accounts"][0]["account"]["name"], name);
            if name == "Alpha" {
                assert_eq!(result["accounts"][0]["account"]["code"], "  1001");
                let totals = &result["accounts"][0]["totals_by_currency"]["EUR"];
                assert_eq!(totals["net_open_amount"], "0.0");
                assert_eq!(totals["item_count"], 2);
                assert_eq!(totals["negative_open_amount"], "-0.1");
            } else if name == "Beta" {
                let totals = &result["accounts"][0]["totals_by_currency"];
                assert_eq!(totals.as_object().unwrap().len(), 2);
                assert_eq!(totals["EUR"]["net_open_amount"], "1.75");
                assert_eq!(totals["USD"]["net_open_amount"], "2.5");
            }
        } else {
            assert_eq!(result["accounts"], json!([]));
        }
        let requests = server.finish();
        assert_eq!(requests.len(), 2);
        let url = server_url(requests[0].target.split_whitespace().nth(1).unwrap());
        assert_eq!(url.path(), "/api/v1/42/read/financial/PayablesList");
        let parameters: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert!(parameters["$select"].contains("AccountCode,AccountName"));
        assert!(!parameters.contains_key("accountId"));
        assert!(!parameters.contains_key("$top"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outstanding_balances_filter_items_before_totalling_and_counting_accounts() {
    let today = chrono::Local::now().date_naive();
    let yesterday = today.pred_opt().unwrap();
    let tomorrow = today.succ_opt().unwrap();
    let mut overdue = balance_item(1, COMPANY_ID, "1001", "Alpha", "EUR", json!(1));
    overdue["DueDate"] = json!(yesterday.to_string());
    let mut due_today = balance_item(2, COMPANY_ID, "1001", "Alpha", "EUR", json!(2));
    due_today["DueDate"] = json!(today.to_string());
    let mut future = balance_item(
        3,
        "22222222-2222-2222-2222-222222222222",
        "1002",
        "Beta",
        "EUR",
        json!(3),
    );
    future["DueDate"] = json!(tomorrow.to_string());
    let mut unknown = balance_item(
        4,
        "33333333-3333-3333-3333-333333333333",
        "1003",
        "Gamma",
        "EUR",
        json!(4),
    );
    unknown["DueDate"] = Value::Null;
    for (filters, count, accounts, amount) in [
        (json!({}), 4, 3, "10"),
        (json!({"overdue_only": true}), 1, 1, "1"),
        (
            json!({"due_date_from": today, "due_date_to": today}),
            1,
            1,
            "2",
        ),
        (json!({"due_date_from": today}), 2, 2, "5"),
        (json!({"due_date_to": today}), 2, 1, "3"),
        (
            json!({"due_date_from": today, "overdue_only": true}),
            0,
            0,
            "0",
        ),
    ] {
        let server = Server::new(vec![Response::json(
            json!({"d": [overdue, due_today, future, unknown]}),
        )]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let mut args = filters.clone();
        args["side"] = json!("receivable");
        let result = serde_json::to_value(
            backend
                .get_outstanding_balances(serde_json::from_value(args).unwrap())
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["source_item_count"], 4);
        assert_eq!(result["source_account_count"], 3);
        assert_eq!(result["matched_item_count"], count);
        assert_eq!(result["matched_account_count"], accounts);
        assert_eq!(result["accounts"].as_array().unwrap().len(), accounts);
        assert_eq!(result["filters"]["due_date_from"], filters["due_date_from"]);
        assert_eq!(result["filters"]["due_date_to"], filters["due_date_to"]);
        assert_eq!(result["has_more"], false);
        assert_eq!(result["next_offset"], Value::Null);
        if count > 0 {
            assert_eq!(
                result["totals_by_currency"]["EUR"]["net_open_amount"],
                amount
            );
        } else {
            assert_eq!(result["totals_by_currency"], json!({}));
        }
        let requests = server.finish();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .target
                .contains("/42/read/financial/ReceivablesList?")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outstanding_balances_return_an_explicit_empty_report() {
    let server = Server::new(vec![Response::json(json!({"d": []}))]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = backend
        .get_outstanding_balances(serde_json::from_value(json!({"side": "receivable"})).unwrap())
        .await
        .unwrap();
    let result = serde_json::to_value(result).unwrap();
    for key in [
        "source_item_count",
        "matched_item_count",
        "source_account_count",
        "matched_account_count",
    ] {
        assert_eq!(result[key], 0);
    }
    assert_eq!(result["accounts"], json!([]));
    assert_eq!(result["totals_by_currency"], json!({}));
    assert_eq!(result["has_more"], false);
    assert_eq!(result["next_offset"], Value::Null);
    assert_eq!(server.finish().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outstanding_balances_reject_bad_rows_even_when_excluded_by_filters() {
    let valid = balance_item(1, COMPANY_ID, "1001", "Alpha", "EUR", json!(1));
    let mut inconsistent_name = valid.clone();
    inconsistent_name["HID"] = json!(2);
    inconsistent_name["AccountName"] = json!("Changed name");
    let mut inconsistent_code = inconsistent_name.clone();
    inconsistent_code["AccountName"] = json!("Alpha");
    inconsistent_code["AccountCode"] = json!("1002");
    let mut duplicate_other_account = valid.clone();
    duplicate_other_account["AccountId"] = json!("22222222-2222-2222-2222-222222222222");
    let mut cases = vec![
        vec![valid.clone(), valid.clone()],
        vec![valid.clone(), duplicate_other_account],
        vec![valid.clone(), inconsistent_name],
        vec![valid.clone(), inconsistent_code],
    ];
    for (key, value) in [
        ("CurrencyCode", json!(" ")),
        ("AccountName", json!(" ")),
        ("AccountCode", json!(" ")),
        ("AccountId", json!("00000000-0000-0000-0000-000000000000")),
        ("AccountId", Value::Null),
        ("DueDate", json!("invalid")),
    ] {
        let mut row = valid.clone();
        row[key] = value;
        cases.push(vec![row]);
    }
    for rows in cases {
        let server = Server::new(vec![Response::json(json!({"d": rows}))]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let result = backend
            .get_outstanding_balances(
                serde_json::from_value(json!({"side": "payable", "due_date_from": "9999-01-01"}))
                    .unwrap(),
            )
            .await;
        assert!(result.is_err());
        assert_eq!(server.finish().len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outstanding_balances_preserve_decimal_precision_and_reject_global_overflow() {
    for (first, second, expected) in [
        ("9007199254740993.01", "0.02", Some("9007199254740993.03")),
        ("79228162514264337593543950335", "1", None),
        ("79228162514264337593543950335", "0.1", None),
    ] {
        let server = Server::new(vec![Response::json(json!({"d": [
            balance_item(1, COMPANY_ID, "1001", "Alpha", "EUR", serde_json::from_str(first).unwrap()),
            balance_item(2, "22222222-2222-2222-2222-222222222222", "1002", "Beta", "EUR", serde_json::from_str(second).unwrap())
        ]}))]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let result = backend
            .get_outstanding_balances(
                serde_json::from_value(json!({"side": "payable", "limit": 1})).unwrap(),
            )
            .await;
        if let Some(expected) = expected {
            let result = serde_json::to_value(result.unwrap()).unwrap();
            assert_eq!(
                result["totals_by_currency"]["EUR"]["net_open_amount"],
                expected
            );
            assert_eq!(
                result["accounts"][0]["totals_by_currency"]["EUR"]["net_open_amount"],
                first
            );
        } else {
            assert!(result.is_err());
        }
        assert_eq!(server.finish().len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outstanding_balances_never_return_partial_totals_on_failed_source_pagination() {
    let mut responses = vec![
        Response::json(
            json!({"d": {"results": [balance_item(1, COMPANY_ID, "1001", "Alpha", "EUR", json!(1))], "__next": "?page=2"}}),
        ),
        Response {
            status: "429 Too Many Requests",
            body: "private financial data".into(),
            location: None,
        },
    ];
    responses.extend(std::iter::repeat_n(responses[1].clone(), 2));
    let server = Server::new(responses);
    let (_directory, backend) = financial_backend(&server.base).await;
    let error = backend
        .get_outstanding_balances(serde_json::from_value(json!({"side": "payable"})).unwrap())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("429"));
    assert!(!format!("{error:#}").contains("private financial data"));
    assert_eq!(server.finish().len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outstanding_balance_inputs_are_validated_before_network_access() {
    let (_directory, backend) = financial_backend(&Url::parse("http://127.0.0.1:1").unwrap()).await;
    for args in [
        json!({"side": "payable", "limit": 0}),
        json!({"side": "payable", "limit": 51}),
        json!({"side": "receivable", "offset": u32::MAX}),
        json!({"side": "receivable", "due_date_from": "2026-02-01", "due_date_to": "2026-01-01"}),
    ] {
        let error = backend
            .get_outstanding_balances(serde_json::from_value(args).unwrap())
            .await
            .err()
            .unwrap();
        assert!(!error.to_string().contains("request failed"));
    }
    for args in [
        json!({"side": "both"}),
        json!({"side": "payable", "due_date_to": "2026-02-30"}),
        json!({"side": "payable", "offset": -1}),
        json!({"side": "payable", "account_id": COMPANY_ID}),
    ] {
        assert!(serde_json::from_value::<crate::financial::GetOutstandingBalances>(args).is_err());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outstanding_balances_default_page_limit_does_not_truncate_overall_totals() {
    let rows: Vec<_> = (1..=51)
        .rev()
        .map(|index| {
            balance_item(
                index,
                &uuid::Uuid::from_u128(index as u128).to_string(),
                &format!("{index:04}"),
                &format!("Company {index:02}"),
                "EUR",
                json!(1),
            )
        })
        .collect();
    for (offset, count, next) in [(0, 50, Some(50)), (50, 1, None)] {
        let server = Server::new(vec![Response::json(json!({"d": rows}))]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let args = serde_json::from_value(json!({"side": "payable", "offset": offset})).unwrap();
        let result =
            serde_json::to_value(backend.get_outstanding_balances(args).await.unwrap()).unwrap();
        assert_eq!(result["accounts"].as_array().unwrap().len(), count);
        assert_eq!(
            result["accounts"][0]["account"]["name"],
            format!("Company {:02}", offset + 1)
        );
        assert_eq!(result["matched_account_count"], 51);
        assert_eq!(result["matched_item_count"], 51);
        assert_eq!(result["totals_by_currency"]["EUR"]["net_open_amount"], "51");
        assert_eq!(result["next_offset"], json!(next));
        assert_eq!(server.finish().len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outstanding_aging_reconciles_across_due_ranges_currencies_and_response_pages() {
    let today = chrono::Local::now().date_naive();
    let cases = [
        (Some(-1), "2.50"),
        (Some(0), "3.50"),
        (Some(1), "4.25"),
        (Some(30), "-1.25"),
        (Some(31), "5.50"),
        (Some(60), "6.50"),
        (Some(61), "7.75"),
        (Some(90), "8.25"),
        (Some(91), "-2.00"),
        (None, "9.00"),
    ];
    let mut rows: Vec<_> = cases
        .iter()
        .enumerate()
        .map(|(index, (days, amount))| {
            let mut item = balance_item(
                index as i64 + 1,
                COMPANY_ID,
                "1001",
                "Alpha",
                "EUR",
                serde_json::from_str(amount).unwrap(),
            );
            item["DueDate"] =
                json!(days.map(|days| (today - chrono::Duration::days(days)).to_string()));
            item["AmountInTransit"] = json!(0.01);
            item
        })
        .collect();
    let mut other = balance_item(
        11,
        "22222222-2222-2222-2222-222222222222",
        "1002",
        "Beta",
        "USD",
        json!(100),
    );
    other["DueDate"] = json!((today - chrono::Duration::days(31)).to_string());
    rows.push(other);
    for side in ["payable", "receivable"] {
        let server = Server::new(vec![
            Response::json(json!({"d": {"results": rows[..5], "__next": "?page=2"}})),
            Response::json(json!({"d": rows[5..]})),
        ]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let args = serde_json::from_value(json!({"side": side, "limit": 1})).unwrap();
        let result =
            serde_json::to_value(backend.get_outstanding_balances(args).await.unwrap()).unwrap();
        assert_eq!(result["as_of_date"], today.to_string());
        assert_eq!(result["accounts"].as_array().unwrap().len(), 1);
        assert_eq!(result["matched_account_count"], 2);
        assert_eq!(result["has_more"], true);
        let euro = &result["totals_by_currency"]["EUR"];
        assert_eq!(*euro, result["accounts"][0]["totals_by_currency"]["EUR"]);
        let aging = &euro["aging_by_due_date"];
        let money = |value: &Value| {
            value
                .as_str()
                .unwrap()
                .parse::<rust_decimal::Decimal>()
                .unwrap()
        };
        assert_eq!(aging.as_object().unwrap().len(), 6);
        for (bucket, count, amount, positive, negative) in [
            ("current", 2, "6", "6", "0"),
            ("overdue_1_30_days", 2, "3", "4.25", "-1.25"),
            ("overdue_31_60_days", 2, "12", "12", "0"),
            ("overdue_61_90_days", 2, "16", "16", "0"),
            ("overdue_over_90_days", 1, "-2", "0", "-2"),
            ("unknown_due_date", 1, "9", "9", "0"),
        ] {
            assert_eq!(aging[bucket]["item_count"], count);
            assert_eq!(
                money(&aging[bucket]["net_open_amount"]),
                amount.parse::<rust_decimal::Decimal>().unwrap()
            );
            assert_eq!(
                money(&aging[bucket]["positive_open_amount"]),
                positive.parse::<rust_decimal::Decimal>().unwrap()
            );
            assert_eq!(
                money(&aging[bucket]["negative_open_amount"]),
                negative.parse::<rust_decimal::Decimal>().unwrap()
            );
        }
        assert_eq!(
            money(&euro["net_open_amount"]),
            "44".parse::<rust_decimal::Decimal>().unwrap()
        );
        assert_eq!(
            money(&euro["overdue_net_amount"]),
            "29".parse::<rust_decimal::Decimal>().unwrap()
        );
        assert_eq!(
            money(&euro["known_in_transit_amount"]),
            "0.10".parse::<rust_decimal::Decimal>().unwrap()
        );
        assert_eq!(euro["items_without_due_date"], 1);
        let dollars = &result["totals_by_currency"]["USD"]["aging_by_due_date"];
        assert_eq!(dollars.as_object().unwrap().len(), 1);
        assert_eq!(dollars["overdue_31_60_days"]["net_open_amount"], "100");
        assert_eq!(server.finish().len(), 2);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_net_aging_buckets_retain_both_positive_and_negative_rows() {
    let server = Server::new(vec![Response::json(json!({"d": [
        balance_item(1, COMPANY_ID, "1001", "Alpha", "EUR", json!(0.1)),
        balance_item(2, COMPANY_ID, "1001", "Alpha", "EUR", json!(-0.1))
    ]}))]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = backend
        .get_outstanding_balances(
            serde_json::from_value(json!({"side": "receivable", "overdue_only": true})).unwrap(),
        )
        .await
        .unwrap();
    let result = serde_json::to_value(result).unwrap();
    let aging = &result["totals_by_currency"]["EUR"]["aging_by_due_date"];
    assert_eq!(aging.as_object().unwrap().len(), 1);
    let bucket = &aging["overdue_over_90_days"];
    assert_eq!(bucket["item_count"], 2);
    assert_eq!(bucket["net_open_amount"], "0.0");
    assert_eq!(bucket["positive_open_amount"], "0.1");
    assert_eq!(bucket["negative_open_amount"], "-0.1");
    assert_eq!(result["matched_account_count"], 1);
    assert_eq!(server.finish().len(), 1);
}

const INVOICE_ENTRY_ID: &str = "44444444-4444-4444-4444-444444444444";
const INVOICE_LINE_ID: &str = "66666666-6666-6666-6666-666666666666";

fn invoice_header(side: &str) -> Value {
    let mut header = json!({
        "EntryID": INVOICE_ENTRY_ID, "Division": 42, "EntryNumber": 12345,
        "Journal": if side == "payable" { "60" } else { "70" },
        "Currency": "USD", "AmountDC": if side == "payable" { -90.0 } else { 108.9 },
        "AmountFC": if side == "payable" { -100 } else { 121 },
        "VATAmountDC": if side == "payable" { 0.0 } else { 18.9 },
        "VATAmountFC": if side == "payable" { 0 } else { 21 }, "Rate": 0.9,
        "EntryDate": "/Date(946684800000)/", "DueDate": "2000-01-31",
        "InvoiceNumber": 12345, "YourRef": "INVOICE-001", "Description": "Invoice",
        "Type": if side == "payable" { 30 } else { 20 },
        "TypeDescription": "Accounting entry", "Status": 50, "StatusDescription": "Processed",
        "Reversal": false, "Document": "55555555-5555-5555-5555-555555555555",
        "DocumentNumber": 1, "DocumentSubject": "Linked invoice document"
    });
    let party = if side == "payable" {
        "Supplier"
    } else {
        "Customer"
    };
    header[party] = json!(COMPANY_ID);
    header[format!("{party}Name")] = json!("Company X");
    header
}

fn invoice_line() -> Value {
    json!({
        "ID": INVOICE_LINE_ID, "EntryID": INVOICE_ENTRY_ID, "Division": 42,
        "LineNumber": 1, "Description": "Ledger detail", "GLAccount": "77777777-7777-7777-7777-777777777777",
        "GLAccountCode": "4000", "GLAccountDescription": "Services", "Quantity": 0.125,
        "AmountDC": 90, "AmountFC": 100, "VATAmountDC": 18.9, "VATAmountFC": 21,
        "VATBaseAmountDC": 90, "VATBaseAmountFC": 100,
        "VATCode": "1", "VATCodeDescription": "Standard VAT", "VATPercentage": 0.21
    })
}

fn invoice_args(side: &str) -> crate::invoice::GetInvoiceDetails {
    serde_json::from_value(json!({
        "account_id": COMPANY_ID, "side": side, "entry_number": 12345,
        "journal_code": if side == "payable" { "60" } else { "70" }
    }))
    .unwrap()
}

fn division_currency_response() -> Response {
    Response::json(json!({"d": [{"Code": 42, "Currency": "EUR"}]}))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_details_scope_headers_and_paginated_lines_and_label_both_currencies() {
    for side in ["payable", "receivable"] {
        let mut first = invoice_line();
        first["AmountFC"] = json!(60);
        first["AmountDC"] = json!(54);
        first["VATAmountFC"] = json!(12.6);
        first["VATAmountDC"] = json!(11.34);
        first["VATBaseAmountFC"] = json!(60);
        first["VATBaseAmountDC"] = json!(54);
        first["LineNumber"] = json!(2);
        let mut second = invoice_line();
        second["ID"] = json!("88888888-8888-8888-8888-888888888888");
        second["AmountFC"] = json!(40);
        second["AmountDC"] = json!(36);
        second["VATAmountFC"] = json!(8.4);
        second["VATAmountDC"] = json!(7.56);
        second["VATBaseAmountFC"] = json!(40);
        second["VATBaseAmountDC"] = json!(36);
        let server = Server::new(vec![
            Response::json(json!({"d": [invoice_header(side)]})),
            division_currency_response(),
            Response::json(json!({"d": {"results": [first], "__next": "?page=2"}})),
            Response::json(json!({"d": [second]})),
        ]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let result = serde_json::to_value(
            backend
                .get_invoice_details(invoice_args(side))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["administration"]["Code"], 42);
        assert_eq!(result["side"], side);
        assert_eq!(result["default_currency"], "EUR");
        assert_eq!(result["amount_basis"], "exact_accounting_entry");
        assert_eq!(result["entry"]["currency"], "USD");
        assert_eq!(result["entry"]["account_id"], COMPANY_ID);
        assert_eq!(result["entry"]["account_name"], "Company X");
        assert_eq!(
            result["entry"]["amount_default_currency"],
            if side == "payable" { "-90.0" } else { "108.9" }
        );
        assert_eq!(
            result["entry"]["amount_transaction_currency"],
            if side == "payable" { "-100" } else { "121" }
        );
        assert_eq!(
            result["entry"]["vat_amount_transaction_currency"],
            if side == "payable" { "0" } else { "21" }
        );
        assert_eq!(result["lines"][0]["vat_amount_transaction_currency"], "8.4");
        assert_eq!(result["entry"]["rate"], "0.9");
        assert_eq!(result["entry"]["entry_date"], "2000-01-01");
        assert_eq!(result["entry"]["due_date"], "2000-01-31");
        assert_eq!(
            result["entry"]["document_id"],
            "55555555-5555-5555-5555-555555555555"
        );
        assert_eq!(result["line_count"], 2);
        assert_eq!(result["lines"][0]["line_number"], 1);
        assert_eq!(result["lines"][1]["line_number"], 2);
        assert_eq!(result["lines"][0]["quantity"], "0.125");
        assert_eq!(result["lines"][0]["vat_percentage"], "0.21");
        assert_eq!(result["lines"][0]["amount_transaction_currency"], "40");
        assert!(
            result["interpretation"]
                .as_str()
                .unwrap()
                .contains("NOT current outstanding balances")
        );
        let requests = server.finish();
        assert_eq!(requests.len(), 4);
        assert!(
            requests
                .iter()
                .all(|request| request.target.starts_with("GET "))
        );
        let header = server_url(requests[0].target.split_whitespace().nth(1).unwrap());
        let path = if side == "payable" {
            "purchaseentry/PurchaseEntries"
        } else {
            "salesentry/SalesEntries"
        };
        let party = if side == "payable" {
            "Supplier"
        } else {
            "Customer"
        };
        assert_eq!(header.path(), format!("/api/v1/42/{path}"));
        let parameters: HashMap<_, _> = header.query_pairs().into_owned().collect();
        let journal = if side == "payable" { "60" } else { "70" };
        assert_eq!(
            parameters["$filter"],
            format!(
                "EntryNumber eq 12345 and Journal eq '{journal}' and {party} eq guid'{COMPANY_ID}'"
            )
        );
        assert!(parameters["$select"].contains(&format!("{party},{party}Name")));
        let currency = server_url(requests[1].target.split_whitespace().nth(1).unwrap());
        assert_eq!(currency.path(), "/api/v1/42/system/Divisions");
        assert!(
            currency
                .query_pairs()
                .any(|(key, value)| key == "$filter" && value == "Code eq 42")
        );
        let lines = server_url(requests[2].target.split_whitespace().nth(1).unwrap());
        assert!(lines.path().ends_with(if side == "payable" {
            "/purchaseentry/PurchaseEntryLines"
        } else {
            "/salesentry/SalesEntryLines"
        }));
        assert!(lines.query_pairs().any(|(key, value)| key == "$filter"
            && value == format!("EntryID eq guid'{INVOICE_ENTRY_ID}'")));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_details_preserve_negative_precision_and_unknown_values_without_inference() {
    let mut header = invoice_header("payable");
    header["AmountFC"] = serde_json::from_str("-9007199254740993.01").unwrap();
    header["AmountDC"] = Value::Null;
    header["VATAmountFC"] = Value::Null;
    header["Rate"] = serde_json::from_str("1e-12").unwrap();
    header["Type"] = json!(31);
    header["Status"] = Value::Null;
    header["Reversal"] = json!(true);
    header["Document"] = Value::Null;
    let mut line = invoice_line();
    line["AmountFC"] = serde_json::from_str("-9007199254740993.01").unwrap();
    line["Quantity"] = serde_json::from_str("9007199254740993.01").unwrap();
    line["AmountDC"] = Value::Null;
    line["VATPercentage"] = Value::Null;
    let server = Server::new(vec![
        Response::json(json!({"d": [header]})),
        division_currency_response(),
        Response::json(json!({"d": [line]})),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .get_invoice_details(invoice_args("payable"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        result["entry"]["amount_transaction_currency"],
        "-9007199254740993.01"
    );
    assert_eq!(
        result["lines"][0]["amount_transaction_currency"],
        "-9007199254740993.01"
    );
    assert_eq!(result["lines"][0]["quantity"], "9007199254740993.01");
    assert_eq!(result["entry"]["amount_default_currency"], Value::Null);
    assert_eq!(
        result["entry"]["vat_amount_transaction_currency"],
        Value::Null
    );
    assert_eq!(result["lines"][0]["amount_default_currency"], Value::Null);
    assert_eq!(result["lines"][0]["vat_percentage"], Value::Null);
    assert_eq!(result["entry"]["status_code"], Value::Null);
    assert_eq!(result["entry"]["type_code"], 31);
    assert_eq!(result["entry"]["reversal"], true);
    assert_eq!(server.finish().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_headers_must_match_uniquely_before_reading_currency_or_lines() {
    let valid = invoice_header("payable");
    let mut cases = vec![vec![], vec![valid.clone(), valid.clone()]];
    for (key, value) in [
        ("Supplier", json!("22222222-2222-2222-2222-222222222222")),
        ("Division", json!(99)),
        ("Journal", json!("70")),
        ("EntryNumber", json!(54321)),
        ("EntryID", json!("00000000-0000-0000-0000-000000000000")),
        ("Currency", json!(" ")),
    ] {
        let mut header = valid.clone();
        header[key] = value;
        cases.push(vec![header]);
    }
    for entries in cases {
        let server = Server::new(vec![Response::json(json!({"d": entries}))]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .get_invoice_details(invoice_args("payable"))
                .await
                .is_err()
        );
        assert_eq!(server.finish().len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_lines_must_belong_to_the_entry_and_division_without_duplicates() {
    let valid = invoice_line();
    let mut cases = vec![vec![valid.clone(), valid.clone()]];
    for (key, value) in [
        ("EntryID", json!("22222222-2222-2222-2222-222222222222")),
        ("Division", json!(99)),
        ("ID", json!("00000000-0000-0000-0000-000000000000")),
    ] {
        let mut line = valid.clone();
        line[key] = value;
        cases.push(vec![line]);
    }
    for lines in cases {
        let server = Server::new(vec![
            Response::json(json!({"d":[invoice_header("receivable")]})),
            division_currency_response(),
            Response::json(json!({"d":lines})),
        ]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .get_invoice_details(invoice_args("receivable"))
                .await
                .is_err()
        );
        assert_eq!(server.finish().len(), 3);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_default_currency_must_resolve_without_using_the_transaction_currency_as_fallback()
{
    for divisions in [
        json!([]),
        json!([{"Code":99,"Currency":"EUR"}]),
        json!([{"Code":42,"Currency":" "}]),
        json!([{"Code":42,"Currency":"EUR"},{"Code":42,"Currency":"USD"}]),
    ] {
        let server = Server::new(vec![
            Response::json(json!({"d":[invoice_header("payable")]})),
            Response::json(json!({"d":divisions})),
        ]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .get_invoice_details(invoice_args("payable"))
                .await
                .is_err()
        );
        assert_eq!(server.finish().len(), 2);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_queries_escape_journals_and_reject_invalid_inputs_before_network_access() {
    let (_directory, backend) = financial_backend(&Url::parse("http://127.0.0.1:1").unwrap()).await;
    for (entry_number, journal, account) in [
        (0, "60", COMPANY_ID),
        (-1, "60", COMPANY_ID),
        (i64::from(i32::MAX) + 1, "60", COMPANY_ID),
        (1, "", COMPANY_ID),
        (1, "60", "company name"),
        (1, "60", "00000000-0000-0000-0000-000000000000"),
    ] {
        let args=serde_json::from_value(json!({"account_id":account,"side":"payable","entry_number":entry_number,"journal_code":journal})).unwrap();
        let error = backend.get_invoice_details(args).await.err().unwrap();
        assert!(!format!("{error:#}").contains("request failed"));
    }
    for args in [
        json!({"account_id": COMPANY_ID, "side": "both", "entry_number": 12345, "journal_code": "60"}),
        json!({"account_id": COMPANY_ID, "side": "payable", "entry_number": 12345}),
        json!({"account_id": COMPANY_ID, "side": "payable", "entry_number": 12345, "journal_code": "60", "url": "https://example.invalid"}),
    ] {
        assert!(serde_json::from_value::<crate::invoice::GetInvoiceDetails>(args).is_err());
    }
    let mut header = invoice_header("payable");
    header["Journal"] = json!("A'B");
    let server = Server::new(vec![
        Response::json(json!({"d":[header]})),
        division_currency_response(),
        Response::json(json!({"d":[]})),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let mut args = invoice_args("payable");
    args.journal_code = "  A'B  ".into();
    let result = serde_json::to_value(backend.get_invoice_details(args).await.unwrap()).unwrap();
    assert_eq!(result["line_count"], 0);
    let requests = server.finish();
    let url = server_url(requests[0].target.split_whitespace().nth(1).unwrap());
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "$filter" && value.contains("Journal eq 'A''B'"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_permission_and_pagination_errors_return_no_partial_details_or_response_bodies() {
    for failure_at in 0..3 {
        let mut responses = vec![
            Response::json(json!({"d":[invoice_header("payable")]})),
            division_currency_response(),
            Response::json(json!({"d":{"results":[invoice_line()],"__next":"?page=2"}})),
        ];
        if failure_at == 0 {
            responses.clear();
        } else if failure_at == 1 {
            responses.truncate(1);
        }
        responses.push(Response {
            status: "403 Forbidden",
            body: "private token and invoice details".into(),
            location: None,
        });
        let count = responses.len();
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        let error = backend
            .get_invoice_details(invoice_args("payable"))
            .await
            .err()
            .unwrap();
        let text = format!("{error:#}");
        assert!(text.contains("403"));
        assert!(!text.contains("private token and invoice details"));
        assert_eq!(server.finish().len(), count);
    }
}

const PDF_DOCUMENT_ID: &str = "55555555-5555-5555-5555-555555555555";
const PDF_ATTACHMENT_ID: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const PDF_BYTES: &[u8] = b"%PDF-1.7\nmock invoice\n%%EOF\n";

fn pdf_document() -> Value {
    json!({"ID": PDF_DOCUMENT_ID, "Division": 42, "Account": COMPANY_ID,
        "Type": 20, "TypeDescription": "Purchase invoice", "Subject": "Invoice",
        "FinancialTransactionEntryID": INVOICE_ENTRY_ID, "SalesInvoiceNumber": null})
}

fn pdf_attachment() -> Value {
    json!({"ID": PDF_ATTACHMENT_ID, "Document": PDF_DOCUMENT_ID,
        "FileName": "../../session.json.pdf", "FileSize": PDF_BYTES.len(),
        "Url": "https://untrusted.invalid/never-follow-this?token=private"})
}

fn pdf_catalog_responses() -> Vec<Response> {
    vec![
        Response::json(json!({"d":[invoice_header("payable")]})),
        Response::json(json!({"d":[pdf_document()]})),
        Response::json(json!({"d":[pdf_document()]})),
        Response::json(json!({"d":[pdf_attachment()]})),
    ]
}

fn pdf_response(bytes: &[u8]) -> Response {
    Response {
        status: "200 OK",
        body: String::from_utf8(bytes.to_vec()).unwrap(),
        location: None,
    }
}

fn pdf_args() -> crate::pdf::DownloadInvoicePdf {
    serde_json::from_value(json!({"invoice":{"account_id":COMPANY_ID,"side":"payable","entry_number":12345,"journal_code":"60"},"document_id":PDF_DOCUMENT_ID,"attachment_id":PDF_ATTACHMENT_ID})).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_pdf_download_revalidates_links_ignores_urls_and_saves_private_unique_files() {
    let mut responses = pdf_catalog_responses();
    responses.push(pdf_response(PDF_BYTES));
    responses.push(Response::json(json!({"d":[pdf_attachment()]})));
    responses.extend(pdf_catalog_responses());
    responses.push(pdf_response(PDF_BYTES));
    responses.push(Response::json(json!({"d":[pdf_attachment()]})));
    let server = Server::new(responses);
    let (directory, backend) = financial_backend(&server.base).await;
    let mut paths = Vec::new();
    for _ in 0..2 {
        let result =
            serde_json::to_value(backend.download_invoice_pdf(pdf_args()).await.unwrap()).unwrap();
        let path = PathBuf::from(result["saved_path"].as_str().unwrap());
        assert!(
            path.starts_with(
                directory
                    .path()
                    .canonicalize()
                    .unwrap()
                    .join("downloads/42")
            )
        );
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("invoice-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa-")
        );
        assert_eq!(path.extension().unwrap(), "pdf");
        assert_eq!(fs::read(&path).unwrap(), PDF_BYTES);
        assert_eq!(result["original_file_name"], "../../session.json.pdf");
        assert_eq!(result["byte_count"], PDF_BYTES.len());
        assert_eq!(result["media_type"], "application/pdf");
        assert_eq!(result["document"]["type_code"], 20);
        assert!(!result.to_string().contains("untrusted.invalid"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        paths.push(path);
    }
    assert_ne!(paths[0], paths[1]);
    let store = Store::open(directory.path()).await.unwrap();
    assert_eq!(
        store.require_session().await.unwrap().tokens.access_token,
        "old-access"
    );
    drop(store);
    let requests = server.finish();
    assert_eq!(requests.len(), 12);
    assert!(
        requests
            .iter()
            .all(|r| r.target.starts_with("GET /api/v1/42/")
                || r.target.starts_with("GET /docs/SysAttachment.aspx?"))
    );
    let binary = server_url(requests[4].target.split_whitespace().nth(1).unwrap());
    assert_eq!(binary.path(), "/docs/SysAttachment.aspx");
    assert!(
        binary
            .query_pairs()
            .any(|(key, value)| key == "ID" && value == PDF_ATTACHMENT_ID)
    );
    assert!(
        binary
            .query_pairs()
            .any(|(key, value)| key == "Download" && value == "1")
    );
    assert!(
        binary
            .query_pairs()
            .any(|(key, value)| key == "_Division_" && value == "42")
    );
    let url = server_url(requests[5].target.split_whitespace().nth(1).unwrap());
    assert_eq!(url.path(), "/api/v1/42/documents/DocumentAttachments");
    let parameters: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(
        parameters["$filter"],
        format!("ID eq guid'{PDF_ATTACHMENT_ID}' and Document eq guid'{PDF_DOCUMENT_ID}'")
    );
    assert_eq!(parameters["$select"], "ID,Document,FileName,FileSize");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_document_catalog_lists_provenance_without_downloading_bytes() {
    let server = Server::new(pdf_catalog_responses());
    let (directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .list_invoice_documents(invoice_args("payable"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["documents"].as_array().unwrap().len(), 1);
    let doc = &result["documents"][0];
    assert_eq!(doc["invoice_type_match"], true);
    assert_eq!(
        doc["sources"],
        json!(["accounting_header", "accounting_entry"])
    );
    assert_eq!(doc["attachments"][0]["pdf_filename"], true);
    assert_eq!(doc["attachments"][0]["id"], PDF_ATTACHMENT_ID);
    assert!(!directory.path().join("downloads").exists());
    assert!(!result.to_string().contains("untrusted.invalid"));
    let requests = server.finish();
    assert_eq!(requests.len(), 4);
    let url = server_url(requests[3].target.split_whitespace().nth(1).unwrap());
    assert!(
        url.query_pairs()
            .any(|(k, v)| k == "$select" && v == "ID,Document,FileName,FileSize")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_pdf_rejects_reminders_unrelated_ids_changed_metadata_and_invalid_content() {
    for case in [
        "reminder",
        "document",
        "attachment",
        "changed-id",
        "changed-document",
        "changed-name",
        "html",
        "truncated",
        "duplicate",
        "next-page",
        "redirect",
    ] {
        let mut responses = pdf_catalog_responses();
        let mut args = pdf_args();
        let mut fresh = pdf_attachment();
        let mut include_download = true;
        let mut include_fresh = true;
        let mut response = pdf_response(PDF_BYTES);
        match case {
            "reminder" => {
                let mut doc = pdf_document();
                doc["Type"] = json!(15);
                responses[1] = Response::json(json!({"d":[doc]}));
                responses[2] = Response::json(json!({"d":[doc]}));
                include_download = false;
            }
            "document" => {
                args.document_id = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".into();
                include_download = false;
            }
            "attachment" => {
                args.attachment_id = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".into();
                include_download = false;
            }
            "changed-id" => fresh["ID"] = json!("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"),
            "changed-document" => fresh["Document"] = json!("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"),
            "changed-name" => fresh["FileName"] = json!("other.pdf"),
            "html" => {
                response = pdf_response(b"<html>not a PDF</html>");
                include_fresh = false;
            }
            "truncated" => {
                response = pdf_response(b"%PDF-1.7\nmissing EOF");
                include_fresh = false;
            }
            "redirect" => {
                response = Response {
                    status: "302 Found",
                    body: "private data".into(),
                    location: Some("http://127.0.0.1:1/stolen".into()),
                };
                include_fresh = false;
            }
            _ => {}
        }
        if include_download {
            responses.push(response);
            if include_fresh {
                responses.push(Response::json(if case == "duplicate" {
                    json!({"d":[fresh,fresh]})
                } else if case == "next-page" {
                    json!({"d":{"results":[fresh],"__next":"http://127.0.0.1:1/stolen"}})
                } else {
                    json!({"d":[fresh]})
                }));
            }
        }
        let count = responses.len();
        let server = Server::new(responses);
        let (directory, backend) = financial_backend(&server.base).await;
        assert!(backend.download_invoice_pdf(args).await.is_err(), "{case}");
        assert!(!directory.path().join("downloads").exists(), "{case}");
        assert_eq!(server.finish().len(), count);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_document_metadata_cannot_cross_accounts_entries_or_administrations() {
    for (key, value) in [
        ("Account", json!("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb")),
        ("Division", json!(99)),
        (
            "FinancialTransactionEntryID",
            json!("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"),
        ),
    ] {
        let mut doc = pdf_document();
        doc[key] = value;
        let server = Server::new(vec![
            Response::json(json!({"d":[invoice_header("payable")]})),
            Response::json(json!({"d":[doc]})),
        ]);
        let (directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .list_invoice_documents(invoice_args("payable"))
                .await
                .is_err()
        );
        assert!(!directory.path().join("downloads").exists());
        assert_eq!(server.finish().len(), 2);
    }
    for case in ["wrong-document", "duplicate"] {
        let mut responses = pdf_catalog_responses();
        let mut attachment = pdf_attachment();
        if case == "wrong-document" {
            attachment["Document"] = json!("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
        }
        responses[3] = Response::json(if case == "duplicate" {
            json!({"d":[attachment,attachment]})
        } else {
            json!({"d":[attachment]})
        });
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .list_invoice_documents(invoice_args("payable"))
                .await
                .is_err()
        );
        assert_eq!(server.finish().len(), 4);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sales_invoice_document_discovery_uses_commercial_invoice_uuid_and_does_not_confuse_reminders()
 {
    let sales_id = "99999999-9999-9999-9999-999999999999";
    let reminder_id = "88888888-8888-8888-8888-888888888888";
    let mut header = invoice_header("receivable");
    header["Document"] = Value::Null;
    let mut original = pdf_document();
    original["Type"] = json!(10);
    original["FinancialTransactionEntryID"] = json!(sales_id);
    let mut reminder = pdf_document();
    reminder["ID"] = json!(reminder_id);
    reminder["Type"] = json!(15);
    reminder["TypeDescription"] = json!("Reminder");
    let mut reminder_attachment = pdf_attachment();
    reminder_attachment["ID"] = json!("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
    reminder_attachment["Document"] = json!(reminder_id);
    let server = Server::new(vec![
        Response::json(json!({"d":[header]})),
        Response::json(
            json!({"d":[{"InvoiceID":sales_id,"Division":42,"InvoiceNumber":12345,"InvoiceTo":COMPANY_ID,"Journal":"70","Document":null}]}),
        ),
        Response::json(json!({"d":[reminder]})),
        Response::json(json!({"d":[original]})),
        Response::json(json!({"d":[pdf_attachment()]})),
        Response::json(json!({"d":[reminder_attachment]})),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .list_invoice_documents(invoice_args("receivable"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["documents"].as_array().unwrap().len(), 2);
    assert_eq!(result["documents"][0]["id"], PDF_DOCUMENT_ID);
    assert_eq!(result["documents"][0]["invoice_type_match"], true);
    assert_eq!(result["documents"][0]["sources"], json!(["sales_invoice"]));
    assert_eq!(result["documents"][1]["invoice_type_match"], false);
    let requests = server.finish();
    assert_eq!(requests.len(), 6);
    let url = server_url(requests[1].target.split_whitespace().nth(1).unwrap());
    assert_eq!(url.path(), "/api/v1/42/salesinvoice/SalesInvoices");
    let url = server_url(requests[3].target.split_whitespace().nth(1).unwrap());
    assert!(
        url.query_pairs().any(|(k, v)| k == "$filter"
            && v == format!("FinancialTransactionEntryID eq guid'{sales_id}'"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pdf_attachment_http_body_limits_apply_with_and_without_content_length() {
    let server = Server::new(vec![Response::json(
        json!({"d":[{"Attachment":"X".repeat(256)}]}),
    )]);
    let (_directory, mut client) = async_exact_client(&server.base, false).await;
    client.pin_administration().unwrap();
    let result = client
        .get_invoice_pdf_bytes(42, uuid::Uuid::parse_str(PDF_ATTACHMENT_ID).unwrap(), 32)
        .await;
    assert!(result.is_err());
    assert_eq!(server.finish().len(), 1);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut stream, _) = listener.accept().await.unwrap();
        read_async_request(&mut stream).await;
        stream.write_all(format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n40\r\n{}\r\n0\r\n\r\n","X".repeat(64)).as_bytes()).await.unwrap();
    });
    let (_directory, mut client) = async_exact_client(&base, false).await;
    client.pin_administration().unwrap();
    assert!(
        client
            .get_invoice_pdf_bytes(42, uuid::Uuid::parse_str(PDF_ATTACHMENT_ID).unwrap(), 32)
            .await
            .is_err()
    );
    task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pdf_binary_size_limit_rejects_advertised_oversize_before_reading() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let (mut stream, _) = listener.accept().await.unwrap();
        read_async_request(&mut stream).await;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    crate::pdf::MAX_PDF_BYTES + 1
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let (directory, mut client) = async_exact_client(&base, false).await;
    client.pin_administration().unwrap();
    let error = client
        .get_invoice_pdf_bytes(
            42,
            uuid::Uuid::parse_str(PDF_ATTACHMENT_ID).unwrap(),
            crate::pdf::MAX_PDF_BYTES,
        )
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("size limit"));
    assert!(!directory.path().join("downloads").exists());
    task.await.unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_pdf_storage_rejects_public_directories_and_symlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for symbolic in [false, true] {
        let directory = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let store = Store::open(directory.path()).await.unwrap();
        let downloads = directory.path().join("downloads");
        if symbolic {
            symlink(outside.path(), &downloads).unwrap();
        } else {
            fs::create_dir(&downloads).unwrap();
            fs::set_permissions(&downloads, fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(
            store
                .save_invoice_pdf(
                    42,
                    uuid::Uuid::parse_str(PDF_ATTACHMENT_ID).unwrap(),
                    PDF_BYTES.to_vec()
                )
                .await
                .is_err()
        );
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_pdf_inputs_and_final_save_reject_invalid_ids_or_changed_scope() {
    let (_directory, backend) = financial_backend(&Url::parse("http://127.0.0.1:1").unwrap()).await;
    for (document, attachment) in [
        ("not-a-uuid", PDF_ATTACHMENT_ID),
        (PDF_DOCUMENT_ID, "00000000-0000-0000-0000-000000000000"),
    ] {
        let mut args = pdf_args();
        args.document_id = document.into();
        args.attachment_id = attachment.into();
        let error = backend.download_invoice_pdf(args).await.err().unwrap();
        assert!(!format!("{error:#}").contains("request failed"));
    }
    let mut value=serde_json::to_value(json!({"invoice":{"account_id":COMPANY_ID,"side":"payable","entry_number":12345,"journal_code":"60"},"document_id":PDF_DOCUMENT_ID,"attachment_id":PDF_ATTACHMENT_ID})).unwrap();
    value["output_path"] = json!("/tmp/arbitrary.pdf");
    assert!(serde_json::from_value::<crate::pdf::DownloadInvoicePdf>(value).is_err());
    let (directory, mut client) =
        async_exact_client(&Url::parse("http://127.0.0.1:1").unwrap(), false).await;
    client.pin_administration().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    let mut state = store.require_session().await.unwrap();
    state.administration.as_mut().unwrap().code = 99;
    store.save(&state).await.unwrap();
    drop(store);
    let error = client
        .save_invoice_pdf(
            42,
            uuid::Uuid::parse_str(PDF_ATTACHMENT_ID).unwrap(),
            PDF_BYTES.to_vec(),
        )
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("restart"));
    assert!(!directory.path().join("downloads").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commercial_invoice_document_mapping_rejects_wrong_or_ambiguous_invoices() {
    let mut header = invoice_header("receivable");
    header["Document"] = Value::Null;
    let valid = json!({"InvoiceID":"99999999-9999-9999-9999-999999999999","Division":42,"InvoiceNumber":12345,"InvoiceTo":COMPANY_ID,"Journal":"70","Document":null});
    let mut cases = vec![vec![valid.clone(), valid.clone()]];
    for (key, value) in [
        ("InvoiceTo", json!("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb")),
        ("Division", json!(99)),
        ("InvoiceNumber", json!(54321)),
        ("Journal", json!("60")),
        ("InvoiceID", json!("00000000-0000-0000-0000-000000000000")),
    ] {
        let mut invoice = valid.clone();
        invoice[key] = value;
        cases.push(vec![invoice]);
    }
    for invoices in cases {
        let server = Server::new(vec![
            Response::json(json!({"d":[header]})),
            Response::json(json!({"d":invoices})),
        ]);
        let (directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .list_invoice_documents(invoice_args("receivable"))
                .await
                .is_err()
        );
        assert!(!directory.path().join("downloads").exists());
        assert_eq!(server.finish().len(), 2);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_original_invoice_pdfs_are_not_generated_or_guessed() {
    let mut header = invoice_header("receivable");
    header["Document"] = Value::Null;
    let server = Server::new(vec![
        Response::json(json!({"d":[header]})),
        Response::json(json!({"d":[]})),
        Response::json(json!({"d":[]})),
    ]);
    let (directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .list_invoice_documents(invoice_args("receivable"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["documents"], json!([]));
    assert!(!directory.path().join("downloads").exists());
    let requests = server.finish();
    assert_eq!(requests.len(), 3);
    assert!(
        requests
            .iter()
            .all(|request| request.target.starts_with("GET "))
    );
}

fn matching_ledger(entry: i64, journal: &str, id: &str) -> Value {
    json!({"ID":id,"EntryID":if entry==12345 {INVOICE_ENTRY_ID} else {"99999999-9999-9999-9999-999999999999"},
        "Division":42,"Account":COMPANY_ID,"EntryNumber":entry,"FinancialYear":2025,"FinancialPeriod":12,
        "JournalCode":journal,"GLAccountCode":" 01200 ","LineNumber":0,"Type":if journal=="20" {40} else {20},
        "Date":"2025-12-20","AmountDC":42.125,"AmountFC":51.25,"Currency":"USD","InvoiceNumber":12345})
}

fn matching_member(entry: i64, journal: &str, amount: &str) -> String {
    format!(
        "<MatchLine finyear=\"2025\" finperiod=\"12\" journal=\"{journal}\" entry=\"{entry}\" amountdc=\"{amount}\"/>"
    )
}

fn matching_set(code: &str, members: &str) -> String {
    format!(
        "<MatchSet><GLAccount code=\"01200\"/><Account code=\"{code}\"/><MatchLines>{members}</MatchLines></MatchSet>"
    )
}

fn matching_page(sets: &str, count: usize, size: usize, cursor: &str) -> Response {
    Response {
        status: "200 OK",
        location: None,
        body: format!(
            "<eExact><MatchSets>{sets}</MatchSets><Topics><Topic code=\"MatchSets\" ts_d=\"{cursor}\" count=\"{count}\" pagesize=\"{size}\"/></Topics><Messages/></eExact>"
        ),
    }
}

fn matching_setup() -> Vec<Response> {
    let mut header = invoice_header("receivable");
    header["AmountDC"] = json!(42.125);
    vec![
        Response::json(json!({"d":[header]})),
        Response::json(json!({"d":[{"ID":COMPANY_ID,"Division":42,"Code":" 00042 "}]})),
        division_currency_response(),
        Response::json(json!({"d":[matching_ledger(12345,"70",INVOICE_LINE_ID)]})),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_matches_read_every_xml_page_filter_codes_exactly_and_keep_ambiguous_ledger_lines()
{
    let pair = format!(
        "{}{}",
        matching_member(12345, "70", "42.125"),
        matching_member(54321, "20", "-42.125")
    );
    let mut responses = matching_setup();
    responses.push(matching_page(
        &matching_set("000042", &pair),
        1,
        1,
        "0x0000000000000001",
    ));
    responses.push(matching_page(
        &matching_set("00042", &pair),
        1,
        1,
        "0x0000000000000002",
    ));
    responses.push(Response {
        status: "200 OK",
        location: None,
        body: "<eExact><Messages/></eExact>".into(),
    });
    let first = matching_ledger(54321, "20", "77777777-7777-7777-7777-777777777777");
    let mut second = first.clone();
    second["ID"] = json!("88888888-8888-8888-8888-888888888888");
    second["LineNumber"] = json!(2);
    second["AmountDC"] = json!(-42.125);
    responses.push(Response::json(
        json!({"d":{"results":[first],"__next":"?page=2"}}),
    ));
    responses.push(Response::json(json!({"d":[second]})));
    let server = Server::new(responses);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .get_invoice_matches(invoice_args("receivable"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["export_pages_read"], 3);
    assert_eq!(result["matching_group_count"], 1);
    assert_eq!(result["account_code"], "00042");
    assert_eq!(result["groups"][0]["export_page"], 2);
    assert_eq!(
        result["groups"][0]["members"][0]["allocated_amount_default_currency"],
        "42.125"
    );
    assert_eq!(
        result["groups"][0]["members"][1]["allocated_amount_default_currency"],
        "-42.125"
    );
    assert_eq!(result["groups"][0]["members"][0]["is_target_invoice"], true);
    let evidence = result["ledger_evidence"].as_array().unwrap();
    let bank = evidence
        .iter()
        .find(|x| x["reference"]["journal_code"] == "20")
        .unwrap();
    assert_eq!(bank["resolution"], "ambiguous_ledger_lines");
    assert_eq!(bank["candidates"].as_array().unwrap().len(), 2);
    assert_eq!(result["default_currency"], "EUR");
    assert_eq!(result["entry"]["currency"], "USD");
    assert!(
        result["interpretation"]
            .as_str()
            .unwrap()
            .contains("not bank settlement")
    );
    let base = server.base.clone();
    let requests = server.finish();
    assert_eq!(requests.len(), 9);
    for (index, request) in requests.iter().enumerate().take(7).skip(4) {
        let url = base
            .join(request.target.split_whitespace().nth(1).unwrap())
            .unwrap();
        assert_eq!(url.path(), "/docs/XMLDownload.aspx");
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params["_Division_"], "42");
        assert_eq!(params["Topic"], "MatchSets");
        assert!(!params.contains_key("Params_DownloadID"));
        assert_eq!(params.len(), if index == 4 { 2 } else { 3 });
        if index > 4 {
            assert_eq!(params["TSPaging"], format!("0x{:016X}", index - 4));
        }
        assert_eq!(request.headers["authorization"], "Bearer old-access");
    }
    assert!(
        requests
            .iter()
            .all(|r| r.target.starts_with("GET ") && r.body.is_empty())
    );
    assert!(!result.to_string().contains("000042"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_matches_preserve_partial_and_credit_group_members_without_pairwise_apportionment()
{
    let first = format!(
        "{}{}{}",
        matching_member(12345, "70", "25.125"),
        matching_member(54321, "20", "-20"),
        matching_member(54322, "70", "-5.125")
    );
    let second = format!(
        "{}{}",
        matching_member(12345, "70", "17"),
        matching_member(54321, "20", "-17")
    );
    let mut responses = matching_setup();
    responses.push(matching_page(
        &format!(
            "{}{}",
            matching_set("00042", &first),
            matching_set("00042", &second)
        ),
        2,
        1000,
        "0x0000000000000003",
    ));
    let mut bank = matching_ledger(54321, "20", "77777777-7777-7777-7777-777777777777");
    bank["AmountDC"] = json!(-1000);
    responses.push(Response::json(json!({"d":[bank]})));
    let mut credit = matching_ledger(54322, "70", "88888888-8888-8888-8888-888888888888");
    credit["Type"] = json!(21);
    credit["AmountDC"] = json!(-5.125);
    responses.push(Response::json(json!({"d":[credit]})));
    let server = Server::new(responses);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .get_invoice_matches(invoice_args("receivable"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["matching_group_count"], 2);
    assert_eq!(result["groups"][0]["members"].as_array().unwrap().len(), 3);
    assert_eq!(
        result["groups"][0]["members"][0]["allocated_amount_default_currency"],
        "25.125"
    );
    assert_eq!(
        result["groups"][1]["members"][0]["allocated_amount_default_currency"],
        "17"
    );
    assert_eq!(result["ledger_evidence"].as_array().unwrap().len(), 3);
    assert!(result.get("remaining_amount").is_none());
    assert!(result.get("paid_amount").is_none());
    assert!(
        result["interpretation"]
            .as_str()
            .unwrap()
            .contains("not an apportioned invoice-to-payment edge")
    );
    assert_eq!(server.finish().len(), 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_matches_no_groups_or_missing_peer_rows_do_not_imply_settlement_status() {
    for missing in [false, true] {
        let mut responses = matching_setup();
        let pair = matching_set(
            "00042",
            &format!(
                "{}{}",
                matching_member(12345, "70", "42.125"),
                matching_member(54321, "20", "-42.125")
            ),
        );
        responses.push(matching_page(
            if missing { &pair } else { "" },
            usize::from(missing),
            1000,
            "0x0000000000000001",
        ));
        if missing {
            responses.push(Response::json(json!({"d":[]})));
        }
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        let result = serde_json::to_value(
            backend
                .get_invoice_matches(invoice_args("receivable"))
                .await
                .unwrap(),
        )
        .unwrap();
        if missing {
            assert!(
                result["ledger_evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["resolution"] == "not_found")
            );
        } else {
            assert_eq!(result["matching_group_count"], 0);
            assert_eq!(result["ledger_evidence"], json!([]));
        }
        assert!(
            result["interpretation"]
                .as_str()
                .unwrap()
                .contains("not proof of nonpayment")
        );
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_matches_reject_cursor_cycles_inconsistent_pages_source_errors_and_redirects() {
    let pair = matching_set(
        "00042",
        &format!(
            "{}{}",
            matching_member(12345, "70", "42.125"),
            matching_member(54321, "20", "-42.125")
        ),
    );
    for next in [
        matching_page(&pair, 1, 1, "0x0000000000000001"),
        matching_page(&pair, 1, 2, "0x0000000000000002"),
        matching_page(&pair, 2, 1, "0x0000000000000002"),
        Response {
            status: "403 Forbidden",
            body: "PRIVATE_FINANCIAL_TEXT".into(),
            location: None,
        },
        Response {
            status: "429 Too Many Requests",
            body: "PRIVATE_FINANCIAL_TEXT".into(),
            location: None,
        },
        Response {
            status: "302 Found",
            body: "PRIVATE_FINANCIAL_TEXT".into(),
            location: Some("https://untrusted.invalid/".into()),
        },
    ] {
        let mut responses = matching_setup();
        responses.push(matching_page(&pair, 1, 1, "0x0000000000000001"));
        let retry_count = if next.status.starts_with("429") { 2 } else { 0 };
        responses.extend(std::iter::repeat_n(next.clone(), retry_count));
        responses.push(next);
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        let error = backend
            .get_invoice_matches(invoice_args("receivable"))
            .await
            .err()
            .unwrap();
        assert!(!format!("{error:#}").contains("PRIVATE_FINANCIAL_TEXT"));
        assert_eq!(server.finish().len(), 6 + retry_count);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_matches_reject_wrong_identity_and_wrong_or_duplicate_ledger_rows() {
    let pair = matching_set(
        "00042",
        &format!(
            "{}{}",
            matching_member(12345, "70", "42.125"),
            matching_member(54321, "20", "-42.125")
        ),
    );
    for field in [
        "Division",
        "Account",
        "FinancialYear",
        "FinancialPeriod",
        "JournalCode",
        "EntryNumber",
        "ID",
    ] {
        let mut row = matching_ledger(54321, "20", "77777777-7777-7777-7777-777777777777");
        row[field] = match field {
            "Division" => json!(43),
            "Account" => json!("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            "FinancialYear" => json!(2024),
            "FinancialPeriod" => json!(11),
            "JournalCode" => json!("30"),
            "EntryNumber" => json!(54322),
            _ => json!(uuid::Uuid::nil().to_string()),
        };
        let mut responses = matching_setup();
        responses.push(matching_page(&pair, 1, 1000, "0x0000000000000001"));
        responses.push(Response::json(json!({"d":[row]})));
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .get_invoice_matches(invoice_args("receivable"))
                .await
                .is_err()
        );
        server.finish();
    }
    let row = matching_ledger(54321, "20", "77777777-7777-7777-7777-777777777777");
    let mut responses = matching_setup();
    responses.push(matching_page(&pair, 1, 1000, "0x0000000000000001"));
    responses.push(Response::json(json!({"d":[row.clone(),row]})));
    let server = Server::new(responses);
    let (_directory, backend) = financial_backend(&server.base).await;
    assert!(
        backend
            .get_invoice_matches(invoice_args("receivable"))
            .await
            .is_err()
    );
    server.finish();
    for account in [
        json!([]),
        json!([{"ID":COMPANY_ID,"Division":43,"Code":"00042"}]),
        json!([{"ID":COMPANY_ID,"Division":42,"Code":" "}]),
    ] {
        let mut responses = matching_setup();
        responses.truncate(2);
        responses[1] = Response::json(json!({"d":account}));
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .get_invoice_matches(invoice_args("receivable"))
                .await
                .is_err()
        );
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_matching_limits_never_return_partial_groups_or_unbounded_ledger_evidence() {
    let pair = matching_set(
        "00042",
        &format!(
            "{}{}",
            matching_member(12345, "70", "42.125"),
            matching_member(54321, "20", "-42.125")
        ),
    );
    let mut responses = matching_setup();
    for cursor in 1..=128 {
        responses.push(matching_page(&pair, 1, 1, &format!("0x{cursor:016X}")));
    }
    let server = Server::new(responses);
    let (_directory, backend) = financial_backend(&server.base).await;
    let error = backend
        .get_invoice_matches(invoice_args("receivable"))
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("page limit"));
    assert_eq!(server.finish().len(), 132);
    let mut members = matching_member(12345, "70", "256");
    for entry in 1000..1256 {
        members.push_str(&matching_member(entry, "20", "-1"));
    }
    let mut responses = matching_setup();
    responses.push(matching_page(
        &matching_set("00042", &members),
        1,
        1000,
        "0x0000000000000001",
    ));
    let server = Server::new(responses);
    let (_directory, backend) = financial_backend(&server.base).await;
    let error = backend
        .get_invoice_matches(invoice_args("receivable"))
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("evidence limit"));
    assert_eq!(server.finish().len(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn matching_xml_requests_reject_wrong_administration_cursors_and_changed_scope_before_io() {
    let server = Server::new(vec![]);
    let (directory, _backend) = financial_backend(&server.base).await;
    let mut client = exact::ExactClient::new(
        directory.path().to_owned(),
        oauth::http_client().unwrap(),
        None,
    )
    .await
    .unwrap();
    client.pin_administration().unwrap();
    assert!(client.get_match_sets_page(43, None, 32).await.is_err());
    assert!(
        client
            .get_match_sets_page(42, Some("0x0000000000000001&Topic=Accounts"), 32)
            .await
            .is_err()
    );
    let store = Store::open(directory.path()).await.unwrap();
    let mut changed = store.require_session().await.unwrap();
    changed.administration.as_mut().unwrap().code = 43;
    store.save(&changed).await.unwrap();
    drop(store);
    assert!(client.get_match_sets_page(42, None, 32).await.is_err());
    assert_eq!(server.finish().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn matching_xml_body_size_is_bounded_and_rejects_oversize_before_parsing() {
    let server = Server::new(vec![Response {
        status: "200 OK",
        location: None,
        body: "x".repeat(64),
    }]);
    let (directory, _backend) = financial_backend(&server.base).await;
    let mut client = exact::ExactClient::new(
        directory.path().to_owned(),
        oauth::http_client().unwrap(),
        None,
    )
    .await
    .unwrap();
    client.pin_administration().unwrap();
    assert!(
        client
            .get_match_sets_page(42, None, 32)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("size limit")
    );
    assert_eq!(server.finish().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_matches_reject_reused_ledger_ids_across_invoice_and_counterpart() {
    let pair = matching_set(
        "00042",
        &format!(
            "{}{}",
            matching_member(12345, "70", "42.125"),
            matching_member(54321, "20", "-42.125")
        ),
    );
    let mut responses = matching_setup();
    responses.push(matching_page(&pair, 1, 1000, "0x0000000000000001"));
    responses.push(Response::json(
        json!({"d":[matching_ledger(54321,"20",INVOICE_LINE_ID)]}),
    ));
    let server = Server::new(responses);
    let (_directory, backend) = financial_backend(&server.base).await;
    assert!(
        backend
            .get_invoice_matches(invoice_args("receivable"))
            .await
            .is_err()
    );
    server.finish();
}

fn invoice_search_args(side: &str) -> crate::invoice::SearchInvoices {
    serde_json::from_value(json!({"account_id":COMPANY_ID,"side":side})).unwrap()
}

fn invoice_search_setup() -> Vec<Response> {
    vec![
        Response::json(
            json!({"d":[{"ID":COMPANY_ID,"Division":42,"Code":" 00100 ","Name":"Company X"}]}),
        ),
        division_currency_response(),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_search_reads_all_pages_filters_dates_and_references_then_paginates_stably() {
    for side in ["payable", "receivable"] {
        for offset in [0, 1] {
            let mut old = invoice_header(side);
            old["EntryDate"] = json!("2025-01-01");
            old["YourRef"] = json!("Supplier-X");
            old["Currency"] = json!(" USD ");
            let mut recent = old.clone();
            recent["EntryID"] = json!("77777777-7777-7777-7777-777777777777");
            recent["EntryNumber"] = json!(12346);
            recent["EntryDate"] = json!("2025-12-31");
            recent["YourRef"] = json!("SUPPLIER-X-2");
            let mut unrelated = old.clone();
            unrelated["EntryID"] = json!("88888888-8888-8888-8888-888888888888");
            unrelated["YourRef"] = json!("Other");
            let mut unknown = old.clone();
            unknown["EntryID"] = json!("99999999-9999-9999-9999-999999999999");
            unknown["EntryDate"] = Value::Null;
            let mut outside = old.clone();
            outside["EntryID"] = json!("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
            outside["EntryDate"] = json!("2024-12-31");
            let mut responses = invoice_search_setup();
            responses.push(Response::json(
                json!({"d":{"results":[old,unrelated,unknown],"__next":"?page=2"}}),
            ));
            responses.push(Response::json(json!({"d":[recent,outside]})));
            let server = Server::new(responses);
            let (_directory, backend) = financial_backend(&server.base).await;
            let mut args = invoice_search_args(side);
            args.query = Some(" supplier-x ".into());
            args.entry_date_from = Some("2025-01-01".parse().unwrap());
            args.entry_date_to = Some("2025-12-31".parse().unwrap());
            args.limit = Some(1);
            args.offset = offset;
            let result =
                serde_json::to_value(backend.search_invoices(args).await.unwrap()).unwrap();
            assert_eq!(result["source_entry_count"], 5);
            assert_eq!(result["matched_entry_count"], 2);
            assert_eq!(result["query"], "supplier-x");
            assert_eq!(result["default_currency"], "EUR");
            assert_eq!(
                result["invoices"][0]["entry_number"],
                if offset == 0 { 12346 } else { 12345 }
            );
            assert_eq!(result["invoices"][0]["currency"], "USD");
            assert_eq!(result["invoices"][0]["status_code"], 50);
            assert!(result["invoices"][0].get("paid").is_none());
            assert_eq!(result["has_more"], offset == 0);
            assert_eq!(
                result["next_offset"],
                if offset == 0 { json!(1) } else { Value::Null }
            );
            let base = server.base.clone();
            let requests = server.finish();
            assert_eq!(requests.len(), 4);
            let url = base
                .join(requests[2].target.split_whitespace().nth(1).unwrap())
                .unwrap();
            let parameters: HashMap<_, _> = url.query_pairs().into_owned().collect();
            assert_eq!(
                parameters["$filter"],
                format!(
                    "{} eq guid'{COMPANY_ID}'",
                    if side == "payable" {
                        "Supplier"
                    } else {
                        "Customer"
                    }
                )
            );
            assert!(!parameters["$filter"].contains("supplier-x"));
            assert!(
                requests
                    .iter()
                    .all(|r| r.target.starts_with("GET ") && r.body.is_empty())
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_search_uses_literal_text_exact_numeric_renderings_and_keeps_reused_accounting_numbers()
 {
    for (query, expected) in [
        (Some("000123"), 1),
        (Some("123"), 2),
        (Some("2025001"), 1),
        (Some("O'NEIL_%"), 1),
        (None, 2),
    ] {
        let mut first = invoice_header("payable");
        first["EntryNumber"] = json!(123);
        first["InvoiceNumber"] = json!(2025001);
        first["YourRef"] = json!("Supplier-000123");
        first["Description"] = json!("Work for O'Neil_%");
        let mut second = first.clone();
        second["EntryID"] = json!("77777777-7777-7777-7777-777777777777");
        second["EntryDate"] = Value::Null;
        second["InvoiceNumber"] = json!(123);
        second["YourRef"] = Value::Null;
        second["Description"] = Value::Null;
        let mut responses = invoice_search_setup();
        responses.push(Response::json(json!({"d":[second,first]})));
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        let mut args = invoice_search_args("payable");
        args.query = query.map(str::to_owned);
        let result = serde_json::to_value(backend.search_invoices(args).await.unwrap()).unwrap();
        assert_eq!(result["matched_entry_count"], expected);
        if query.is_none() {
            assert_eq!(result["invoices"][0]["entry_id"], INVOICE_ENTRY_ID);
            assert!(result["invoices"][1]["entry_date"].is_null());
        }
        assert_eq!(result["invoices"][0]["amount_default_currency"], "-90.0");
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_search_rejects_invalid_inputs_before_network_and_validates_excluded_source_rows() {
    let (_directory, backend) = financial_backend(&Url::parse("http://127.0.0.1:1").unwrap()).await;
    for fields in [
        json!({"account_id":"bad"}),
        json!({"account_id":uuid::Uuid::nil().to_string()}),
        json!({"query":" "}),
        json!({"query":"x".repeat(101)}),
        json!({"limit":0}),
        json!({"limit":51}),
        json!({"offset":u32::MAX}),
        json!({"entry_date_from":"2025-02-01","entry_date_to":"2025-01-01"}),
    ] {
        let mut value = json!({"account_id":COMPANY_ID,"side":"payable"});
        value
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let error = backend
            .search_invoices(serde_json::from_value(value).unwrap())
            .await
            .err()
            .unwrap();
        assert!(!format!("{error:#}").contains("request failed"));
    }
    for field in [
        "Division",
        "Supplier",
        "EntryID",
        "EntryNumber",
        "Journal",
        "Currency",
    ] {
        let mut entry = invoice_header("payable");
        entry["Description"] = json!("not a match");
        entry[field] = match field {
            "Division" => json!(43),
            "Supplier" => json!("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            "EntryID" => json!(uuid::Uuid::nil().to_string()),
            "EntryNumber" => json!(0),
            _ => json!(" "),
        };
        let mut responses = invoice_search_setup();
        responses.push(Response::json(json!({"d":[entry]})));
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        let mut args = invoice_search_args("payable");
        args.query = Some("missing".into());
        assert!(backend.search_invoices(args).await.is_err());
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_search_rejects_duplicate_entries_bad_account_and_failed_pagination_without_partial_results()
 {
    let header = invoice_header("receivable");
    for tail in [
        Response::json(json!({"d":[header.clone()]})),
        Response {
            status: "403 Forbidden",
            location: None,
            body: "PRIVATE_FINANCIAL_TEXT".into(),
        },
    ] {
        let mut responses = invoice_search_setup();
        responses.push(Response::json(
            json!({"d":{"results":[header.clone()],"__next":"?page=2"}}),
        ));
        responses.push(tail);
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        let error = backend
            .search_invoices(invoice_search_args("receivable"))
            .await
            .err()
            .unwrap();
        assert!(!format!("{error:#}").contains("PRIVATE_FINANCIAL_TEXT"));
        assert_eq!(server.finish().len(), 4);
    }
    for account in [
        json!([]),
        json!([{"ID":COMPANY_ID,"Division":43,"Code":"100"}]),
        json!([{"ID":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa","Division":42,"Code":"100"}]),
    ] {
        let server = Server::new(vec![Response::json(json!({"d":account}))]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .search_invoices(invoice_search_args("payable"))
                .await
                .is_err()
        );
        assert_eq!(server.finish().len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoice_search_empty_or_exhausted_pages_preserve_counts_and_do_not_imply_nonpayment() {
    for offset in [0, 10] {
        let mut responses = invoice_search_setup();
        responses.push(Response::json(
            json!({"d":if offset==0 {json!([])} else {json!([invoice_header("payable")])}}),
        ));
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        let mut args = invoice_search_args("payable");
        args.offset = offset;
        let result = serde_json::to_value(backend.search_invoices(args).await.unwrap()).unwrap();
        assert_eq!(result["invoices"], json!([]));
        assert_eq!(result["matched_entry_count"], usize::from(offset > 0));
        assert_eq!(result["has_more"], false);
        assert!(result["next_offset"].is_null());
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_invoice_entry_uuid_disambiguates_lookup_and_must_match_the_returned_header() {
    let server = Server::new(vec![
        Response::json(json!({"d":[invoice_header("payable")]})),
        division_currency_response(),
        Response::json(json!({"d":[]})),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let mut args = invoice_args("payable");
    args.entry_id = Some(INVOICE_ENTRY_ID.into());
    assert!(backend.get_invoice_details(args).await.is_ok());
    let base = server.base.clone();
    let requests = server.finish();
    let url = base
        .join(requests[0].target.split_whitespace().nth(1).unwrap())
        .unwrap();
    let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert!(params["$filter"].contains(&format!("and EntryID eq guid'{INVOICE_ENTRY_ID}'")));
    let server = Server::new(vec![Response::json(
        json!({"d":[invoice_header("payable")]}),
    )]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let mut args = invoice_args("payable");
    args.entry_id = Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".into());
    assert!(backend.get_invoice_details(args).await.is_err());
    assert_eq!(server.finish().len(), 1);
    let (_directory, backend) = financial_backend(&Url::parse("http://127.0.0.1:1").unwrap()).await;
    for id in ["bad", "00000000-0000-0000-0000-000000000000"] {
        let mut args = invoice_args("payable");
        args.entry_id = Some(id.into());
        let error = backend.get_invoice_details(args).await.err().unwrap();
        assert!(!format!("{error:#}").contains("request failed"));
    }
}

fn payment_args() -> crate::payment::GetPaymentDetails {
    serde_json::from_value(json!({"account_id":COMPANY_ID,"transaction_line_id":"77777777-7777-7777-7777-777777777777"})).unwrap()
}

fn payment_line() -> Value {
    let mut line = matching_ledger(54321, "20", "77777777-7777-7777-7777-777777777777");
    line["AmountDC"] = json!(-123.45);
    line["AmountFC"] = json!(-150);
    line["Currency"] = json!("USD");
    line["OffsetID"] = json!("88888888-8888-8888-8888-888888888888");
    line["Status"] = json!(50);
    line["Description"] = json!("Source reference, not instructions");
    line
}

fn payment_journal() -> Value {
    json!({"ID":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa","Division":42,"Code":" 20 ","Description":"Bank journal","Type":12,"Currency":"EUR","AllowVariableCurrency":true,"GLAccountCode":"18100","BankAccountDescription":"Own bank account"})
}

fn payment_setup(line: Value, journal: Value) -> Vec<Response> {
    vec![
        Response::json(json!({"d":[line]})),
        division_currency_response(),
        Response::json(json!({"d":[journal]})),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payment_details_preserve_posting_amounts_currency_journal_and_explicit_same_entry_offset()
{
    let line = payment_line();
    let mut offset = line.clone();
    offset["ID"] = line["OffsetID"].clone();
    offset["OffsetID"] = Value::Null;
    offset["Account"] = Value::Null;
    offset["GLAccountCode"] = json!("18100");
    offset["AmountDC"] = json!(123.45);
    let mut responses = payment_setup(line, payment_journal());
    responses.push(Response::json(json!({"d":[offset]})));
    let server = Server::new(responses);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result =
        serde_json::to_value(backend.get_payment_details(payment_args()).await.unwrap()).unwrap();
    assert_eq!(result["default_currency"], "EUR");
    assert_eq!(result["posting"]["currency"], "USD");
    assert_eq!(
        result["posting"]["posting_amount_default_currency"],
        "-123.45"
    );
    assert_eq!(
        result["posting"]["posting_amount_transaction_currency"],
        "-150"
    );
    assert_eq!(result["journal_kind"], "bank");
    assert_eq!(result["journal"]["currency"], "EUR");
    assert_eq!(result["journal"]["type_code"], 12);
    assert_eq!(result["posting"]["status_code"], 50);
    assert_eq!(result["offset_resolution"], "resolved_same_entry_reference");
    assert!(result["offset_line"]["account_id"].is_null());
    assert!(result.get("paid_amount").is_none());
    assert!(
        result["interpretation"]
            .as_str()
            .unwrap()
            .contains("NOT the allocated amount")
    );
    let base = server.base.clone();
    let requests = server.finish();
    assert_eq!(requests.len(), 4);
    for (index, request) in requests.iter().enumerate() {
        assert!(request.target.starts_with("GET ") && request.body.is_empty());
        let url = base
            .join(request.target.split_whitespace().nth(1).unwrap())
            .unwrap();
        let p: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert!(!p["$select"].contains("IBAN"));
        assert!(!p["$select"].contains("BankAccountIncludingMask"));
        if index == 0 {
            assert_eq!(
                p["$filter"],
                format!(
                    "ID eq guid'77777777-7777-7777-7777-777777777777' and Account eq guid'{COMPANY_ID}'"
                )
            );
        }
        if index == 2 {
            assert_eq!(p["$filter"], "trim(Code) eq '20'");
        }
        if index == 3 {
            assert_eq!(
                p["$filter"],
                "ID eq guid'88888888-8888-8888-8888-888888888888' and EntryID eq guid'99999999-9999-9999-9999-999999999999'"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payment_journal_kind_is_source_typed_and_missing_offsets_are_not_settlement_claims() {
    for (kind, label) in [
        (10, "cash"),
        (12, "bank"),
        (16, "payment_service"),
        (90, "other"),
        (999, "other"),
    ] {
        let mut line = payment_line();
        line["OffsetID"] = Value::Null;
        line["AmountFC"] = Value::Null;
        line["Currency"] = Value::Null;
        line["Date"] = Value::Null;
        let mut journal = payment_journal();
        journal["Type"] = json!(kind);
        journal["Currency"] = Value::Null;
        let server = Server::new(payment_setup(line, journal));
        let (_directory, backend) = financial_backend(&server.base).await;
        let result =
            serde_json::to_value(backend.get_payment_details(payment_args()).await.unwrap())
                .unwrap();
        assert_eq!(result["journal_kind"], label);
        assert_eq!(result["offset_resolution"], "not_reported");
        assert!(result["offset_line"].is_null());
        assert!(result["posting"]["currency"].is_null());
        assert!(result["posting"]["posting_amount_transaction_currency"].is_null());
        assert!(result["posting"]["posting_date"].is_null());
        assert_eq!(server.finish().len(), 3);
    }
    let mut responses = payment_setup(payment_line(), payment_journal());
    responses.push(Response::json(json!({"d":[]})));
    let server = Server::new(responses);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result =
        serde_json::to_value(backend.get_payment_details(payment_args()).await.unwrap()).unwrap();
    assert_eq!(result["offset_resolution"], "not_found");
    assert!(result["offset_line"].is_null());
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payment_details_reject_invalid_inputs_and_wrong_or_non_cashflow_selected_lines() {
    let (_directory, backend) = financial_backend(&Url::parse("http://127.0.0.1:1").unwrap()).await;
    for (account, line) in [
        ("bad", INVOICE_LINE_ID),
        (COMPANY_ID, "bad"),
        ("00000000-0000-0000-0000-000000000000", INVOICE_LINE_ID),
        (COMPANY_ID, "00000000-0000-0000-0000-000000000000"),
    ] {
        let args = serde_json::from_value(json!({"account_id":account,"transaction_line_id":line}))
            .unwrap();
        let error = backend.get_payment_details(args).await.err().unwrap();
        assert!(!format!("{error:#}").contains("request failed"));
    }
    for field in [
        "ID",
        "Account",
        "Division",
        "Type",
        "EntryID",
        "EntryNumber",
        "FinancialYear",
        "FinancialPeriod",
        "JournalCode",
        "OffsetID",
    ] {
        let mut row = payment_line();
        row[field] = match field {
            "ID" => json!(INVOICE_LINE_ID),
            "Account" => json!("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            "Division" => json!(43),
            "Type" => json!(20),
            "EntryID" => json!(uuid::Uuid::nil().to_string()),
            "EntryNumber" => json!(0),
            "FinancialYear" => json!(0),
            "FinancialPeriod" => json!(-1),
            "JournalCode" => json!(" "),
            _ => row["ID"].clone(),
        };
        let server = Server::new(vec![Response::json(json!({"d":[row]}))]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(backend.get_payment_details(payment_args()).await.is_err());
        assert_eq!(server.finish().len(), 1);
    }
    for rows in [json!([]), json!([payment_line(), payment_line()])] {
        let server = Server::new(vec![Response::json(json!({"d":rows}))]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(backend.get_payment_details(payment_args()).await.is_err());
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payment_details_reject_wrong_or_ambiguous_journals_and_out_of_entry_offset_references() {
    for field in ["Division", "Code", "ID"] {
        let mut journal = payment_journal();
        journal[field] = match field {
            "Division" => json!(43),
            "Code" => json!("30"),
            _ => json!(uuid::Uuid::nil().to_string()),
        };
        let server = Server::new(payment_setup(payment_line(), journal));
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(backend.get_payment_details(payment_args()).await.is_err());
        server.finish();
    }
    for rows in [json!([]), json!([payment_journal(), payment_journal()])] {
        let mut responses = payment_setup(payment_line(), payment_journal());
        responses[2] = Response::json(json!({"d":rows}));
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(backend.get_payment_details(payment_args()).await.is_err());
        server.finish();
    }
    for field in [
        "ID",
        "EntryID",
        "Division",
        "EntryNumber",
        "FinancialYear",
        "FinancialPeriod",
        "JournalCode",
    ] {
        let mut offset = payment_line();
        offset["ID"] = offset["OffsetID"].clone();
        offset["OffsetID"] = Value::Null;
        offset[field] = match field {
            "ID" => json!(INVOICE_LINE_ID),
            "EntryID" => json!(INVOICE_ENTRY_ID),
            "Division" => json!(43),
            "EntryNumber" => json!(54322),
            "FinancialYear" => json!(2024),
            "FinancialPeriod" => json!(11),
            _ => json!("30"),
        };
        let mut responses = payment_setup(payment_line(), payment_journal());
        responses.push(Response::json(json!({"d":[offset]})));
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(backend.get_payment_details(payment_args()).await.is_err());
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payment_details_fail_source_errors_without_partial_postings_or_response_bodies() {
    for (position, status) in [
        (0, "403 Forbidden"),
        (2, "403 Forbidden"),
        (3, "429 Too Many Requests"),
        (3, "302 Found"),
    ] {
        let mut responses = payment_setup(payment_line(), payment_journal());
        responses.push(Response::json(json!({"d":[]})));
        responses.truncate(position + 1);
        responses[position] = Response {
            status,
            location: if status.starts_with("302") {
                Some("https://untrusted.invalid/".into())
            } else {
                None
            },
            body: "PRIVATE_FINANCIAL_TEXT".into(),
        };
        let retry_count = if status.starts_with("429") { 2 } else { 0 };
        responses.extend(std::iter::repeat_n(
            responses[position].clone(),
            retry_count,
        ));
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        let error = backend
            .get_payment_details(payment_args())
            .await
            .err()
            .unwrap();
        assert!(!format!("{error:#}").contains("PRIVATE_FINANCIAL_TEXT"));
        assert_eq!(server.finish().len(), position + 1 + retry_count);
    }
}

const GL_ID: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

fn gl_account() -> Value {
    json!({"ID":GL_ID,"Division":42,"Code":" 01200 ","Description":"Trade accounts","Type":20,"TypeDescription":"Accounts receivable","BalanceSide":"C","BalanceType":"B","IsBlocked":true})
}

fn ledger_args() -> crate::ledger::GetLedgerReport {
    serde_json::from_value(
        json!({"gl_account_id":GL_ID,"financial_year":2025,"period_from":0,"period_to":13}),
    )
    .unwrap()
}

fn ledger_line(id: &str, amount: Value, period: i32, status: i32) -> Value {
    let mut line = matching_ledger(12345, "70", id);
    line["GLAccount"] = json!(GL_ID);
    line["AmountDC"] = amount;
    line["FinancialPeriod"] = json!(period);
    line["Status"] = json!(status);
    line
}

fn ledger_setup() -> Vec<Response> {
    vec![
        Response::json(json!({"d":[gl_account()]})),
        division_currency_response(),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gl_account_search_preserves_codes_validates_every_page_and_includes_blocked_accounts() {
    for code_mode in [false, true] {
        let first = gl_account();
        let mut second = first.clone();
        second["ID"] = json!("cccccccc-cccc-cccc-cccc-cccccccccccc");
        second["Code"] = json!("1200");
        second["Description"] = json!("TRADE costs");
        let server = Server::new(vec![
            Response::json(json!({"d":{"results":[second],"__next":"?page=2"}})),
            Response::json(json!({"d":[first]})),
        ]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let args=serde_json::from_value(json!({"query":if code_mode {"01200"} else {"TRADE"},"match_by":if code_mode {"code"} else {"description"},"limit":1})).unwrap();
        let result = serde_json::to_value(backend.search_gl_accounts(args).await.unwrap()).unwrap();
        assert_eq!(result["accounts"][0]["code"], " 01200 ");
        assert_eq!(result["accounts"][0]["is_blocked"], true);
        assert_eq!(
            result["matched_account_count"],
            if code_mode { 1 } else { 2 }
        );
        assert_eq!(result["has_more"], !code_mode);
        assert_eq!(server.finish().len(), 2);
    }
    for bad in [
        json!({"Division":43}),
        json!({"ID":uuid::Uuid::nil().to_string()}),
        json!({"Code":" "}),
    ] {
        let mut row = gl_account();
        row.as_object_mut()
            .unwrap()
            .extend(bad.as_object().unwrap().clone());
        row["Description"] = json!("excluded");
        let server = Server::new(vec![Response::json(json!({"d":[row]}))]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .search_gl_accounts(serde_json::from_value(json!({"query":"no match"})).unwrap())
                .await
                .is_err()
        );
        server.finish();
    }
    let server = Server::new(vec![Response::json(
        json!({"d":[gl_account(),gl_account()]}),
    )]);
    let (_directory, backend) = financial_backend(&server.base).await;
    assert!(
        backend
            .search_gl_accounts(serde_json::from_value(json!({"query":"Trade"})).unwrap())
            .await
            .is_err()
    );
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ledger_totals_cover_all_pages_statuses_signs_and_currencies_before_row_pagination() {
    for (offset, summary) in [(0, false), (1, false), (0, true)] {
        let mut first = ledger_line("11111111-aaaa-aaaa-aaaa-aaaaaaaaaaaa", json!(0.1), 0, 20);
        first["Date"] = json!("2025-01-01");
        first["Currency"] = json!("USD");
        first["AmountFC"] = json!(1000);
        let mut second = ledger_line("22222222-aaaa-aaaa-aaaa-aaaaaaaaaaaa", json!(-0.1), 1, 20);
        second["Date"] = json!("2025-02-01");
        second["Currency"] = json!("GBP");
        second["AmountFC"] = json!(-9000);
        let mut third = ledger_line("33333333-aaaa-aaaa-aaaa-aaaaaaaaaaaa", json!(0.2), 13, 50);
        third["Date"] = Value::Null;
        third["Account"] = Value::Null;
        third["AmountFC"] = Value::Null;
        third["Currency"] = Value::Null;
        let mut responses = ledger_setup();
        responses.push(Response::json(
            json!({"d":{"results":[third,second],"__next":"?page=2"}}),
        ));
        responses.push(Response::json(json!({"d":[first]})));
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        let mut args = ledger_args();
        args.limit = Some(1);
        args.offset = offset;
        args.summary_only = summary;
        let result = serde_json::to_value(backend.get_ledger_report(args).await.unwrap()).unwrap();
        assert_eq!(result["totals"]["line_count"], 3);
        assert_eq!(result["totals"]["net_movement_default_currency"], "0.2");
        assert_eq!(
            result["totals"]["positive_movement_default_currency"],
            "0.3"
        );
        assert_eq!(
            result["totals"]["negative_movement_default_currency"],
            "-0.1"
        );
        assert_eq!(
            result["totals_by_status"]["20"]["net_movement_default_currency"],
            "0.0"
        );
        assert_eq!(result["totals_by_status"]["20"]["line_count"], 2);
        assert_eq!(
            result["totals_by_status"]["50"]["net_movement_default_currency"],
            "0.2"
        );
        assert_eq!(result["default_currency"], "EUR");
        assert!(result.get("opening_balance").is_none());
        if summary {
            assert!(result.get("transactions").is_none());
            assert_eq!(result["has_more"], false);
        } else {
            assert_eq!(
                result["transactions"][0]["id"],
                if offset == 0 {
                    "11111111-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
                } else {
                    "22222222-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
                }
            );
            assert_eq!(result["has_more"], true);
            assert_eq!(result["next_offset"], offset + 1);
        }
        let base = server.base.clone();
        let requests = server.finish();
        let url = base
            .join(requests[2].target.split_whitespace().nth(1).unwrap())
            .unwrap();
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(
            params["$filter"],
            format!(
                "GLAccount eq guid'{GL_ID}' and FinancialYear eq 2025 and FinancialPeriod ge 0 and FinancialPeriod le 13"
            )
        );
        assert!(
            requests
                .iter()
                .all(|r| r.target.starts_with("GET ") && r.body.is_empty())
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ledger_explicit_status_and_empty_or_zero_net_selections_remain_explicit() {
    for count in [0, 2] {
        let mut responses = ledger_setup();
        let rows = if count == 0 {
            json!([])
        } else {
            json!([
                ledger_line("11111111-aaaa-aaaa-aaaa-aaaaaaaaaaaa", json!(12), 1, 50),
                ledger_line("22222222-aaaa-aaaa-aaaa-aaaaaaaaaaaa", json!(-12), 1, 50)
            ])
        };
        responses.push(Response::json(json!({"d":rows})));
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        let mut args = ledger_args();
        args.status = Some(50);
        let result = serde_json::to_value(backend.get_ledger_report(args).await.unwrap()).unwrap();
        assert_eq!(result["totals"]["net_movement_default_currency"], "0");
        assert_eq!(result["totals"]["line_count"], count);
        assert_eq!(result["status_filter"], 50);
        assert_eq!(result["has_more"], false);
        let requests = server.finish();
        assert!(requests[2].target.contains("Status+eq+50"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ledger_inputs_are_validated_before_network_and_source_scope_is_enforced() {
    let (_directory, backend) = financial_backend(&Url::parse("http://127.0.0.1:1").unwrap()).await;
    for fields in [
        json!({"gl_account_id":"bad"}),
        json!({"gl_account_id":uuid::Uuid::nil().to_string()}),
        json!({"financial_year":0}),
        json!({"period_from":-1}),
        json!({"period_from":14,"period_to":13}),
        json!({"period_to":32768}),
        json!({"status":5}),
        json!({"limit":0}),
        json!({"limit":201}),
        json!({"offset":u32::MAX}),
    ] {
        let mut args =
            json!({"gl_account_id":GL_ID,"financial_year":2025,"period_from":0,"period_to":13});
        args.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let error = backend
            .get_ledger_report(serde_json::from_value(args).unwrap())
            .await
            .err()
            .unwrap();
        assert!(!format!("{error:#}").contains("request failed"));
    }
    for fields in [
        json!({"query":" "}),
        json!({"query":"x".repeat(101)}),
        json!({"query":"X","limit":0}),
        json!({"query":"X","limit":51}),
        json!({"query":"X","offset":u32::MAX}),
    ] {
        assert!(
            backend
                .search_gl_accounts(serde_json::from_value(fields).unwrap())
                .await
                .is_err()
        );
    }
    for (field, value) in [
        ("Division", json!(43)),
        ("GLAccount", json!(COMPANY_ID)),
        ("GLAccountCode", json!("other")),
        ("FinancialYear", json!(2024)),
        ("FinancialPeriod", json!(-1)),
        ("FinancialPeriod", json!(14)),
        ("Status", json!(20)),
        ("ID", json!(uuid::Uuid::nil().to_string())),
        ("EntryID", json!(uuid::Uuid::nil().to_string())),
        ("EntryNumber", json!(0)),
        ("EntryNumber", json!(i64::from(i32::MAX) + 1)),
        ("JournalCode", json!(" ")),
        ("JournalCode", json!("x".repeat(101))),
    ] {
        let mut row = ledger_line("11111111-aaaa-aaaa-aaaa-aaaaaaaaaaaa", json!(1), 1, 50);
        row[field] = value;
        let mut responses = ledger_setup();
        responses.push(Response::json(json!({"d":[row]})));
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        let mut args = ledger_args();
        args.status = Some(50);
        assert!(backend.get_ledger_report(args).await.is_err());
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ledger_missing_amounts_duplicates_failed_paging_and_decimal_overflow_return_no_totals() {
    let row = ledger_line("11111111-aaaa-aaaa-aaaa-aaaaaaaaaaaa", json!(1), 1, 20);
    for tail in [
        Response::json(json!({"d":[row.clone()]})),
        Response {
            status: "403 Forbidden",
            location: None,
            body: "PRIVATE_FINANCIAL_TEXT".into(),
        },
    ] {
        let mut responses = ledger_setup();
        responses.push(Response::json(
            json!({"d":{"results":[row.clone()],"__next":"?page=2"}}),
        ));
        responses.push(tail);
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        let error = backend
            .get_ledger_report(ledger_args())
            .await
            .err()
            .unwrap();
        assert!(!format!("{error:#}").contains("PRIVATE_FINANCIAL_TEXT"));
        server.finish();
    }
    let max: Value = serde_json::from_str("79228162514264337593543950335").unwrap();
    for rows in [
        json!([ledger_line(
            "11111111-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            Value::Null,
            1,
            20
        )]),
        json!([
            ledger_line("11111111-aaaa-aaaa-aaaa-aaaaaaaaaaaa", max, 1, 20),
            ledger_line("22222222-aaaa-aaaa-aaaa-aaaaaaaaaaaa", json!(1), 1, 20)
        ]),
    ] {
        let mut responses = ledger_setup();
        responses.push(Response::json(json!({"d":rows})));
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(backend.get_ledger_report(ledger_args()).await.is_err());
        server.finish();
    }
    for accounts in [
        json!([]),
        json!([gl_account(), gl_account()]),
        json!([{"ID":COMPANY_ID,"Division":42,"Code":"01200"}]),
    ] {
        let server = Server::new(vec![Response::json(json!({"d":accounts}))]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(backend.get_ledger_report(ledger_args()).await.is_err());
        assert_eq!(server.finish().len(), 1);
    }
}

fn paginated_test_rows(rows: Vec<Value>, page_size: usize) -> Vec<Response> {
    let page_count = rows.len().div_ceil(page_size);
    rows.chunks(page_size)
        .enumerate()
        .map(|(index, page)| {
            let mut envelope = json!({"results": page});
            if index + 1 < page_count {
                envelope["__next"] = json!(format!("?page={}", index + 2));
            }
            Response::json(json!({"d": envelope}))
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_outstanding_summaries_keep_all_page_totals_and_aging() {
    const COUNT: usize = 10_000;
    for administration_wide in [false, true] {
        let rows = (0..COUNT)
            .map(|index| {
                if administration_wide {
                    balance_item(
                        index as i64,
                        COMPANY_ID,
                        "1001",
                        "Company X",
                        "EUR",
                        json!(0.01),
                    )
                } else {
                    open_item(index as i64, "EUR", json!(0.01))
                }
            })
            .collect();
        let mut responses = Vec::new();
        if !administration_wide {
            responses.push(company_response());
        }
        responses.extend(paginated_test_rows(rows, 500));
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        let result = if administration_wide {
            serde_json::to_value(
                backend
                    .get_outstanding_balances(
                        serde_json::from_value(json!({"side":"payable","limit":1})).unwrap(),
                    )
                    .await
                    .unwrap(),
            )
            .unwrap()
        } else {
            let mut args = open_items_args();
            args.summary_only = true;
            serde_json::to_value(backend.get_open_items(args).await.unwrap()).unwrap()
        };
        assert_eq!(result["source_item_count"], COUNT);
        assert_eq!(result["matched_item_count"], COUNT);
        assert_eq!(
            result["totals_by_currency"]["EUR"]["net_open_amount"],
            "100.00"
        );
        assert_eq!(
            result["totals_by_currency"]["EUR"]["aging_by_due_date"]["overdue_over_90_days"]["item_count"],
            COUNT
        );
        assert!(result.get("items").is_none());
        assert_eq!(
            server.finish().len(),
            20 + usize::from(!administration_wide)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_ledger_report_keeps_complete_totals_with_small_late_sorted_pages() {
    const COUNT: usize = 10_000;
    for summary in [false, true] {
        let rows = (1..=COUNT)
            .rev()
            .map(|index| {
                let id = uuid::Uuid::from_u128(index as u128).to_string();
                let mut row = ledger_line(&id, json!(0.01), 1, 50);
                row["Date"] = json!("2025-01-01");
                row["LineNumber"] = json!(index);
                row
            })
            .collect();
        let mut responses = ledger_setup();
        responses.extend(paginated_test_rows(rows, 500));
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        let mut args = ledger_args();
        args.limit = Some(3);
        args.offset = 100;
        args.summary_only = summary;
        let result = serde_json::to_value(backend.get_ledger_report(args).await.unwrap()).unwrap();
        assert_eq!(result["totals"]["line_count"], COUNT);
        assert_eq!(result["totals"]["net_movement_default_currency"], "100.00");
        assert_eq!(result["totals_by_status"]["50"]["line_count"], COUNT);
        if summary {
            assert!(result.get("transactions").is_none());
        } else {
            assert_eq!(result["transactions"].as_array().unwrap().len(), 3);
            assert_eq!(result["transactions"][0]["line_number"], 101);
            assert_eq!(result["transactions"][2]["line_number"], 103);
            assert_eq!(result["has_more"], true);
            assert_eq!(result["next_offset"], 103);
        }
        assert_eq!(server.finish().len(), 22);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_invoice_search_counts_all_pages_and_retains_correct_latest_window() {
    const COUNT: usize = 10_000;
    let rows = (1..=COUNT)
        .map(|index| {
            let mut row = invoice_header("payable");
            row["EntryID"] = json!(uuid::Uuid::from_u128(index as u128).to_string());
            row["EntryNumber"] = json!(index);
            row["EntryDate"] = json!("2025-01-01");
            row
        })
        .collect();
    let mut responses = invoice_search_setup();
    responses.extend(paginated_test_rows(rows, 500));
    let server = Server::new(responses);
    let (_directory, backend) = financial_backend(&server.base).await;
    let mut args = invoice_search_args("payable");
    args.limit = Some(3);
    args.offset = 100;
    let result = serde_json::to_value(backend.search_invoices(args).await.unwrap()).unwrap();
    assert_eq!(result["source_entry_count"], COUNT);
    assert_eq!(result["matched_entry_count"], COUNT);
    assert_eq!(result["invoices"].as_array().unwrap().len(), 3);
    assert_eq!(result["invoices"][0]["entry_number"], COUNT - 100);
    assert_eq!(result["invoices"][2]["entry_number"], COUNT - 102);
    assert_eq!(result["next_offset"], 103);
    assert_eq!(server.finish().len(), 22);
}

fn bank_configuration(value: u128) -> crate::store::BankConfiguration {
    crate::store::BankConfiguration {
        unassigned_gl_account_ids: vec![uuid::Uuid::from_u128(value)],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_configuration_storage_scopes_replacement_and_clear_without_touching_tokens() {
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    let initial = session(&Url::parse("http://127.0.0.1:1").unwrap(), false);
    store.save(&initial).await.unwrap();
    let session_path = directory.path().join("session.json");
    let session_bytes = fs::read(&session_path).unwrap();
    let config_path = directory.path().join("bank-config.json");
    assert!(store.bank_configuration(&initial).await.unwrap().is_none());
    assert!(!store.clear_bank_configuration(&initial).await.unwrap());
    assert!(!config_path.exists());
    let mut other_administration = initial.clone();
    other_administration.administration.as_mut().unwrap().code = 43;
    let mut other_origin = initial.clone();
    other_origin.base_url = "https://other.example/".into();
    let mut other_app = initial.clone();
    other_app.client_id = "other-client".into();
    for (index, scope) in [&initial, &other_administration, &other_origin, &other_app]
        .into_iter()
        .enumerate()
    {
        store
            .save_bank_configuration(scope, bank_configuration(index as u128 + 1))
            .await
            .unwrap();
    }
    for (index, scope) in [&initial, &other_administration, &other_origin, &other_app]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            store.bank_configuration(scope).await.unwrap(),
            Some(bank_configuration(index as u128 + 1))
        );
    }
    store
        .save_bank_configuration(&initial, bank_configuration(5))
        .await
        .unwrap();
    assert_eq!(
        store.bank_configuration(&initial).await.unwrap(),
        Some(bank_configuration(5))
    );
    assert!(
        store
            .clear_bank_configuration(&other_administration)
            .await
            .unwrap()
    );
    assert!(
        !store
            .clear_bank_configuration(&other_administration)
            .await
            .unwrap()
    );
    assert_eq!(
        store.bank_configuration(&other_origin).await.unwrap(),
        Some(bank_configuration(3))
    );
    assert_eq!(
        store.bank_configuration(&other_app).await.unwrap(),
        Some(bank_configuration(4))
    );
    assert_eq!(fs::read(session_path).unwrap(), session_bytes);
    let json = fs::read_to_string(&config_path).unwrap();
    assert!(!json.contains("access_token"));
    assert!(!json.contains("refresh_token"));
    assert_eq!(
        serde_json::from_str::<Value>(&json)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        3
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(config_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_bank_configuration_never_overwrites_existing_settings() {
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    let scope = session(&Url::parse("http://127.0.0.1:1").unwrap(), false);
    store
        .save_bank_configuration(&scope, bank_configuration(1))
        .await
        .unwrap();
    let path = directory.path().join("bank-config.json");
    let original = fs::read(&path).unwrap();
    for ids in [
        vec![],
        vec![uuid::Uuid::nil()],
        vec![uuid::Uuid::from_u128(1); 2],
        (1..=11).map(uuid::Uuid::from_u128).collect(),
    ] {
        assert!(
            store
                .save_bank_configuration(
                    &scope,
                    crate::store::BankConfiguration {
                        unassigned_gl_account_ids: ids
                    }
                )
                .await
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), original);
    }
    let mut unselected = scope.clone();
    unselected.administration = None;
    assert!(store.bank_configuration(&unselected).await.is_err());
    assert!(
        store
            .save_bank_configuration(&unselected, bank_configuration(2))
            .await
            .is_err()
    );
    assert!(store.clear_bank_configuration(&unselected).await.is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
    let entry: Value = serde_json::from_slice(&original).unwrap();
    for contents in [
        "PRIVATE_LOCAL_SECRET".to_owned(),
        json!([entry[0], entry[0]]).to_string(),
        json!([{"scope":entry[0]["scope"],"configuration":{"unassigned_gl_account_ids":[]}}])
            .to_string(),
    ] {
        fs::write(&path, &contents).unwrap();
        let error = store.bank_configuration(&scope).await.unwrap_err();
        assert!(!format!("{error:#}").contains("PRIVATE_LOCAL_SECRET"));
        assert!(
            store
                .save_bank_configuration(&scope, bank_configuration(3))
                .await
                .is_err()
        );
        assert!(store.clear_bank_configuration(&scope).await.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_configuration_rejects_symlinks_and_public_permissions() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    let scope = session(&Url::parse("http://127.0.0.1:1").unwrap(), false);
    let path = directory.path().join("bank-config.json");
    let target = directory.path().join("private-target.json");
    fs::write(&target, "[]\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&target, &path).unwrap();
    assert!(store.bank_configuration(&scope).await.is_err());
    assert!(
        store
            .save_bank_configuration(&scope, bank_configuration(1))
            .await
            .is_err()
    );
    assert!(store.clear_bank_configuration(&scope).await.is_err());
    assert!(fs::symlink_metadata(&path).unwrap().is_symlink());
    assert_eq!(fs::read_to_string(&target).unwrap(), "[]\n");
    fs::remove_file(&path).unwrap();
    fs::write(&path, "[]\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.bank_configuration(&scope).await.is_err());
    assert!(
        store
            .save_bank_configuration(&scope, bank_configuration(1))
            .await
            .is_err()
    );
    assert!(store.clear_bank_configuration(&scope).await.is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), "[]\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_configuration_client_requires_pinned_scope_and_preserves_rotated_tokens() {
    let directory = tempdir().unwrap();
    let initial = session(&Url::parse("http://127.0.0.1:1").unwrap(), false);
    {
        let store = Store::open(directory.path()).await.unwrap();
        store.save(&initial).await.unwrap();
    }
    let mut exact = exact::ExactClient::new(directory.path().into(), client(), None)
        .await
        .unwrap();
    assert!(exact.bank_configuration().await.is_err());
    assert!(
        exact
            .save_bank_configuration(bank_configuration(1))
            .await
            .is_err()
    );
    assert!(exact.clear_bank_configuration().await.is_err());
    exact.pin_administration().unwrap();
    let mut rotated = initial.clone();
    rotated.tokens.access_token = "rotated-private-access".into();
    rotated.tokens.refresh_token = "rotated-private-refresh".into();
    {
        let store = Store::open(directory.path()).await.unwrap();
        store.save(&rotated).await.unwrap();
    }
    let session_path = directory.path().join("session.json");
    let rotated_bytes = fs::read(&session_path).unwrap();
    exact
        .save_bank_configuration(bank_configuration(1))
        .await
        .unwrap();
    assert_eq!(fs::read(&session_path).unwrap(), rotated_bytes);
    let configuration_path = directory.path().join("bank-config.json");
    let saved = fs::read(&configuration_path).unwrap();
    for field in ["administration", "base_url", "client_id"] {
        let mut changed = rotated.clone();
        match field {
            "administration" => changed.administration.as_mut().unwrap().code = 43,
            "base_url" => changed.base_url = "https://other.example/".into(),
            _ => changed.client_id = "other-client".into(),
        }
        {
            let store = Store::open(directory.path()).await.unwrap();
            store.save(&changed).await.unwrap();
        }
        assert!(exact.bank_configuration().await.is_err());
        assert!(
            exact
                .save_bank_configuration(bank_configuration(2))
                .await
                .is_err()
        );
        assert!(exact.clear_bank_configuration().await.is_err());
        assert_eq!(fs::read(&configuration_path).unwrap(), saved);
    }
    {
        let store = Store::open(directory.path()).await.unwrap();
        store.save(&rotated).await.unwrap();
    }
    assert_eq!(
        exact.bank_configuration().await.unwrap(),
        Some(bank_configuration(1))
    );
    assert!(exact.clear_bank_configuration().await.unwrap());
    assert_eq!(fs::read(&session_path).unwrap(), rotated_bytes);
    exact.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_bank_configuration_updates_preserve_other_scopes() {
    let directory = tempdir().unwrap();
    let first = session(&Url::parse("http://127.0.0.1:1").unwrap(), false);
    let mut second = first.clone();
    second.administration.as_mut().unwrap().code = 43;
    let left_path = directory.path().to_owned();
    let right_path = left_path.clone();
    let left = tokio::spawn(async move {
        let store = Store::open(&left_path).await.unwrap();
        store
            .save_bank_configuration(&first, bank_configuration(1))
            .await
            .unwrap();
    });
    let right = tokio::spawn(async move {
        let store = Store::open(&right_path).await.unwrap();
        store
            .save_bank_configuration(&second, bank_configuration(2))
            .await
            .unwrap();
    });
    left.await.unwrap();
    right.await.unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    let first = session(&Url::parse("http://127.0.0.1:1").unwrap(), false);
    let mut second = first.clone();
    second.administration.as_mut().unwrap().code = 43;
    assert_eq!(
        store.bank_configuration(&first).await.unwrap(),
        Some(bank_configuration(1))
    );
    assert_eq!(
        store.bank_configuration(&second).await.unwrap(),
        Some(bank_configuration(2))
    );
}

#[path = "master_data/tests.rs"]
mod master_data_tests;

#[path = "master_data/items_tests.rs"]
mod item_context_tests;

#[path = "purchase_documents/tests.rs"]
mod purchase_document_tests;

#[path = "purchase_documents/duplicates_tests.rs"]
mod purchase_duplicate_tests;

#[path = "master_data/preflight_tests.rs"]
mod purchase_preflight_tests;

#[path = "bank/proposal_tests.rs"]
mod bank_proposal_tests;

#[path = "bank/xml_preview_tests.rs"]
mod bank_xml_preview_tests;
