use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use uuid::Uuid;

#[derive(Clone, Deserialize, Serialize)]
pub struct Session {
    pub base_url: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub tokens: Tokens,
    pub administration: Option<Administration>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Administration {
    pub code: u32,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BankConfiguration {
    pub unassigned_gl_account_ids: Vec<Uuid>,
}

impl BankConfiguration {
    pub fn validate(&self) -> Result<BTreeSet<Uuid>> {
        ensure!(
            (1..=10).contains(&self.unassigned_gl_account_ids.len()),
            "Select 1–10 verified unassigned G/L UUIDs"
        );
        let mut ids = BTreeSet::new();
        for id in &self.unassigned_gl_account_ids {
            ensure!(!id.is_nil(), "Unassigned G/L UUID must not be nil");
            ensure!(ids.insert(*id), "Duplicate unassigned G/L UUID selection");
        }
        Ok(ids)
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct BankConfigurationScope {
    base_url: String,
    client_id: String,
    administration: u32,
}

impl BankConfigurationScope {
    fn from_session(session: &Session) -> Result<Self> {
        Ok(Self {
            base_url: session.base_url.clone(),
            client_id: session.client_id.clone(),
            administration: session
                .administration
                .as_ref()
                .context("No administration selected; run `exact-mcp administrations select <code>` first")?
                .code,
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredBankConfiguration {
    scope: BankConfigurationScope,
    configuration: BankConfiguration,
}

#[derive(Clone)]
pub struct Store {
    directory: PathBuf,
    lock: Arc<File>,
}

impl Store {
    pub async fn open(directory: &Path) -> Result<Self> {
        let directory = directory.to_owned();
        let store = tokio::task::spawn_blocking(move || Self::prepare(&directory))
            .await
            .context("Session storage task failed")??;
        loop {
            match store.lock.try_lock() {
                Ok(()) => return Ok(store),
                Err(TryLockError::WouldBlock) => {
                    tokio::time::sleep(Duration::from_millis(25)).await
                }
                Err(TryLockError::Error(error)) => {
                    return Err(error).context("Cannot lock session storage");
                }
            }
        }
    }

    fn prepare(directory: &Path) -> Result<Self> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(directory)
            .context("Cannot create data directory")?;
        check_private(directory)?;
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(directory.join("session.lock"))?;
        Ok(Self {
            directory: directory.to_owned(),
            lock: Arc::new(lock),
        })
    }

    pub async fn load(&self) -> Result<Option<Session>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.load_sync())
            .await
            .context("Session read task failed")?
    }

    fn load_sync(&self) -> Result<Option<Session>> {
        let path = self.directory.join("session.json");
        match fs::symlink_metadata(&path) {
            Ok(_) => check_private(&path)?,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("Cannot inspect stored session"),
        }
        let bytes = fs::read(path).context("Cannot read stored session")?;
        let session = serde_json::from_slice(&bytes).map_err(|_| {
            anyhow::anyhow!("Invalid session file; run `exact-mcp auth login` again")
        })?;
        Ok(Some(session))
    }

    pub async fn require_session(&self) -> Result<Session> {
        self.load()
            .await?
            .context("Not logged in; run `exact-mcp auth login` first")
    }

    pub async fn save(&self, session: &Session) -> Result<()> {
        let store = self.clone();
        let session = session.clone();
        tokio::task::spawn_blocking(move || store.save_sync(&session))
            .await
            .context("Session save task failed")?
    }

    pub async fn bank_configuration(&self, session: &Session) -> Result<Option<BankConfiguration>> {
        let scope = BankConfigurationScope::from_session(session)?;
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            Ok(store
                .load_bank_configurations_sync()?
                .into_iter()
                .find(|entry| entry.scope == scope)
                .map(|entry| entry.configuration))
        })
        .await
        .context("Bank configuration read task failed")?
    }

    pub async fn save_bank_configuration(
        &self,
        session: &Session,
        configuration: BankConfiguration,
    ) -> Result<()> {
        configuration.validate()?;
        let scope = BankConfigurationScope::from_session(session)?;
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut entries = store.load_bank_configurations_sync()?;
            entries.retain(|entry| entry.scope != scope);
            entries.push(StoredBankConfiguration {
                scope,
                configuration,
            });
            entries.sort_by(|a, b| a.scope.cmp(&b.scope));
            store.save_bank_configurations_sync(&entries)
        })
        .await
        .context("Bank configuration save task failed")?
    }

    pub async fn clear_bank_configuration(&self, session: &Session) -> Result<bool> {
        let scope = BankConfigurationScope::from_session(session)?;
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut entries = store.load_bank_configurations_sync()?;
            let count = entries.len();
            entries.retain(|entry| entry.scope != scope);
            if entries.len() == count {
                return Ok(false);
            }
            store.save_bank_configurations_sync(&entries)?;
            Ok(true)
        })
        .await
        .context("Bank configuration clear task failed")?
    }

    fn load_bank_configurations_sync(&self) -> Result<Vec<StoredBankConfiguration>> {
        let path = self.directory.join("bank-config.json");
        match fs::symlink_metadata(&path) {
            Ok(_) => check_private(&path)?,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("Cannot inspect bank configuration"),
        }
        let bytes = fs::read(&path).context("Cannot read bank configuration")?;
        let entries: Vec<StoredBankConfiguration> = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("Invalid bank configuration file"))?;
        let mut scopes = BTreeSet::new();
        for entry in &entries {
            entry.configuration.validate()?;
            ensure!(
                scopes.insert(entry.scope.clone()),
                "Duplicate bank configuration scope"
            );
        }
        Ok(entries)
    }

    fn save_bank_configurations_sync(&self, entries: &[StoredBankConfiguration]) -> Result<()> {
        let mut temporary = NamedTempFile::new_in(&self.directory)?;
        serde_json::to_writer_pretty(&mut temporary, entries)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.directory.join("bank-config.json"))
            .context("Cannot save bank configuration")?;
        #[cfg(unix)]
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }

    pub async fn save_invoice_pdf(
        &self,
        division: u32,
        attachment_id: uuid::Uuid,
        bytes: Vec<u8>,
    ) -> Result<PathBuf> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut directory = store.directory.clone();
            for component in ["downloads".to_owned(), division.to_string()] {
                directory.push(component);
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                match builder.create(&directory) {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(error).context("Cannot create invoice download directory");
                    }
                }
                check_private(&directory)?;
                anyhow::ensure!(
                    directory.is_dir(),
                    "Invoice download path must be a directory"
                );
            }
            let mut file = tempfile::Builder::new()
                .prefix(&format!("invoice-{attachment_id}-"))
                .suffix(".part")
                .tempfile_in(&directory)?;
            file.write_all(&bytes)?;
            file.as_file().sync_all()?;
            let path = file.path().with_extension("pdf");
            file.persist_noclobber(&path)
                .context("Cannot save invoice PDF without overwriting an existing file")?;
            #[cfg(unix)]
            File::open(&directory)?.sync_all()?;
            fs::canonicalize(path).context("Cannot resolve saved invoice PDF path")
        })
        .await
        .context("Invoice PDF save task failed")?
    }

    fn save_sync(&self, session: &Session) -> Result<()> {
        let mut temporary = NamedTempFile::new_in(&self.directory)?;
        serde_json::to_writer_pretty(&mut temporary, session)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.directory.join("session.json"))
            .context("Cannot save session; the refreshed tokens may require a new login")?;
        #[cfg(unix)]
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
}

pub(crate) fn check_private(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_symlink() {
        bail!(
            "Session storage must not be a symbolic link: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!(
                "Session storage must not be accessible to other users: {}",
                path.display()
            );
        }
    }
    Ok(())
}
