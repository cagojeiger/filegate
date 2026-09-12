mod read;
mod write;

use crate::args::{ClientCommand, ClientKeyCommand, Command, CredentialCommand, StorageCommand};
use crate::error::Error;
use crate::http::Api;
use crate::model::Data;

pub(super) type CommandResult = Result<(Data, Option<Error>), Error>;

pub async fn run(api: &Api, command: &Command) -> CommandResult {
    match command {
        Command::Update { .. } | Command::Install { .. } => Err(Error::input(
            "Local commands execute without the management API",
        )),
        Command::Status => {
            let (status, error) = crate::status::inspect(api).await;
            Ok((Data::Status(status), error))
        }
        Command::Storage(StorageCommand::List) => read::storage_list(api).await,
        Command::Storage(StorageCommand::Show { id }) => read::storage_show(api, id).await,
        Command::Storage(StorageCommand::Create { id, from }) => {
            write::storage_create(api, id, from).await
        }
        Command::Storage(StorageCommand::Replace { id, from, yes }) => {
            write::storage_replace(api, id, from, *yes).await
        }
        Command::Storage(StorageCommand::Delete { id, yes }) => {
            write::storage_delete(api, id, *yes).await
        }
        Command::Client(ClientCommand::List) => read::client_list(api).await,
        Command::Client(ClientCommand::Show { id }) => read::client_show(api, id).await,
        Command::Client(ClientCommand::Create { id, storage }) => {
            write::client_create(api, id, storage).await
        }
        Command::Client(ClientCommand::Delete { id, yes }) => {
            write::client_delete(api, id, *yes).await
        }
        Command::Credential(CredentialCommand::List { client }) => {
            read::credential_list(api, client).await
        }
        Command::Credential(CredentialCommand::Create { client, secret_out }) => {
            write::credential_create(api, client, secret_out).await
        }
        Command::Credential(CredentialCommand::Delete {
            client,
            access_key_id,
            yes,
        }) => write::credential_delete(api, client, access_key_id, *yes).await,
        Command::ClientKey(ClientKeyCommand::List { client }) => {
            read::client_key_list(api, client).await
        }
        Command::ClientKey(ClientKeyCommand::Register { client, key_file }) => {
            write::client_key_register(api, client, key_file).await
        }
        Command::ClientKey(ClientKeyCommand::Delete {
            client,
            key_hash,
            yes,
        }) => write::client_key_delete(api, client, key_hash, *yes).await,
        Command::Usage(usage) => read::usage(api, usage).await,
    }
}
