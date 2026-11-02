use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

fn run(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_exact-mcp"))
        .current_dir(directory.parent().unwrap())
        .arg("--data-dir")
        .arg(directory)
        .args(arguments)
        .env_remove("EXACT_CLIENT_ID")
        .env_remove("EXACT_CLIENT_SECRET")
        .env_remove("EXACT_BASE_URL")
        .env_remove("EXACT_REDIRECT_URI")
        .env_remove("EXACT_TLS_CERT")
        .env_remove("EXACT_TLS_KEY")
        .output()
        .unwrap()
}

#[test]
fn help_and_version_do_not_create_session_storage() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    for arguments in [
        vec!["--help"],
        vec!["auth", "--help"],
        vec!["bank-config", "--help"],
        vec!["bank-config", "set", "--help"],
        vec!["bank-config", "show", "--help"],
        vec!["bank-config", "clear", "--help"],
        vec!["--version"],
    ] {
        let output = run(&directory, &arguments);
        assert!(output.status.success());
        assert!(!directory.exists());
    }
}

#[test]
fn fresh_status_is_local_and_not_logged_in() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    let output = run(&directory, &["auth", "status"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Not logged in."));
    assert!(stdout.contains(&directory.display().to_string()));
    assert!(!directory.join("session.json").exists());
}

#[test]
fn list_select_and_serve_require_login() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    for arguments in [
        vec!["administrations", "list"],
        vec!["administrations", "select", "42"],
        vec!["serve"],
        vec!["bank-config", "show"],
        vec!["bank-config", "show", "--json"],
        vec!["bank-config", "clear"],
        vec![
            "bank-config",
            "set",
            "--unassigned-gl-account-id",
            "00000000-0000-0000-0000-000000000010",
        ],
    ] {
        let output = run(&directory, &arguments);
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("auth login"));
        assert!(!directory.join("session.json").exists());
    }
}

#[test]
fn login_requires_credentials_before_listening_or_creating_storage() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    let output = run(&directory, &["auth", "login"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("EXACT_CLIENT_ID")
    );
    assert!(!directory.exists());
}

#[test]
fn custom_certificate_and_key_must_be_provided_together() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    for (provided, required) in [("--tls-cert", "--tls-key"), ("--tls-key", "--tls-cert")] {
        let output = run(&directory, &["auth", "login", provided, "missing.pem"]);
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(required));
        assert!(!directory.exists());
    }
}

#[test]
fn login_help_uses_the_registered_https_callback() {
    let root = TempDir::new().unwrap();
    let output = run(&root.path().join("data"), &["auth", "login", "--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: exact-mcp auth login [OPTIONS]"));
    assert!(stdout.contains("https://exact-mcp.localhost:8400/callback"));
    assert!(stdout.contains("EXACT_TLS_CERT"));
    assert!(stdout.contains("EXACT_TLS_KEY"));
}

#[test]
fn unsafe_redirect_is_rejected_before_listening() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    let output = run(
        &directory,
        &[
            "auth",
            "login",
            "--redirect-uri",
            "https://0.0.0.0:8400/callback",
            "--tls-cert",
            "missing.pem",
            "--tls-key",
            "missing-key.pem",
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("Redirect URI")
    );
    assert!(!directory.exists());
}

#[test]
fn dotenv_credentials_are_loaded_without_printing_or_executing_their_contents() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    std::fs::write(
        root.path().join(".env"),
        "EXACT_CLIENT_ID=fixture-id\nEXACT_CLIENT_SECRET='fixture-$(touch should-not-exist)'\n",
    )
    .unwrap();
    let output = run(
        &directory,
        &[
            "auth",
            "login",
            "--tls-cert",
            "missing.pem",
            "--tls-key",
            "missing-key.pem",
        ],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("TLS certificate"), "{stderr}");
    assert!(!stderr.contains("fixture-"));
    assert!(!root.path().join("should-not-exist").exists());
    assert!(!directory.exists());
}

#[test]
fn malformed_dotenv_errors_do_not_echo_secret_lines() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    std::fs::write(
        root.path().join(".env"),
        "EXACT_CLIENT_SECRET='private-value-without-closing-quote\n",
    )
    .unwrap();
    let output = run(&directory, &["auth", "status"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Cannot load .env"));
    assert!(!stderr.contains("private-value"));
}

#[test]
fn exported_environment_takes_precedence_over_dotenv() {
    let root = TempDir::new().unwrap();
    std::fs::write(
        root.path().join(".env"),
        "EXACT_CLIENT_ID=fixture-id\nEXACT_CLIENT_SECRET=fixture-secret\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_exact-mcp"))
        .current_dir(root.path())
        .args(["auth", "login"])
        .env("EXACT_CLIENT_ID", "")
        .env_remove("EXACT_CLIENT_SECRET")
        .env_remove("EXACT_BASE_URL")
        .env_remove("EXACT_REDIRECT_URI")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("EXACT_CLIENT_ID must not be empty")
    );
}

