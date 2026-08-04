use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

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
    let GatewayAdapters {
        language,
        quality,
        memory,
    } = config.into_adapters().map_err(|_| {
        eprintln!("gateway adapter initialization failed");
    })?;

    let memory_enabled = memory.is_some();
    let mut runtime = TextTurnRuntime::new(Arc::new(language)).with_quality_controller(quality);
    if let Some(memory) = memory {
        runtime = runtime
            .with_memory_provider(Arc::new(memory), ExecutionLocation::Local)
            .map_err(|_| {
                eprintln!("gateway memory initialization failed");
            })?;
    }
    let status = RuntimeStatus {
        transport: "stdio".to_owned(),
        privacy_mode: "local_only".to_owned(),
        language_location: "local".to_owned(),
        model_id: "configured-local-model".to_owned(),
        memory_enabled,
        memory_location: memory_enabled.then(|| "local".to_owned()),
        telemetry_enabled: false,
        capabilities: vec!["text".to_owned()],
    };
    let session = GatewaySession::new(runtime, status);
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
