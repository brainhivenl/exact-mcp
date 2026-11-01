use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
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
    for status in [
        "401 Unauthorized",
        "403 Forbidden",
        "429 Too Many Requests",
        "302 Found",
    ] {
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
