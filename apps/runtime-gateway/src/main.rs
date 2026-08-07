use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use conversation_memory::SystemMemoryClock;
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
    let adapters = GatewayConfig::load(&config_path).map_err(|_| {
        eprintln!("gateway configuration failed");
    })?;
    let status = if adapters.voice.is_some() {
        adapters.status.clone()
    } else {
        adapters.text_only_status()
    };
    let GatewayAdapters {
        context,
        language,
        voice,
        memory_store,
        status: _,
    } = adapters;
    let runtime = TextTurnRuntime::new(context.clone(), language.clone());
    let mut session = GatewaySession::new(runtime, status);
    if let Some(voice_adapters) = voice {
        session = session.with_voice(voice_adapters, context, language);
    }
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
