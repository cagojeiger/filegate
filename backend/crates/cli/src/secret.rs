use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::Error;

pub struct SecretOutput {
    path: PathBuf,
    file: Option<File>,
}

#[derive(Serialize)]
struct CredentialFile<'a> {
    schema_version: u8,
    client_id: &'a str,
    access_key_id: &'a str,
    secret_key: &'a str,
}

impl SecretOutput {
    pub fn create(path: &Path) -> Result<Self, Error> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(path)
            .map_err(|_| Error::input("Secret output must be a new writable file"))?;
        Ok(Self {
            path: path.to_owned(),
            file: Some(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write_credential(
        &mut self,
        client_id: &str,
        access_key_id: &str,
        secret_key: &str,
    ) -> io::Result<()> {
        let payload = serde_json::to_vec_pretty(&CredentialFile {
            schema_version: 1,
            client_id,
            access_key_id,
            secret_key,
        })?;
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "secret output is closed"))?;
        file.write_all(&payload)?;
        file.write_all(b"\n")?;
        file.sync_all()
    }

    pub fn state(&self) -> &'static str {
        match self.file.as_ref().and_then(|file| file.metadata().ok()) {
            Some(metadata) if metadata.len() > 0 => "partial",
            _ => "empty",
        }
    }

    pub fn discard(mut self) {
        self.file.take();
        let _ = fs::remove_file(&self.path);
    }
}
