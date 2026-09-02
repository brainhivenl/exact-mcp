use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Barrier};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use reqwest::Url;
use reqwest::blocking::Client;
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

fn tls_configs() -> (Arc<ServerConfig>, Arc<ClientConfig>) {
    let directory = tempdir().unwrap();
    let generated = rcgen::generate_simple_self_signed(vec!["exact-mcp.localhost".into()]).unwrap();
    let cert_path = directory.path().join("cert.pem");
    let key_path = directory.path().join("key.pem");
    std::fs::write(&cert_path, generated.cert.pem()).unwrap();
    std::fs::write(&key_path, generated.signing_key.serialize_pem()).unwrap();
    let server = oauth::load_tls_config(&cert_path, &key_path).unwrap();
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

#[test]
fn local_callback_ignores_noise_and_returns_only_a_valid_code() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let redirect = Url::parse(&format!(
        "https://exact-mcp.localhost:{}/callback",
        address.port()
    ))
    .unwrap();
    let (server_config, client_config) = tls_configs();
    let handle = thread::spawn(move || {
        oauth::wait_for_code(
            &listener,
            &server_config,
            &redirect,
            "expected",
            Duration::from_secs(3),
        )
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
    assert_eq!(handle.join().unwrap().unwrap(), "good");
}

#[test]
fn local_callback_waits_for_complete_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let redirect = Url::parse(&format!(
        "https://exact-mcp.localhost:{}/callback",
        address.port()
    ))
    .unwrap();
    let (server_config, client_config) = tls_configs();
    let handle = thread::spawn(move || {
        oauth::wait_for_code(
            &listener,
            &server_config,
            &redirect,
            "expected",
            Duration::from_secs(3),
        )
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
    assert_eq!(handle.join().unwrap().unwrap(), "good");
}

#[test]
fn self_signed_callback_works_with_an_explicit_local_certificate_exception() {
    let redirect =
        oauth::validate_redirect_uri("https://exact-mcp.localhost:8400/callback").unwrap();
    let tls_config = oauth::self_signed_tls_config(&redirect).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let redirect = Url::parse(&format!(
        "https://exact-mcp.localhost:{}/callback",
        address.port()
    ))
    .unwrap();
    let callback = redirect.join("?state=expected&code=test-code").unwrap();
    let handle = thread::spawn(move || {
        oauth::wait_for_code(
            &listener,
            &tls_config,
            &redirect,
            "expected",
            Duration::from_secs(5),
        )
    });
    let strict = Client::builder()
        .no_proxy()
        .resolve("exact-mcp.localhost", address)
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    assert!(strict.get(callback.clone()).send().is_err());
    let local_exception = Client::builder()
        .no_proxy()
        .resolve("exact-mcp.localhost", address)
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let response = local_exception.get(callback).send().unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(handle.join().unwrap().unwrap(), "test-code");
}

#[test]
fn tls_config_rejects_missing_malformed_and_mismatched_keys() {
    let directory = tempdir().unwrap();
    let cert_path = directory.path().join("cert.pem");
    let key_path = directory.path().join("key.pem");
    assert!(oauth::load_tls_config(&cert_path, &key_path).is_err());
    std::fs::write(&cert_path, "not a certificate").unwrap();
    std::fs::write(&key_path, "private-key-content").unwrap();
    let error = oauth::load_tls_config(&cert_path, &key_path).err().unwrap();
    assert!(!format!("{error:#}").contains("private-key-content"));
    let first = rcgen::generate_simple_self_signed(vec!["exact-mcp.localhost".into()]).unwrap();
    let second = rcgen::generate_simple_self_signed(vec!["exact-mcp.localhost".into()]).unwrap();
    std::fs::write(&cert_path, first.cert.pem()).unwrap();
    std::fs::write(&key_path, second.signing_key.serialize_pem()).unwrap();
    assert!(oauth::load_tls_config(&cert_path, &key_path).is_err());
}

#[test]
fn tls_callback_handles_authorization_denial() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let redirect = Url::parse(&format!(
        "https://exact-mcp.localhost:{}/callback",
        address.port()
    ))
    .unwrap();
    let (server_config, client_config) = tls_configs();
    let handle = thread::spawn(move || {
        oauth::wait_for_code(
            &listener,
            &server_config,
            &redirect,
            "expected",
            Duration::from_secs(3),
        )
    });
    let mut stream = tls_stream(address, client_config);
    stream.write_all(b"GET /callback?state=expected&error=access_denied HTTP/1.1\r\nHost: exact-mcp.localhost\r\n\r\n").unwrap();
    stream.flush().unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
    assert!(
        handle
            .join()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("denied")
    );
}

#[test]
fn callback_times_out_without_requests() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let redirect = Url::parse("https://exact-mcp.localhost:8400/callback").unwrap();
    let (server_config, _) = tls_configs();
    let error = oauth::wait_for_code(
        &listener,
        &server_config,
        &redirect,
        "state",
        Duration::from_millis(25),
    )
    .unwrap_err();
    assert!(error.to_string().contains("Timed out"));
}

#[test]
fn code_exchange_sends_form_and_accepts_numeric_or_string_expiry() {
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

#[test]
fn malformed_token_responses_and_server_errors_do_not_leak_secrets() {
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
        );
        let error = format!("{:#}", error.err().expect("Expected token failure"));
        for secret in ["client-secret", "access-token", "refresh-token"] {
            assert!(!error.contains(secret), "{error}");
        }
        assert_eq!(server.finish().len(), 1);
    }
}

