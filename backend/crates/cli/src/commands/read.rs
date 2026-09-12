use crate::args::Usage;
use crate::error::Error;
use crate::http::Api;
use crate::model::{self, Data};

use super::CommandResult;

pub async fn storage_list(api: &Api) -> CommandResult {
    let mut rows = api
        .get::<Vec<model::Storage>>(&["storages"], &[], true)
        .await?;
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    Ok((Data::Storages(rows), None))
}

pub async fn storage_show(api: &Api, id: &str) -> CommandResult {
    let row: model::Storage = api.get(&["storages", id], &[], true).await?;
    if row.id != id {
        return Err(Error::invalid_response());
    }
    Ok((Data::Storage(row), None))
}

pub async fn client_list(api: &Api) -> CommandResult {
    strings(api, &["clients"]).await
}

pub async fn client_show(api: &Api, id: &str) -> CommandResult {
    let row: model::Client = api.get(&["clients", id], &[], true).await?;
    if row.id != id {
        return Err(Error::invalid_response());
    }
    Ok((Data::Client(row), None))
}

pub async fn credential_list(api: &Api, client: &str) -> CommandResult {
    strings(api, &["clients", client, "s3-credentials"]).await
}

pub async fn client_key_list(api: &Api, client: &str) -> CommandResult {
    strings(api, &["clients", client, "keys"]).await
}

pub async fn usage(api: &Api, command: &Usage) -> CommandResult {
    match command {
        Usage::Storages => {
            let mut rows = api
                .get::<Vec<model::StorageUsage>>(&["usage"], &[], true)
                .await?;
            rows.sort_by(|a, b| a.storage_id.cmp(&b.storage_id));
            Ok((Data::StorageUsage(rows), None))
        }
        Usage::Clients => {
            let mut rows = api
                .get::<Vec<model::ClientUsage>>(&["usage", "clients"], &[], true)
                .await?;
            rows.sort_by(|a, b| {
                a.client_id
                    .cmp(&b.client_id)
                    .then(a.storage_id.cmp(&b.storage_id))
            });
            Ok((Data::ClientUsage(rows), None))
        }
        Usage::History { days } => {
            let mut rows = api
                .get::<Vec<model::Snapshot>>(
                    &["usage", "history"],
                    &[("days", days.to_string())],
                    true,
                )
                .await?;
            rows.sort_by(|a, b| {
                (&a.day, &a.storage_id, &a.client_id).cmp(&(&b.day, &b.storage_id, &b.client_id))
            });
            Ok((Data::History(rows), None))
        }
    }
}

async fn strings(api: &Api, path: &[&str]) -> CommandResult {
    let mut rows = api.get::<Vec<String>>(path, &[], true).await?;
    rows.sort();
    Ok((Data::Strings(rows), None))
}