#[cfg(unix)]
#[test]
fn pi_launcher_loads_dotenv_and_registers_its_own_artifact_without_serializing_secrets() {
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new().unwrap();
    let fake = root.path().join("pi");
    std::fs::write(&fake, "#!/bin/sh\n[ \"$EXACT_CLIENT_SECRET\" = fixture-secret ] || exit 42\nprintf '%s\\n' \"$@\" >> \"$LAUNCH_TEST_LOG\"\nprintf 'END\\n' >> \"$LAUNCH_TEST_LOG\"\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        root.path().join(".env"),
        "EXACT_CLIENT_SECRET=fixture-secret\n",
    )
    .unwrap();
    let log = root.path().join("args.log");
    let path = format!(
        "{}:{}",
        root.path().display(),
        std::env::var("PATH").unwrap()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_exact-mcp"))
        .current_dir(root.path())
        .args(["pi", "--", "--name", "Launcher test"])
        .env("PATH", path)
        .env("LAUNCH_TEST_LOG", &log)
        .env_remove("EXACT_CLIENT_SECRET")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let args = std::fs::read_to_string(log).unwrap();
    let expected_binary = std::fs::canonicalize(env!("CARGO_BIN_EXE_exact-mcp")).unwrap();
    assert_eq!(
        args,
        format!(
            "mcp\nadd\nexact\n--local\n--exposure\ndirect\n--\n{}\nserve\nEND\n--name\nLauncher test\nEND\n",
            expected_binary.display()
        )
    );
    assert!(!args.contains("fixture-secret"));
}

const UNASSIGNED_GL_ID: &str = "00000000-0000-0000-0000-000000000010";
const OTHER_GL_ID: &str = "00000000-0000-0000-0000-000000000020";

fn write_private_json(path: &Path, value: &Value) {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).unwrap();
    serde_json::to_writer(&mut file, value).unwrap();
}

fn write_session(
    directory: &Path,
    base_url: &str,
    client_id: &str,
    code: Option<u32>,
    expired: bool,
) {
    std::fs::create_dir_all(directory).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    write_private_json(
        &directory.join("session.json"),
        &json!({
            "base_url": base_url,
            "client_id": client_id,
            "redirect_uri": "https://exact-mcp.localhost:8400/callback",
            "tokens": {
                "access_token": "private-access-token",
                "refresh_token": "private-refresh-token",
                "expires_at": if expired { 0 } else { u64::MAX },
            },
            "administration": code.map(|code| json!({"Code": code, "Description": format!("Fixture {code}")})),
        }),
    );
}

fn assert_no_secrets(output: &Output) {
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for secret in [
        "private-access-token",
        "private-refresh-token",
        "private-client-secret",
        "access_token",
        "refresh_token",
    ] {
        assert!(!combined.contains(secret), "Output exposed {secret}");
    }
}

#[test]
fn bank_config_help_describes_subcommands_and_repeatable_uuid_flag() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    let output = run(&directory, &["bank-config", "--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    for command in ["set", "show", "clear"] {
        assert!(stdout.contains(command));
    }
    let output = run(&directory, &["bank-config", "set", "--help"]);
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("--unassigned-gl-account-id <UUID>")
    );
    assert!(!directory.exists());
}

#[test]
fn bank_config_requires_selection_without_refreshing_expired_tokens() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    write_session(&directory, "http://127.0.0.1:1/", "public-app", None, true);
    let before = std::fs::read(directory.join("session.json")).unwrap();
    for arguments in [
        vec!["bank-config", "show"],
        vec!["bank-config", "show", "--json"],
        vec!["bank-config", "clear"],
        vec![
            "bank-config",
            "set",
            "--unassigned-gl-account-id",
            UNASSIGNED_GL_ID,
        ],
    ] {
        let output = run(&directory, &arguments);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("administrations select"), "{stderr}");
        assert_no_secrets(&output);
        assert_eq!(
            std::fs::read(directory.join("session.json")).unwrap(),
            before
        );
    }
}

struct BankConfigServer {
    base_url: String,
    handle: JoinHandle<Vec<String>>,
}

