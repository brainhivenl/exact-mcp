use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

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

fn check_private(path: &Path) -> Result<()> {
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
