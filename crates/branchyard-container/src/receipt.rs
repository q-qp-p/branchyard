use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use branchyard_sandbox::ProviderError;
use serde::{Deserialize, Serialize};

use crate::Config;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    pub version: u8,
    pub name: String,
    pub owner: String,
    pub daemon: String,
    pub digest: String,
    pub image: String,
    pub image_id: String,
    pub cpus: u8,
    pub memory: u32,
    pub config: Config,
    pub id: Option<String>,
    pub volumes: Vec<String>,
}

impl Receipt {
    pub fn container_name(&self) -> String {
        format!("branchyard-{}", self.owner.to_ascii_lowercase())
    }
}

pub(crate) struct Locked {
    _lock: File,
    path: PathBuf,
}

impl Locked {
    pub fn open(dir: &Path, name: &str) -> Result<Self, ProviderError> {
        let key = blake3::hash(name.as_bytes()).to_hex();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(dir.join(format!("{key}.lock")))?;
        lock.lock()?;
        Ok(Self {
            _lock: lock,
            path: dir.join(format!("{key}.json")),
        })
    }

    pub fn read(&self) -> Result<Option<Receipt>, ProviderError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|_| {
                ProviderError::Runtime("container ownership receipt is invalid".into())
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn write(&self, receipt: &Receipt) -> Result<(), ProviderError> {
        let bytes = serde_json::to_vec(receipt).map_err(io::Error::other)?;
        let temporary = self.path.with_extension("pending");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &self.path)?;
        self.sync_dir()
    }

    pub fn remove(&self) -> Result<(), ProviderError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => self.sync_dir(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn sync_dir(&self) -> Result<(), ProviderError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("receipt has no parent"))?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}