impl BankConfigServer {
    fn new(accounts: Value, journals: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}/", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..2 {
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
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                assert!(
                    request.starts_with("GET /api/v1/42/financial/"),
                    "{request}"
                );
                let rows = if request.contains("/GLAccounts?") {
                    &accounts
                } else {
                    assert!(request.contains("/Journals?"), "{request}");
                    &journals
                };
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    assert!(!line.is_empty());
                }
                let body = json!({"d": {"results": rows}}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                requests.push(request);
            }
            requests
        });
        Self { base_url, handle }
    }

    fn finish(self) -> Vec<String> {
        self.handle.join().unwrap()
    }
}

fn bank_gl(id: &str) -> Value {
    json!({
        "ID": id,
        "Division": 42,
        "Code": "0900",
        "Description": "Fixture unassigned bank allocation",
        "Type": 90,
        "BalanceSide": "D",
        "BalanceType": "B",
    })
}

fn bank_configuration_entry(
    base_url: &str,
    client_id: &str,
    administration: u32,
    id: &str,
) -> Value {
    json!({
        "scope": {
            "base_url": base_url,
            "client_id": client_id,
            "administration": administration,
        },
        "configuration": {"unassigned_gl_account_ids": [id]},
    })
}

#[test]
fn bank_config_show_and_clear_are_local_expired_secretless_and_scope_specific() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    let origin = "http://127.0.0.1:1/";
    let other_origin = "http://127.0.0.1:2/";
    let scopes = [
        (origin, "public-app", 42, UNASSIGNED_GL_ID),
        (origin, "public-app", 43, OTHER_GL_ID),
        (origin, "other-app", 42, OTHER_GL_ID),
        (other_origin, "public-app", 42, OTHER_GL_ID),
    ];
    write_session(&directory, origin, "public-app", Some(42), true);
    let entries: Vec<_> = scopes
        .iter()
        .map(|(base, app, code, id)| bank_configuration_entry(base, app, *code, id))
        .collect();
    let configuration_path = directory.join("bank-config.json");
    write_private_json(&configuration_path, &json!(entries));
    let before = std::fs::read(&configuration_path).unwrap();
    for (base, app, code, id) in scopes {
        write_session(&directory, base, app, Some(code), true);
        let session_before = std::fs::read(directory.join("session.json")).unwrap();
        let output = run(&directory, &["bank-config", "show", "--json"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_no_secrets(&output);
        let shown: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            shown["administration"],
            json!({"Code": code, "Description": format!("Fixture {code}")})
        );
        assert_eq!(shown["base_url"], base);
        assert_eq!(shown["client_id"], app);
        assert_eq!(shown["local_only"], true);
        assert_eq!(shown["revalidated"], false);
        assert_eq!(
            shown["configuration"],
            json!({"unassigned_gl_account_ids": [id]})
        );
        assert!(shown.get("tokens").is_none());
        assert_eq!(std::fs::read(&configuration_path).unwrap(), before);
        assert_eq!(
            std::fs::read(directory.join("session.json")).unwrap(),
            session_before
        );
    }
    write_session(&directory, origin, "public-app", Some(42), true);
    let output = run(&directory, &["bank-config", "show"]);
    assert!(output.status.success());
    assert_no_secrets(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "42",
        "Fixture 42",
        origin,
        "public-app",
        "configured",
        UNASSIGNED_GL_ID,
        "Local",
        "not revalidated",
    ] {
        assert!(stdout.contains(expected), "Missing {expected}: {stdout}");
    }
    let session_before = std::fs::read(directory.join("session.json")).unwrap();
    let output = run(&directory, &["bank-config", "clear"]);
    assert!(output.status.success());
    assert_no_secrets(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Cleared local"), "{stdout}");
    assert!(stdout.contains("42"));
    let remaining: Value =
        serde_json::from_slice(&std::fs::read(&configuration_path).unwrap()).unwrap();
    assert_eq!(remaining, json!(&entries[1..]));
    assert_eq!(
        std::fs::read(directory.join("session.json")).unwrap(),
        session_before
    );
    let output = run(&directory, &["bank-config", "show", "--json"]);
    assert!(output.status.success());
    assert_no_secrets(&output);
    let shown: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(shown["configuration"].is_null());
    let before = std::fs::read(&configuration_path).unwrap();
    let output = run(&directory, &["bank-config", "clear"]);
    assert!(output.status.success());
    assert_no_secrets(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("nothing to clear"));
    assert_eq!(std::fs::read(&configuration_path).unwrap(), before);
}

#[test]
fn bank_config_unconfigured_show_and_clear_do_not_create_configuration() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    write_session(
        &directory,
        "http://127.0.0.1:1/",
        "public-app",
        Some(42),
        true,
    );
    for arguments in [vec!["bank-config", "show"], vec!["bank-config", "clear"]] {
        let output = run(&directory, &arguments);
        assert!(output.status.success());
        assert_no_secrets(&output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("not configured") || stdout.contains("nothing to clear"),
            "{stdout}"
        );
        assert!(!directory.join("bank-config.json").exists());
    }
}

