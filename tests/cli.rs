use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn run(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_exact-mcp"))
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
    for arguments in [vec!["--help"], vec!["auth", "--help"], vec!["--version"]] {
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
fn list_and_select_require_login() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    for arguments in [
        vec!["administrations", "list"],
        vec!["administrations", "select", "42"],
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
