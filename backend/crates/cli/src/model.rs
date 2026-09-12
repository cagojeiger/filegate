use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageKind {
    S3,
    Fs,
}

#[derive(Deserialize, Serialize)]
pub struct Storage {
    pub id: String,
    pub kind: StorageKind,
    pub force_relay: bool,
    pub root_path: Option<String>,
    pub endpoint: Option<String>,
    pub public_endpoint: Option<String>,
    pub region: Option<String>,
    pub bucket: Option<String>,
    pub force_path_style: bool,
    pub access_key: Option<String>,
    pub capacity_bytes: i64,
}

#[derive(Deserialize, Serialize)]
pub struct Client {
    pub id: String,
    pub storage_id: String,
}

#[derive(Deserialize, Serialize)]
pub struct StorageUsage {
    pub storage_id: String,
    pub kind: StorageKind,
    pub capacity_bytes: i64,
    pub reserved_bytes: i64,
    pub active_bytes: i64,
    pub purge_pending_bytes: i64,
    pub remaining_bytes: i64,
    pub reserved_files: i64,
    pub active_files: i64,
    pub purge_pending_files: i64,
}

#[derive(Deserialize, Serialize)]
pub struct ClientUsage {
    pub client_id: String,
    pub storage_id: String,
    pub active_files: i64,
    pub active_bytes: i64,
}

#[derive(Deserialize, Serialize)]
pub struct Snapshot {
    pub day: String,
    pub storage_id: String,
    pub client_id: String,
    pub active_bytes: i64,
    pub active_files: i64,
}

#[derive(Deserialize)]
pub struct Identity {
    pub name: String,
    pub version: String,
}

#[derive(Deserialize)]
pub struct Health {
    pub status: String,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum Data {
    Update(crate::update::UpdateResult),
    Installation { path: std::path::PathBuf },
    Strings(Vec<String>),
    Storages(Vec<Storage>),
    Storage(Storage),
    Client(Client),
    StorageUsage(Vec<StorageUsage>),
    ClientUsage(Vec<ClientUsage>),
    History(Vec<Snapshot>),
    Status(crate::status::Status),
}