#[test]
fn bank_config_set_validates_only_master_data_before_saving_repeatable_ids() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    let server = BankConfigServer::new(
        json!([bank_gl(UNASSIGNED_GL_ID), bank_gl(OTHER_GL_ID)]),
        json!([]),
    );
    write_session(&directory, &server.base_url, "public-app", Some(42), false);
    std::fs::write(
        root.path().join(".env"),
        "EXACT_CLIENT_SECRET=private-client-secret\n",
    )
    .unwrap();
    let session_before = std::fs::read(directory.join("session.json")).unwrap();
    let output = run(
        &directory,
        &[
            "bank-config",
            "set",
            "--unassigned-gl-account-id",
            UNASSIGNED_GL_ID,
            "--unassigned-gl-account-id",
            OTHER_GL_ID,
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_no_secrets(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "Saved local",
        "42",
        "Fixture 42",
        UNASSIGNED_GL_ID,
        OTHER_GL_ID,
    ] {
        assert!(stdout.contains(expected), "Missing {expected}: {stdout}");
    }
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains("/Journals?"));
    assert!(requests[1].contains("/GLAccounts?"));
    assert_eq!(
        std::fs::read(directory.join("session.json")).unwrap(),
        session_before
    );
    let output = run(&directory, &["bank-config", "show", "--json"]);
    assert!(output.status.success());
    assert_no_secrets(&output);
    let shown: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        shown["configuration"],
        json!({"unassigned_gl_account_ids": [UNASSIGNED_GL_ID, OTHER_GL_ID]})
    );
}

#[test]
fn bank_config_set_rejected_master_data_does_not_overwrite_old_configuration() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    let server = BankConfigServer::new(json!([]), json!([]));
    write_session(&directory, &server.base_url, "public-app", Some(42), false);
    let configuration_path = directory.join("bank-config.json");
    write_private_json(
        &configuration_path,
        &json!([
            bank_configuration_entry(&server.base_url, "public-app", 42, OTHER_GL_ID),
            bank_configuration_entry(&server.base_url, "public-app", 43, UNASSIGNED_GL_ID),
        ]),
    );
    let before = std::fs::read(&configuration_path).unwrap();
    let output = run(
        &directory,
        &[
            "bank-config",
            "set",
            "--unassigned-gl-account-id",
            UNASSIGNED_GL_ID,
        ],
    );
    assert!(!output.status.success());
    assert_no_secrets(&output);
    assert_eq!(server.finish().len(), 2);
    assert_eq!(std::fs::read(&configuration_path).unwrap(), before);
}

#[test]
fn bank_config_set_rejects_invalid_input_without_network_or_overwriting_configuration() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    let origin = "http://127.0.0.1:1/";
    write_session(&directory, origin, "public-app", Some(42), true);
    let configuration_path = directory.join("bank-config.json");
    write_private_json(
        &configuration_path,
        &json!([bank_configuration_entry(
            origin,
            "public-app",
            42,
            OTHER_GL_ID
        ),]),
    );
    let before = std::fs::read(&configuration_path).unwrap();
    let mut too_many = vec!["bank-config", "set"];
    for _ in 0..11 {
        too_many.extend(["--unassigned-gl-account-id", UNASSIGNED_GL_ID]);
    }
    for arguments in [
        vec!["bank-config", "set"],
        vec![
            "bank-config",
            "set",
            "--unassigned-gl-account-id",
            "not-a-uuid",
        ],
        vec![
            "bank-config",
            "set",
            "--unassigned-gl-account-id",
            UNASSIGNED_GL_ID,
            "--unassigned-gl-account-id",
            UNASSIGNED_GL_ID,
        ],
        too_many,
    ] {
        let output = run(&directory, &arguments);
        assert!(!output.status.success());
        assert_no_secrets(&output);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("EXACT_CLIENT_SECRET"), "{stderr}");
        assert!(!stderr.contains("request failed"), "{stderr}");
        assert_eq!(std::fs::read(&configuration_path).unwrap(), before);
    }
}
