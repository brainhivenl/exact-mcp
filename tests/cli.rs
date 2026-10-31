use std::path::Path;
use std::process::{Command, Output};

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
fn list_select_and_serve_require_login() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    for arguments in [
        vec!["administrations", "list"],
        vec!["administrations", "select", "42"],
        vec!["serve"],
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
