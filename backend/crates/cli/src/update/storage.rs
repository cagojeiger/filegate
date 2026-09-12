use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::{NamedTempFile, TempPath};

use super::REPOSITORY;
use crate::error::Error;

const RECEIPT: &str = "gscli-install-receipt.json";
const LOCK: &str = ".gscli-update.lock";

#[derive(Deserialize, Serialize)]
struct Receipt {
    schema_version: u8,
    managed_by: String,
    repository: String,
    install_path: PathBuf,
    target: String,
}

pub(super) fn validate_receipt(executable: &Path, target: &str) -> Result<(), Error> {
    regular(executable, false)?;
    let path = parent(executable)?.join(RECEIPT);
    regular(&path, false)?;
    let mut bytes = Vec::new();
    File::open(&path)
        .map_err(|_| unmanaged())?
        .take(16385)
        .read_to_end(&mut bytes)
        .map_err(|_| unmanaged())?;
    if bytes.len() > 16384 {
        return Err(unmanaged());
    }
    let receipt: Receipt = serde_json::from_slice(&bytes).map_err(|_| unmanaged())?;
    if receipt.schema_version != 1
        || receipt.managed_by != "gscli-installer"
        || receipt.repository != REPOSITORY
        || receipt.target != target
        || receipt.install_path != fs::canonicalize(executable).map_err(|_| unmanaged())?
    {
        return Err(unmanaged());
    }
    Ok(())
}

pub(super) fn lock(executable: &Path) -> Result<File, Error> {
    let path = parent(executable)?.join(LOCK);
    regular(&path, true)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(|_| local_error())?;
    file.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => Error::new(
            "update_in_progress",
            "Another CLI installation or update is in progress",
            6,
        ),
        TryLockError::Error(_) => local_error(),
    })?;
    Ok(file)
}

pub(super) fn candidate(destination: &Path, bytes: &[u8]) -> Result<TempPath, Error> {
    let mut file = NamedTempFile::new_in(parent(destination)?).map_err(|_| local_error())?;
    file.write_all(bytes).map_err(|_| local_error())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))
            .map_err(|_| local_error())?;
    }
    file.as_file().sync_all().map_err(|_| local_error())?;
    // Close the writable descriptor before executing the candidate (Linux ETXTBSY).
    Ok(file.into_temp_path())
}

pub(super) fn replace(candidate: TempPath, destination: &Path) -> Result<(), Error> {
    regular(destination, true)?;
    candidate.persist(destination).map_err(|_| local_error())?;
    sync(parent(destination)?).map_err(|_| applied_error())
}

pub(super) fn install(source: &Path, directory: &Path, target: &str) -> Result<PathBuf, Error> {
    let directory = fs::canonicalize(directory).map_err(|_| local_error())?;
    let destination = directory.join("gscli");
    let _lock = lock(&destination)?;
    regular(&destination, true)?;
    let receipt_path = directory.join(RECEIPT);
    regular(&receipt_path, true)?;
    let receipt = Receipt {
        schema_version: 1,
        managed_by: "gscli-installer".to_owned(),
        repository: REPOSITORY.to_owned(),
        install_path: destination.clone(),
        target: target.to_owned(),
    };
    let mut record = NamedTempFile::new_in(&directory).map_err(|_| local_error())?;
    serde_json::to_writer(&mut record, &receipt).map_err(|_| local_error())?;
    record.as_file().sync_all().map_err(|_| local_error())?;
    let bytes = fs::read(source).map_err(|_| local_error())?;
    replace(candidate(&destination, &bytes)?, &destination)?;
    persist_receipt(record, &receipt_path).map_err(|_| applied_error())?;
    sync(&directory).map_err(|_| applied_error())?;
    Ok(destination)
}

fn sync(directory: &Path) -> std::io::Result<()> {
    #[cfg(all(test, unix))]
    super::tests::recovery::fail_if_requested(super::tests::recovery::Operation::Sync)?;
    File::open(directory)?.sync_all()
}

fn persist_receipt(record: NamedTempFile, path: &Path) -> std::io::Result<()> {
    #[cfg(all(test, unix))]
    super::tests::recovery::fail_if_requested(super::tests::recovery::Operation::Receipt)?;
    record
        .persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
}

fn parent(path: &Path) -> Result<&Path, Error> {
    path.parent().ok_or_else(local_error)
}

fn regular(path: &Path, allow_missing: bool) -> Result<(), Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(unmanaged()),
    }
}

pub(super) fn local_error() -> Error {
    Error::new(
        "update_local_error",
        "Could not write or access the CLI installation",
        1,
    )
}

fn unmanaged() -> Error {
    Error::new(
        "unmanaged_install",
        "Use the official installer before updating; the install receipt must match this executable",
        2,
    )
}

fn applied_error() -> Error {
    let mut error = Error::new(
        "update_applied_incomplete",
        "CLI binary was replaced but installation metadata or directory sync failed; rerun the official installer",
        8,
    );
    error.outcome = "applied";
    error
}
