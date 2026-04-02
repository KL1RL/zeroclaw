use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::Value;
use std::ffi::OsString;
use std::path::PathBuf;
use zeroclaw::home_assistant_client::{
    read_home_assistant_token_file, read_home_assistant_token_from_environment, HomeAssistantClient,
};

#[derive(Parser, Debug)]
#[command(
    name = "ha-rcli",
    about = "Debug-only Home Assistant REST CLI",
    long_about = r#"Debug-only Home Assistant REST CLI.

This utility talks directly to the Home Assistant REST API using the same
request-building and token parsing logic as ZeroClaw's built-in
home_assistant tool.

Token source:
- By default, reads HOME_ASSISTANT_ACCESS_TOKEN, HOME_ASSISTANT_TOKEN, or
  HOME_ASSISTANT_API_KEY from the process environment.
- With -t/--token-file, reads those same keys from an env-style file such as
  ~/.zeroclaw/workspace/.env.

Global arguments:
- -u, --url <URL>           Home Assistant base URL
- -n, --nostrict            Disable strict TLS certificate validation
- -t, --token-file <PATH>   Read token from env-style file

Commands:
- get-config                GET /api/config
- list-services             GET /api/services
- list-states [--domain]    GET /api/states
- get-state <ENTITY_ID>     GET /api/states/{entity_id}
- call-service              POST /api/services/{domain}/{service}

Security warning: this binary is intended for debugging only. Exposing a Home
Assistant access token via environment variables or token files is dangerous
on a shared host."#,
    after_help = r#"Examples:
  ha-rcli -u http://homeassistant.local:8123 -t ~/.zeroclaw/workspace/.env get-config
  ha-rcli -u http://homeassistant.local:8123 -t ~/.zeroclaw/workspace/.env list-states --domain light
  ha-rcli -u http://homeassistant.local:8123 -t ~/.zeroclaw/workspace/.env get-state light.kitchen
  ha-rcli -u http://homeassistant.local:8123 -t ~/.zeroclaw/workspace/.env call-service --domain light --service turn_off --entity-id light.kitchen
  ha-rcli -u https://ha.example.com -nostrict call-service --domain climate --service set_temperature --entity-id climate.living_room --data '{"temperature":72}'
"#
)]
struct Cli {
    /// Home Assistant base URL, e.g. https://ha.example.com
    #[arg(short = 'u', long = "url")]
    url: String,

    /// Disable strict TLS certificate validation
    #[arg(short = 'n', long = "nostrict")]
    nostrict: bool,

    /// Read the token from an env-style file instead of process environment
    #[arg(short = 't', long = "token-file", value_name = "PATH")]
    token_file: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// GET /api/config
    GetConfig,
    /// GET /api/services
    ListServices,
    /// GET /api/states
    ListStates {
        /// Optional domain filter, e.g. light
        #[arg(long)]
        domain: Option<String>,
    },
    /// GET /api/states/{entity_id}
    GetState {
        /// Entity ID, e.g. light.kitchen
        entity_id: String,
    },
    /// POST /api/services/{domain}/{service}
    CallService {
        /// Service domain, e.g. light
        #[arg(long)]
        domain: String,

        /// Service name, e.g. turn_off
        #[arg(long)]
        service: String,

        /// Shorthand target entity ID, e.g. light.kitchen
        #[arg(long)]
        entity_id: Option<String>,

        /// Extra service payload as a JSON object string
        #[arg(long)]
        data: Option<String>,

        /// Target object as a JSON object string
        #[arg(long)]
        target: Option<String>,

        /// Request response data for services that explicitly support it
        #[arg(long)]
        return_response: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse_from(normalized_args());

    eprintln!(
        "WARNING: ha-rcli is a debug-only utility. Exposing a Home Assistant access token via environment variables or token files is inherently dangerous on a shared host."
    );

    let token = match cli.token_file.as_deref() {
        Some(path) => read_home_assistant_token_file(&expand_path(path)).await?,
        None => read_home_assistant_token_from_environment()?,
    };

    let client = HomeAssistantClient::new(&cli.url, token, cli.nostrict)?;
    let value = match cli.command {
        Command::GetConfig => client.get_config().await?,
        Command::ListServices => client.list_services().await?,
        Command::ListStates { domain } => client.list_states(domain.as_deref()).await?,
        Command::GetState { entity_id } => client.get_state(&entity_id).await?,
        Command::CallService {
            domain,
            service,
            entity_id,
            data,
            target,
            return_response,
        } => {
            let data = parse_json_object(data.as_deref(), "--data")?;
            let target = parse_json_object(target.as_deref(), "--target")?;
            client
                .call_service(
                    &domain,
                    &service,
                    entity_id.as_deref(),
                    data.as_ref(),
                    target.as_ref(),
                    return_response.then_some(true),
                )
                .await?
        }
    };

    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn normalized_args() -> Vec<OsString> {
    std::env::args_os()
        .map(|arg| {
            if arg == "-nostrict" {
                OsString::from("--nostrict")
            } else {
                arg
            }
        })
        .collect()
}

fn expand_path(raw: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(raw).into_owned())
}

fn parse_json_object(raw: Option<&str>, flag_name: &str) -> Result<Option<Value>> {
    raw.map(|raw| {
        let value: Value = serde_json::from_str(raw)
            .with_context(|| format!("Failed to parse {flag_name} as JSON"))?;
        if !value.is_object() {
            anyhow::bail!("{flag_name} must be a JSON object");
        }
        Ok(value)
    })
    .transpose()
}
