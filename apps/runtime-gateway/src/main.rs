use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use conversation_memory::{MemoryClock, MemoryStore, SystemMemoryClock};
use conversation_runtime::TextTurnRuntime;
use conversation_runtime_gateway::{
    input_relay, GatewayAdapters, GatewayConfig, GatewaySession, ProviderSupervisor,
    ProviderSupervisorError,
};
use tokio_util::sync::CancellationToken;

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
        memory_extraction,
        provider_hosts,
        status: _,
    } = adapters;
    let cancellation = CancellationToken::new();
    let relay = input_relay(std::io::stdin(), cancellation.clone());
    let mut providers = match ProviderSupervisor::start(provider_hosts, relay.ended.clone()).await {
        Ok(providers) => providers,
        // The client left before the providers were ready: nothing failed.
        Err(error) if error.is_startup_cancelled() => return Ok(()),
        Err(error) => {
            eprintln!(
                "gateway provider supervision failed: {}",
                error.diagnostic_code()
            );
            return Err(());
        }
    };
    let runtime = TextTurnRuntime::new(context.clone(), language.clone());
    let provider_failure = CancellationToken::new();
    let mut session =
        GatewaySession::new(runtime, status).with_provider_failure(provider_failure.clone());
    if let Some(voice_adapters) = voice {
        session = session.with_voice(voice_adapters, context, language);
    }
    if let Some(store) = memory_store {
        let store: Arc<dyn MemoryStore> = Arc::new(store);
        let clock: Arc<dyn MemoryClock> = Arc::new(SystemMemoryClock);
        session = session.with_memory_inspection(store.clone(), clock.clone());
        if let Some(extraction) = memory_extraction {
            session = session.with_memory_extraction(
                store,
                extraction.language,
                clock,
                extraction.settings,
            );
        }
    }
    let session = session.run(relay.input, tokio::io::stdout());
    tokio::pin!(session);
    let (session_result, provider_error) = tokio::select! {
        biased;
        provider_error = providers.wait_for_exit() => {
            provider_failure.cancel();
            (session.await, Some(provider_error))
        }
        session_result = &mut session => (session_result, None),
    };
    cancellation.cancel();
    let input_result = relay.task.await;
    let shutdown = providers.shutdown().await;
    let failure = provider_error
        .map(supervision_failure)
        .or_else(|| {
            session_result
                .err()
                .map(|error| format!("gateway session failed: {}", error.diagnostic_code()))
        })
        .or_else(|| {
            matches!(input_result, Ok(Err(_)))
                .then(|| "gateway session failed: input_read_failed".to_owned())
        })
        .or_else(|| shutdown.err().map(supervision_failure));
    if let Some(failure) = failure {
        eprintln!("{failure}");
        return Err(());
    }
    Ok(())
}

fn supervision_failure(error: ProviderSupervisorError) -> String {
    format!(
        "gateway provider supervision failed: {}",
        error.diagnostic_code()
    )
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
