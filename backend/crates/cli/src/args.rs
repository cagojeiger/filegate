use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "gscli", version, about = "Grove Storage management CLI")]
pub struct Args {
    /// Management API origin.
    #[arg(long, global = true, env = "GROVE_ENDPOINT", hide_env_values = true)]
    pub endpoint: Option<String>,
    /// Read the operator token from a file (overrides GROVE_OPERATOR_TOKEN).
    #[arg(long, global = true)]
    pub token_file: Option<PathBuf>,
    #[arg(long, global = true, value_enum, default_value = "table")]
    pub output: Output,
    /// Whole-command HTTP deadline in seconds.
    #[arg(long, global = true, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=86400))]
    pub timeout: u64,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Output {
    Table,
    Json,
}

#[derive(Subcommand)]
pub enum Command {
    /// Install the latest stable official CLI release (independent of the server).
    Update {
        /// Check for a newer version without changing the installation.
        #[arg(long)]
        check: bool,
    },
    #[command(name = "__install", hide = true)]
    Install {
        #[arg(long)]
        bin_dir: PathBuf,
    },
    /// Inspect the remote API and registry (not physical storage).
    Status,
    /// Inspect registered storage backends.
    #[command(subcommand)]
    Storage(Resource),
    /// Inspect registered clients and their storage assignments.
    #[command(subcommand)]
    Client(Resource),
    /// List public S3 credential IDs for a client.
    #[command(subcommand)]
    Credential(ClientList),
    /// List registered native API key hashes for a client.
    #[command(subcommand)]
    ClientKey(ClientList),
    /// Inspect storage and client usage.
    #[command(subcommand)]
    Usage(Usage),
}

#[derive(Subcommand)]
pub enum Resource {
    List,
    Show {
        #[arg(value_parser = resource_id)]
        id: String,
    },
}

#[derive(Subcommand)]
pub enum ClientList {
    List {
        #[arg(long, value_parser = resource_id)]
        client: String,
    },
}

#[derive(Subcommand)]
pub enum Usage {
    Storages,
    Clients,
    History {
        #[arg(long, default_value_t = 90, value_parser = clap::value_parser!(u16).range(1..=3650))]
        days: u16,
    },
}

fn resource_id(value: &str) -> Result<String, &'static str> {
    if value.is_empty() || matches!(value, "." | "..") || value.chars().any(char::is_control) {
        return Err("expected a nonempty resource ID without dot segments or control characters");
    }
    Ok(value.to_owned())
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Update { check: true } => "update.check",
            Self::Update { check: false } => "update",
            Self::Install { .. } => "install",
            Self::Status => "status",
            Self::Storage(Resource::List) => "storage.list",
            Self::Storage(Resource::Show { .. }) => "storage.show",
            Self::Client(Resource::List) => "client.list",
            Self::Client(Resource::Show { .. }) => "client.show",
            Self::Credential(_) => "credential.list",
            Self::ClientKey(_) => "client-key.list",
            Self::Usage(Usage::Storages) => "usage.storages",
            Self::Usage(Usage::Clients) => "usage.clients",
            Self::Usage(Usage::History { .. }) => "usage.history",
        }
    }
}