#[test]
fn storage_round_trips_and_atomically_replaces_tokens_and_selection() {
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    assert!(store.load().unwrap().is_none());
    let mut session = session(&Url::parse("https://start.exactonline.nl").unwrap(), false);
    store.save(&session).unwrap();
    session.tokens.access_token = "replacement".into();
    store.save(&session).unwrap();
    let stored = store.require_session().unwrap();
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
#[test]
fn storage_rejects_public_permissions_and_symbolic_links() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = tempdir().unwrap();
    let path = root.path().join("private");
    let store = Store::open(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let session = session(&Url::parse("https://start.exactonline.nl").unwrap(), false);
    store.save(&session).unwrap();
    let file = path.join("session.json");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.load().is_err());
    std::fs::remove_file(&file).unwrap();
    symlink(root.path().join("elsewhere"), &file).unwrap();
    assert!(store.load().is_err());
    drop(store);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Store::open(&path).is_err());
}

#[test]
fn listing_handles_pagination_without_selecting_the_current_division() {
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
    let store = Store::open(directory.path()).unwrap();
    let mut session = session(&server.base, false);
    session.administration = None;
    store.save(&session).unwrap();
    let available = exact::administrations(&client(), &store, &mut session, None).unwrap();
    assert_eq!(
        available.iter().map(|item| item.code).collect::<Vec<_>>(),
        [7, 42]
    );
    assert!(store.require_session().unwrap().administration.is_none());
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

#[test]
fn selection_requires_an_accessible_code_and_preserves_previous_selection_on_failure() {
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let mut session = session(&Url::parse("https://start.exactonline.nl").unwrap(), false);
    store.save(&session).unwrap();
    let available = vec![Administration {
        code: 7,
        description: "First company".into(),
    }];
    assert!(exact::select_administration(&store, &mut session, &available, 999).is_err());
    assert_eq!(
        store
            .require_session()
            .unwrap()
            .administration
            .unwrap()
            .code,
        42
    );
    let selected = exact::select_administration(&store, &mut session, &available, 7).unwrap();
    assert_eq!(selected.code, 7);
    assert_eq!(
        store
            .require_session()
            .unwrap()
            .administration
            .unwrap()
            .code,
        7
    );
}

#[test]
fn refresh_rotates_and_persists_tokens_before_api_calls() {
    let server = Server::new(vec![
        tokens_response(json!(600)),
        me_response(),
        divisions_response(),
    ]);
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let mut session = session(&server.base, true);
    store.save(&session).unwrap();
    exact::administrations(&client(), &store, &mut session, Some("client-secret")).unwrap();
    let stored = store.require_session().unwrap();
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

#[test]
fn concurrent_commands_reload_after_locking_and_refresh_only_once() {
    let server = Server::new(vec![
        tokens_response(json!(600)),
        me_response(),
        divisions_response(),
        me_response(),
        divisions_response(),
    ]);
    let directory = tempdir().unwrap();
    {
        let store = Store::open(directory.path()).unwrap();
        store.save(&session(&server.base, true)).unwrap();
    }
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let path = directory.path().to_owned();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            let store = Store::open(&path).unwrap();
            let mut session = store.require_session().unwrap();
            exact::administrations(&client(), &store, &mut session, Some("client-secret")).unwrap();
        }));
    }
    barrier.wait();
    for handle in handles {
        handle.join().unwrap();
    }
    let requests = server.finish();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target.starts_with("POST"))
            .count(),
        1
    );
    assert_eq!(requests.len(), 5);
}

#[test]
fn expired_token_without_secret_fails_without_contacting_exact() {
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let mut session = session(&Url::parse("http://127.0.0.1:1").unwrap(), true);
    store.save(&session).unwrap();
    let error = exact::administrations(&client(), &store, &mut session, None).unwrap_err();
    assert!(error.to_string().contains("EXACT_CLIENT_SECRET"));
    assert_eq!(
        store.require_session().unwrap().tokens.refresh_token,
        "old-refresh"
    );
}

#[test]
fn rejected_refresh_does_not_overwrite_the_saved_session_or_retry() {
    let server = Server::new(vec![Response {
        status: "400 Bad Request",
        body: "old-refresh client-secret invalid_grant".into(),
        location: None,
    }]);
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let mut session = session(&server.base, true);
    store.save(&session).unwrap();
    let error =
        exact::administrations(&client(), &store, &mut session, Some("client-secret")).unwrap_err();
    assert!(!format!("{error:#}").contains("old-refresh"));
    assert_eq!(
        store.require_session().unwrap().tokens.refresh_token,
        "old-refresh"
    );
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn pagination_rejects_other_origins_non_api_paths_and_cycles() {
    for next in [
        "http://127.0.0.1:1/api/v1/stolen",
        "/outside-api",
        "/api/v1/current/Me?$select=CurrentDivision",
    ] {
        let server = Server::new(vec![Response::json(json!({"d": {
            "results": [{"CurrentDivision": 7}], "__next": next
        }}))]);
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let mut session = session(&server.base, false);
        let error = exact::administrations(&client(), &store, &mut session, None).unwrap_err();
        assert!(error.to_string().contains("pagination"), "{error}");
        assert_eq!(server.finish().len(), 1);
    }
}

#[test]
fn api_errors_and_redirects_are_not_retried_or_followed() {
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
        let store = Store::open(directory.path()).unwrap();
        let mut session = session(&server.base, false);
        let error =
            exact::administrations(&client(), &store, &mut session, Some("secret")).unwrap_err();
        assert!(!format!("{error:#}").contains("old-access"));
        assert!(!format!("{error:#}").contains("financial-data"));
        assert_eq!(server.finish().len(), 1);
    }
}
