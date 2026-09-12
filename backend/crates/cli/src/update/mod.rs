mod download;
mod storage;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::error::Error;

const REPOSITORY: &str = "cagojeiger/filegate";
const LATEST: &str =
    "https://github.com/cagojeiger/filegate/releases/latest/download/gscli-manifest.json";
const MAX_MANIFEST: usize = 256 * 1024;
const MAX_BINARY: usize = 100 * 1024 * 1024;

#[derive(Serialize)]
pub struct UpdateResult {
    status: &'static str,
    current_version: String,
    latest_version: String,
    target: &'static str,
    path: PathBuf,
    pub(crate) updated: bool,
}

#[derive(Deserialize)]
struct Manifest {
    schema_version: u8,
    repository: String,
    version: String,
    assets: BTreeMap<String, Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    sha256: String,
    size: usize,
}

impl Manifest {
    fn validate(&self, target: &str) -> Result<(&Asset, Version), Error> {
        let version = stable_version(&self.version)?;
        let asset = self.assets.get(target).ok_or_else(invalid_manifest)?;
        if self.schema_version != 1
            || self.repository != REPOSITORY
            || asset.name != format!("gscli-{target}")
            || asset.size == 0
            || asset.size > MAX_BINARY
            || asset.sha256.len() != 64
            || !asset
                .sha256
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(invalid_manifest());
        }
        Ok((asset, version))
    }
}

pub async fn run(check: bool, timeout: u64) -> Result<UpdateResult, Error> {
    let path = std::env::current_exe().map_err(|_| storage::local_error())?;
    let source = download::Source::official(timeout)?;
    update(&path, check, &source).await
}

async fn update(
    path: &Path,
    check: bool,
    source: &download::Source,
) -> Result<UpdateResult, Error> {
    let target = target()?;
    storage::validate_receipt(path, target)?;
    // Reinstallation uses this same lock. Read the on-disk version after taking it.
    let _lock = if check {
        None
    } else {
        Some(storage::lock(path)?)
    };
    storage::validate_receipt(path, target)?;
    let current = executable_version(path, Duration::from_secs(5)).await?;
    let bytes = source.get(source.manifest_url(), MAX_MANIFEST).await?;
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|_| invalid_manifest())?;
    let (asset, latest) = manifest.validate(target)?;
    let available = latest > current;
    let mut result = UpdateResult {
        status: if available {
            "update_available"
        } else {
            "up_to_date"
        },
        current_version: current.to_string(),
        latest_version: latest.to_string(),
        target,
        path: path.to_owned(),
        updated: false,
    };
    if check || !available {
        return Ok(result);
    }
    let bytes = source
        .get(
            &source.artifact_url(&manifest.version, &asset.name),
            asset.size,
        )
        .await?;
    if bytes.len() != asset.size || format!("{:x}", Sha256::digest(&bytes)) != asset.sha256 {
        return Err(Error::new(
            "update_checksum_mismatch",
            "Update size or SHA-256 did not match the manifest",
            5,
        ));
    }
    let candidate = storage::candidate(path, &bytes)?;
    if executable_version(&candidate, Duration::from_secs(5)).await? != latest {
        return Err(Error::new(
            "candidate_version_mismatch",
            "Update executable version did not match the manifest",
            5,
        ));
    }
    storage::validate_receipt(path, target)?;
    storage::replace(candidate, path)?;
    result.status = "updated";
    result.updated = true;
    Ok(result)
}

pub fn install(directory: &Path) -> Result<PathBuf, Error> {
    let target = target()?;
    let source = std::env::current_exe().map_err(|_| storage::local_error())?;
    storage::install(&source, directory, target)
}

async fn executable_version(path: &Path, limit: Duration) -> Result<Version, Error> {
    let probe = async {
        let mut child = tokio::process::Command::new(path)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| probe_error())?;
        let stdout = child.stdout.take().ok_or_else(probe_error)?;
        let mut bytes = Vec::new();
        stdout
            .take(256)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| probe_error())?;
        if bytes.len() == 256 || !child.wait().await.map_err(|_| probe_error())?.success() {
            return Err(probe_error());
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| probe_error())?;
        let version = text
            .strip_prefix("gscli ")
            .and_then(|v| v.strip_suffix('\n'))
            .ok_or_else(probe_error)?;
        stable_version(version).map_err(|_| probe_error())
    };
    tokio::time::timeout(limit, probe)
        .await
        .map_err(|_| probe_error())?
}

fn stable_version(input: &str) -> Result<Version, Error> {
    let version = Version::parse(input).map_err(|_| invalid_manifest())?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        return Err(invalid_manifest());
    }
    Ok(version)
}

fn target() -> Result<&'static str, Error> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") if cfg!(target_env = "gnu") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") if cfg!(target_env = "gnu") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => Err(Error::new(
            "unsupported_update_target",
            "Official updates support Linux glibc and macOS on x86_64 or arm64",
            2,
        )),
    }
}

fn invalid_manifest() -> Error {
    Error::new(
        "invalid_update_manifest",
        "Expected a stable official gscli release manifest for this platform",
        5,
    )
}

fn probe_error() -> Error {
    Error::new(
        "update_probe_failed",
        "Could not verify executable version within five seconds",
        5,
    )
}

#[cfg(all(test, unix))]
mod tests;
