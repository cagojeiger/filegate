use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::Error;

const MAX_STORAGE_INPUT_BYTES: u64 = 1024 * 1024;
const MAX_KEY_BYTES: u64 = 8192;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum StorageKind {
    S3,
    Fs,
}

fn default_storage_kind() -> StorageKind {
    StorageKind::S3
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StorageSpec {
    #[serde(default = "default_storage_kind")]
    kind: StorageKind,
    #[serde(default)]
    force_relay: bool,
    root_path: Option<String>,
    endpoint: Option<String>,
    public_endpoint: Option<String>,
    region: Option<String>,
    bucket: Option<String>,
    #[serde(default)]
    force_path_style: bool,
    access_key: Option<String>,
    secret_key: Option<String>,
    capacity_bytes: i64,
}

pub fn storage_spec(path: &Path) -> Result<StorageSpec, Error> {
    let bytes = read(path, MAX_STORAGE_INPUT_BYTES, true)?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Error::input("Storage input must be one valid JSON object"))?;
    let object = value
        .as_object()
        .ok_or_else(|| Error::input("Storage input must be one valid JSON object"))?;
    if object.contains_key("id") {
        return Err(Error::input("Storage input must not contain id"));
    }
    serde_json::from_value(value)
        .map_err(|_| Error::input("Storage input has unknown, missing, or invalid fields"))
}

pub fn client_key_hash(path: &Path) -> Result<String, Error> {
    let mut bytes = read(path, MAX_KEY_BYTES, false)?;
    if bytes.ends_with(b"\r\n") {
        bytes.truncate(bytes.len() - 2);
    } else if bytes.ends_with(b"\n") {
        bytes.truncate(bytes.len() - 1);
    }
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_graphic) {
        return Err(Error::input(
            "Client key file must contain one nonempty ASCII bearer value",
        ));
    }
    Ok(format!("sha256:{:x}", Sha256::digest(&bytes)))
}

fn read(path: &Path, max: u64, stdin_allowed: bool) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    if stdin_allowed && path == Path::new("-") {
        io::stdin()
            .lock()
            .take(max + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::input("Cannot read input from stdin"))?;
    } else {
        let file = File::open(path).map_err(|_| Error::input("Cannot open input file"))?;
        let metadata = file
            .metadata()
            .map_err(|_| Error::input("Cannot inspect input file"))?;
        if !metadata.is_file() {
            return Err(Error::input("Input path must resolve to a regular file"));
        }
        if metadata.len() > max {
            return Err(Error::input("Input file exceeds the size limit"));
        }
        file.take(max + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::input("Cannot read input file"))?;
    }
    if bytes.len() as u64 > max {
        return Err(Error::input("Input file exceeds the size limit"));
    }
    Ok(bytes)
}
