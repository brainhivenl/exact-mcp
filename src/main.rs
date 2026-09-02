mod exact;
mod oauth;
mod store;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;

use store::{Session, Store};

#[derive(Parser)]
#[command(
    version,
    about = "Local Exact Online authentication and administration setup"
)]
struct Cli {
    #[arg(long, global = true, env = "EXACT_MCP_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Authorize this app or inspect the locally stored session.")]
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    #[command(about = "List accessible administrations or explicitly select one.")]
    Administrations {
        #[command(subcommand)]
        command: AdministrationCommand,
    },
}

#[derive(Subcommand)]
enum AuthCommand {
    #[command(about = "Print an authorization URL and wait for the localhost callback.")]
    Login {
        #[arg(
            long,
            env = "EXACT_BASE_URL",
            default_value = "https://start.exactonline.nl"
        )]
        base_url: String,
        #[arg(
            long,
            env = "EXACT_REDIRECT_URI",
            default_value = "https://exact-mcp.localhost:8400/callback"
        )]
        redirect_uri: String,
        #[arg(long, env = "EXACT_TLS_CERT", requires = "tls_key")]
        tls_cert: Option<PathBuf>,
        #[arg(long, env = "EXACT_TLS_KEY", requires = "tls_cert")]
        tls_key: Option<PathBuf>,
    },
    #[command(about = "Show local token expiry and selection without contacting Exact.")]
    Status,
}

#[derive(Subcommand)]
enum AdministrationCommand {
    #[command(about = "Fetch all administrations accessible to the authorized user/app.")]
    List {
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Validate access and save the administration code for future tools.")]
    Select { code: u32 },
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let directory = match cli.data_dir {
        Some(directory) => directory,
        None => ProjectDirs::from("", "", "exact-mcp")
            .context("Cannot determine data directory; set EXACT_MCP_DATA_DIR")?
            .data_local_dir()
            .to_owned(),
    };
    match cli.command {
        Command::Auth {
            command:
                AuthCommand::Login {
                    base_url,
                    redirect_uri,
                    tls_cert,
                    tls_key,
                },
        } => {
            let base = oauth::validate_base_url(&base_url)?;
            let redirect = oauth::validate_redirect_uri(&redirect_uri)?;
            let client_id = required_environment("EXACT_CLIENT_ID")?;
            let client_secret = required_environment("EXACT_CLIENT_SECRET")?;
            let self_signed = tls_cert.is_none();
            let tls_config = match (tls_cert, tls_key) {
                (Some(cert), Some(key)) => oauth::load_tls_config(&cert, &key)?,
                (None, None) => oauth::self_signed_tls_config(&redirect)?,
                _ => anyhow::bail!("Provide both --tls-cert and --tls-key, or neither"),
            };
            let client = oauth::http_client()?;
            let listener = oauth::listen(&redirect)?;
            let state = oauth::random_state()?;
            let url = oauth::authorization_url(&base, &client_id, &redirect_uri, &state)?;
            if self_signed {
                println!(
                    "Using a temporary self-signed certificate. Accept the browser's certificate warning only for {redirect_uri}."
                );
            }
            println!("Open this URL in your browser:\n\n{url}\n");
            println!("Waiting up to 5 minutes for {redirect_uri}");
            let code = oauth::wait_for_code(
                &listener,
                &tls_config,
                &redirect,
                &state,
                Duration::from_secs(300),
            )?;
            let store = Store::open(&directory)?;
            let tokens = oauth::exchange_code(
                &client,
                &base,
                &client_id,
                &client_secret,
                &redirect_uri,
                &code,
            )?;
            store.save(&Session {
                base_url: base.to_string(),
                client_id,
                redirect_uri,
                tokens,
                administration: None,
            })?;
            println!("Login saved. Administration selection cleared.");
            println!(
                "Run `exact-mcp administrations list`, then `exact-mcp administrations select <code>`."
            );
        }
        Command::Auth {
            command: AuthCommand::Status,
        } => {
            let store = Store::open(&directory)?;
            println!("Data directory: {}", directory.display());
            match store.load()? {
                None => println!("Not logged in."),
                Some(session) => {
                    println!("Login: stored locally (not verified with Exact)");
                    println!("Exact: {}", session.base_url);
                    let remaining = session.tokens.expires_at.saturating_sub(oauth::now()?);
                    if remaining > 10 {
                        println!("Access token: expires in {remaining} seconds");
                    } else {
                        println!("Access token: refresh needed on next API command");
                    }
                    match session.administration {
                        Some(administration) => println!(
                            "Administration: {} — {}",
                            administration.code, administration.description
                        ),
                        None => println!("Administration: not selected"),
                    }
                }
            }
        }
        Command::Administrations { command } => {
            let store = Store::open(&directory)?;
            let mut session = store.require_session()?;
            let client = oauth::http_client()?;
            let secret = std::env::var("EXACT_CLIENT_SECRET").ok();
            let available =
                exact::administrations(&client, &store, &mut session, secret.as_deref())?;
            match command {
                AdministrationCommand::List { json: true } => {
                    println!("{}", serde_json::to_string_pretty(&available)?);
                }
                AdministrationCommand::List { json: false } => {
                    println!("  CODE\tNAME");
                    for administration in &available {
                        let marker = if session.administration.as_ref().map(|item| item.code)
                            == Some(administration.code)
                        {
                            '*'
                        } else {
                            ' '
                        };
                        println!(
                            "{marker} {}\t{}",
                            administration.code, administration.description
                        );
                    }
                    println!("* selected; listing does not change the selection.");
                }
                AdministrationCommand::Select { code } => {
                    let selected =
                        exact::select_administration(&store, &mut session, &available, code)?;
                    println!("Selected {} — {}", selected.code, selected.description);
                }
            }
        }
    }
    Ok(())
}

fn required_environment(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| format!("Set {name} before logging in"))?;
    ensure!(!value.trim().is_empty(), "{name} must not be empty");
    Ok(value)
}
