use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use conversation_memory::SystemMemoryClock;
use conversation_protocol::{ExecutionLocation, RuntimeStatus};
use conversation_runtime::TextTurnRuntime;
use conversation_runtime_gateway::{GatewayAdapters, GatewayConfig, GatewaySession};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(()) => ExitCode::FAILURE,
    }
}

async fn run() -> Result<(), ()> {
    let config_path = parse_config_path().ok_or_else(|| {
        eprintln!("gateway arguments are invalid");
    })?;
    let config = GatewayConfig::load(&config_path).map_err(|_| {
        eprintln!("gateway configuration failed");
    })?;
    let adapters: GatewayAdapters = config.into_adapters().map_err(|_| {
        eprintln!("gateway adapter initialization failed");
    })?;

    let model_id = adapters.model_id().to_owned();
    let mut runtime =
        TextTurnRuntime::new(Arc::new(adapters.language)).with_quality_controller(adapters.quality);
    let memory_store = match (adapters.memory_provider, adapters.memory_store) {
        (Some(provider), Some(store)) => {
            runtime = runtime
                .with_memory_provider(Arc::new(provider), ExecutionLocation::Local)
                .map_err(|_| {
                    eprintln!("gateway memory initialization failed");
                })?;
            Some(store)
        }
        (None, None) => None,
        _ => {
            eprintln!("gateway memory initialization failed");
            return Err(());
        }
    };
    let memory_enabled = memory_store.is_some();
    let mut capabilities = vec!["text".to_owned()];
    if memory_enabled {
        capabilities.push("memory_inspection".to_owned());
    }
    let status = RuntimeStatus {
        transport: "stdio".to_owned(),
        privacy_mode: "local_only".to_owned(),
        language_location: "local".to_owned(),
        model_id,
        memory_enabled,
        memory_location: memory_enabled.then(|| "local".to_owned()),
        telemetry_enabled: false,
        capabilities,
    };
    let mut session = GatewaySession::new(runtime, status);
    if let Some(store) = memory_store {
        session = session.with_memory_inspection(Arc::new(store), Arc::new(SystemMemoryClock));
    }
    session
        .run(tokio::io::stdin(), tokio::io::stdout())
        .await
        .map_err(|_| {
            eprintln!("gateway session failed");
        })
}

fn parse_config_path() -> Option<PathBuf> {
    let mut arguments = env::args_os().skip(1);
    if arguments.next()?.to_str()? != "--config" {
        return None;
    }
    let path = PathBuf::from(arguments.next()?);
    if arguments.next().is_some() {
        return None;
    }
    Some(path)
}
