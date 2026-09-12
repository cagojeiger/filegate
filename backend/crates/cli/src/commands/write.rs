use std::path::Path;

use serde::Serialize;

use crate::error::Error;
use crate::http::Api;
use crate::model::{self, Data};

use super::CommandResult;

#[derive(Serialize)]
struct StorageCreateBody<'a> {
    id: &'a str,
    #[serde(flatten)]
    spec: &'a crate::input::StorageSpec,
}

#[derive(Serialize)]
struct ClientCreateBody<'a> {
    id: &'a str,
    storage_id: &'a str,
}

#[derive(Serialize)]
struct ClientKeyCreateBody<'a> {
    key_hash: &'a str,
}

pub async fn storage_create(api: &Api, id: &str, path: &Path) -> CommandResult {
    let spec = crate::input::storage_spec(path)?;
    let row: model::Storage = api
        .post(&["storages"], &StorageCreateBody { id, spec: &spec })
        .await?;
    if row.id != id {
        return Err(Error::applied_invalid_response(201));
    }
    Ok((Data::Storage(row), None))
}

pub async fn storage_replace(api: &Api, id: &str, path: &Path, yes: bool) -> CommandResult {
    let spec = crate::input::storage_spec(path)?;
    confirm(api, yes, "Replace", &format!("storage {id}"))?;
    let row: model::Storage = api.put(&["storages", id], &spec).await?;
    if row.id != id {
        return Err(Error::applied_invalid_response(200));
    }
    Ok((Data::Storage(row), None))
}

pub async fn storage_delete(api: &Api, id: &str, yes: bool) -> CommandResult {
    confirm(api, yes, "Delete", &format!("storage {id}"))?;
    api.delete(&["storages", id]).await?;
    Ok((deleted("storage", id, None), None))
}

pub async fn client_create(api: &Api, id: &str, storage: &str) -> CommandResult {
    let row: model::Client = api
        .post(
            &["clients"],
            &ClientCreateBody {
                id,
                storage_id: storage,
            },
        )
        .await?;
    if row.id != id || row.storage_id != storage {
        return Err(Error::applied_invalid_response(201));
    }
    Ok((Data::Client(row), None))
}

pub async fn client_delete(api: &Api, id: &str, yes: bool) -> CommandResult {
    confirm(api, yes, "Delete", &format!("client {id}"))?;
    api.delete(&["clients", id]).await?;
    Ok((deleted("client", id, None), None))
}

pub async fn credential_create(api: &Api, client: &str, path: &Path) -> CommandResult {
    let mut output = crate::secret::SecretOutput::create(path)?;
    let issued: model::IssuedCredential =
        match api.post_empty(&["clients", client, "s3-credentials"]).await {
            Ok(issued) => issued,
            Err(error) if error.outcome == "not_applied" => {
                output.discard();
                return Err(error);
            }
            Err(error) => {
                return Ok((
                    credential_delivery(client, None, &output, output.state()),
                    Some(error),
                ));
            }
        };
    let reported_access_key_id = crate::args::valid_access_key_id(&issued.access_key_id)
        .then(|| issued.access_key_id.clone());
    if reported_access_key_id.is_none() || issued.secret_key.is_empty() {
        return Ok((
            credential_delivery(client, reported_access_key_id, &output, output.state()),
            Some(Error::applied_invalid_response(201)),
        ));
    }
    let access_key_id = issued.access_key_id;
    if output
        .write_credential(client, &access_key_id, &issued.secret_key)
        .is_err()
    {
        return Ok((
            credential_delivery(client, Some(access_key_id), &output, output.state()),
            Some(Error::applied_secret_write()),
        ));
    }
    Ok((
        credential_delivery(client, Some(access_key_id), &output, "saved"),
        None,
    ))
}

pub async fn credential_delete(
    api: &Api,
    client: &str,
    access_key_id: &str,
    yes: bool,
) -> CommandResult {
    confirm(
        api,
        yes,
        "Delete",
        &format!("credential {access_key_id} for client {client}"),
    )?;
    api.delete(&["clients", client, "s3-credentials", access_key_id])
        .await?;
    Ok((
        deleted("credential", access_key_id, Some(client.to_owned())),
        None,
    ))
}

pub async fn client_key_register(api: &Api, client: &str, path: &Path) -> CommandResult {
    let key_hash = crate::input::client_key_hash(path)?;
    let row: model::ClientKey = api
        .post(
            &["clients", client, "keys"],
            &ClientKeyCreateBody {
                key_hash: &key_hash,
            },
        )
        .await?;
    if row.client_id != client || row.key_hash != key_hash {
        return Err(Error::applied_invalid_response(201));
    }
    Ok((Data::ClientKey(row), None))
}

pub async fn client_key_delete(
    api: &Api,
    client: &str,
    key_hash: &str,
    yes: bool,
) -> CommandResult {
    confirm(
        api,
        yes,
        "Delete",
        &format!("client key {key_hash} for client {client}"),
    )?;
    api.delete(&["clients", client, "keys", key_hash]).await?;
    Ok((
        deleted("client-key", key_hash, Some(client.to_owned())),
        None,
    ))
}

fn confirm(api: &Api, yes: bool, action: &str, resource: &str) -> Result<(), Error> {
    crate::confirm::destructive(yes, &api.origin(), action, resource)
}

fn credential_delivery(
    client: &str,
    access_key_id: Option<String>,
    output: &crate::secret::SecretOutput,
    state: &'static str,
) -> Data {
    Data::CredentialDelivery(model::CredentialDelivery {
        client_id: client.to_owned(),
        access_key_id,
        secret_file: output.path().to_owned(),
        file_state: state,
    })
}

fn deleted(resource: &'static str, id: &str, client_id: Option<String>) -> Data {
    Data::Deleted(model::Deleted {
        resource,
        id: id.to_owned(),
        client_id,
    })
}
