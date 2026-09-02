use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

#[derive(Deserialize, Serialize)]
pub struct Session {
    pub base_url: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub tokens: Tokens,
    pub administration: Option<Administration>,
}

#[derive(Deserialize, Serialize)]
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

pub struct Store {
    directory: PathBuf,
    _lock: File,
}

impl Store {
    pub fn open(directory: &Path) -> Result<Self> {
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
        lock.lock().context("Cannot lock session storage")?;
        Ok(Self {
            directory: directory.to_owned(),
            _lock: lock,
        })
    }

    pub fn load(&self) -> Result<Option<Session>> {
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

    pub fn require_session(&self) -> Result<Session> {
        self.load()?
            .context("Not logged in; run `exact-mcp auth login` first")
    }

    pub fn save(&self, session: &Session) -> Result<()> {
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
