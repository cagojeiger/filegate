mod args;
mod commands;
mod config;
mod error;
mod http;
mod model;
mod output;
mod status;
mod update;

use std::process::ExitCode;

use clap::Parser;

use args::Args;
use output::Envelope;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let mut envelope = Envelope {
        schema_version: 1,
        ok: false,
        command: args.command.name(),
        endpoint: None,
        data: None,
        error: None,
    };
    match execute(&args, &mut envelope).await {
        Ok(()) => envelope.ok = envelope.error.is_none(),
        Err(error) => envelope.error = Some(error),
    }
    let exit = envelope.error.as_ref().map_or(0, |error| error.exit);
    if output::emit(&envelope, args.output).is_err() {
        let applied = envelope.applied();
        if applied {
            eprintln!("gscli: CLI was changed, but command output failed");
        } else {
            eprintln!("gscli: cannot write command output");
        }
        return ExitCode::from(if applied { 8 } else { 1 });
    }
    ExitCode::from(exit)
}

async fn execute(args: &Args, envelope: &mut Envelope) -> Result<(), error::Error> {
    match &args.command {
        args::Command::Update { check } => {
            envelope.data = Some(model::Data::Update(
                update::run(*check, args.timeout).await?,
            ));
            return Ok(());
        }
        args::Command::Install { bin_dir } => {
            envelope.data = Some(model::Data::Installation {
                path: update::install(bin_dir)?,
            });
            return Ok(());
        }
        _ => {}
    }
    let endpoint = config::endpoint(args.endpoint.as_deref())?;
    envelope.endpoint = Some(endpoint.origin().ascii_serialization());
    let api = http::Api::new(endpoint, config::authorization(args)?, args.timeout)?;
    let (data, error) = commands::run(&api, &args.command).await?;
    envelope.data = Some(data);
    envelope.error = error;
    Ok(())
}
