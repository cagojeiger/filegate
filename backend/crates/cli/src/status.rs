use serde::Serialize;

use crate::{
    error::Error,
    http::Api,
    model::{Health, Identity, StorageUsage},
};

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Ok,
    Failed,
    Unknown,
}

#[derive(Serialize)]
pub struct Registry {
    pub state: State,
    pub usage: State,
    pub clients: State,
    pub storage_count: Option<usize>,
    pub client_count: Option<usize>,
}

#[derive(Serialize)]
pub struct Status {
    pub server_version: Option<String>,
    pub identity: State,
    pub health: State,
    pub readiness: State,
    pub registry: Registry,
    pub storage_access: &'static str,
}

fn observed<T>(result: &Result<T, Error>) -> State {
    match result {
        Ok(_) => State::Ok,
        Err(error) if error.http_status.is_some_and(|s| s != 200) => State::Failed,
        Err(_) => State::Unknown,
    }
}

pub async fn inspect(api: &Api) -> (Status, Option<Error>) {
    let identity = api.get::<Identity>(&[], &[], false).await;
    let health = api.get::<Health>(&["healthz"], &[], false).await;
    let readiness = api.get::<Health>(&["readyz"], &[], false).await;
    let usage = api.get::<Vec<StorageUsage>>(&["usage"], &[], true).await;
    let clients = api.get::<Vec<String>>(&["clients"], &[], true).await;
    let identity_state = match &identity {
        Ok(i) if i.name != "filegate" || i.version.is_empty() => State::Failed,
        _ => observed(&identity),
    };
    let health_state = match &health {
        Ok(h) if h.status != "ok" => State::Failed,
        _ => observed(&health),
    };
    let readiness_state = match &readiness {
        Ok(h) if h.status != "ready" => State::Failed,
        _ => observed(&readiness),
    };
    let (usage_state, clients_state) = (observed(&usage), observed(&clients));
    let registry_state = match (usage_state, clients_state) {
        (State::Ok, State::Ok) => State::Ok,
        (State::Failed, _) | (_, State::Failed) => State::Failed,
        _ => State::Unknown,
    };
    let unauthorized = [
        usage.as_ref().err(),
        clients.as_ref().err(),
        identity.as_ref().err(),
        health.as_ref().err(),
        readiness.as_ref().err(),
    ]
    .into_iter()
    .flatten()
    .find(|e| e.exit == 3)
    .and_then(|e| e.http_status);
    let ok = [
        identity_state,
        health_state,
        readiness_state,
        registry_state,
    ]
    .iter()
    .all(|s| *s == State::Ok);
    let error = if let Some(status) = unauthorized {
        Some(Error::response(status))
    } else if !ok {
        Some(Error::new(
            "status_failed",
            "One or more required API checks failed or are unknown",
            5,
        ))
    } else {
        None
    };
    (
        Status {
            server_version: identity
                .ok()
                .filter(|_| identity_state == State::Ok)
                .map(|i| i.version),
            identity: identity_state,
            health: health_state,
            readiness: readiness_state,
            registry: Registry {
                state: registry_state,
                usage: usage_state,
                clients: clients_state,
                storage_count: usage.ok().map(|rows| rows.len()),
                client_count: clients.ok().map(|rows| rows.len()),
            },
            storage_access: "not_checked",
        },
        error,
    )
}
