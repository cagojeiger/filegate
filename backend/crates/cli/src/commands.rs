use crate::args::{ClientList, Command, Resource, Usage};
use crate::error::Error;
use crate::http::Api;
use crate::model::{self, Data};

pub async fn run(api: &Api, command: &Command) -> Result<(Data, Option<Error>), Error> {
    match command {
        Command::Update { .. } | Command::Install { .. } => Err(Error::input(
            "Local commands execute without the management API",
        )),
        Command::Status => {
            let (status, error) = crate::status::inspect(api).await;
            Ok((Data::Status(status), error))
        }
        Command::Storage(Resource::List) => {
            let mut rows = api
                .get::<Vec<model::Storage>>(&["storages"], &[], true)
                .await?;
            rows.sort_by(|a, b| a.id.cmp(&b.id));
            Ok((Data::Storages(rows), None))
        }
        Command::Storage(Resource::Show { id }) => {
            let row: model::Storage = api.get(&["storages", id], &[], true).await?;
            if &row.id != id {
                return Err(Error::invalid_response());
            }
            Ok((Data::Storage(row), None))
        }
        Command::Client(Resource::List) => strings(api, &["clients"]).await,
        Command::Client(Resource::Show { id }) => {
            let row: model::Client = api.get(&["clients", id], &[], true).await?;
            if &row.id != id {
                return Err(Error::invalid_response());
            }
            Ok((Data::Client(row), None))
        }
        Command::Credential(ClientList::List { client }) => {
            strings(api, &["clients", client, "s3-credentials"]).await
        }
        Command::ClientKey(ClientList::List { client }) => {
            strings(api, &["clients", client, "keys"]).await
        }
        Command::Usage(Usage::Storages) => {
            let mut rows = api
                .get::<Vec<model::StorageUsage>>(&["usage"], &[], true)
                .await?;
            rows.sort_by(|a, b| a.storage_id.cmp(&b.storage_id));
            Ok((Data::StorageUsage(rows), None))
        }
        Command::Usage(Usage::Clients) => {
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
        Command::Usage(Usage::History { days }) => {
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

async fn strings(api: &Api, path: &[&str]) -> Result<(Data, Option<Error>), Error> {
    let mut rows = api.get::<Vec<String>>(path, &[], true).await?;
    rows.sort();
    Ok((Data::Strings(rows), None))
}
