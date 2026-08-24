use std::env;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use conversation_memory::{MemoryClock, MemoryStore, SystemMemoryClock};
use conversation_runtime::TextTurnRuntime;
use conversation_runtime_gateway::{
    GatewayAdapters, GatewayConfig, GatewaySession, ProviderSupervisor,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const INPUT_RELAY_BYTES: usize = 64 * 1024;
const INPUT_CHUNK_BYTES: usize = 8 * 1024;
const INPUT_CHUNK_COUNT: usize = INPUT_RELAY_BYTES / INPUT_CHUNK_BYTES;

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
    let provider_hosts = adapters.provider_hosts().to_vec();
    let cancellation = CancellationToken::new();
    let (session_input, input_task) = input_relay(cancellation.clone());
    let mut providers = ProviderSupervisor::start(provider_hosts, cancellation.clone())
        .await
        .map_err(|error| {
            cancellation.cancel();
            eprintln!(
                "gateway provider supervision failed: {}",
                error.diagnostic_code()
            );
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
        status: _,
    } = adapters;
    let runtime = TextTurnRuntime::new(context.clone(), language.clone());
    let mut session = GatewaySession::new(runtime, status);
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
    let session = session.run(session_input, tokio::io::stdout());
    tokio::pin!(session);
    let outcome = tokio::select! {
        biased;
        provider_error = providers.wait_for_exit() => {
            cancellation.cancel();
            let session_result = session.await;
            (session_result, Some(provider_error))
        }
        session_result = &mut session => {
            cancellation.cancel();
            (session_result, None)
        }
    };
    let _ = input_task.await;
    let shutdown = providers.shutdown().await;
    if let Some(error) = outcome.1 {
        eprintln!(
            "gateway provider supervision failed: {}",
            error.diagnostic_code()
        );
        return Err(());
    }
    if let Err(error) = outcome.0 {
        eprintln!("gateway session failed: {}", error.diagnostic_code());
        return Err(());
    }
    if let Err(error) = shutdown {
        eprintln!(
            "gateway provider supervision failed: {}",
            error.diagnostic_code()
        );
        return Err(());
    }
    Ok(())
}

fn input_relay(
    cancellation: CancellationToken,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let (input_sender, mut input_receiver) = mpsc::channel::<Vec<u8>>(INPUT_CHUNK_COUNT);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buffer = [0_u8; INPUT_CHUNK_BYTES];
        loop {
            let count = match stdin.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(count) => count,
            };
            if input_sender
                .blocking_send(buffer[..count].to_vec())
                .is_err()
            {
                return;
            }
        }
    });

    let (session_input, mut input_sink) = tokio::io::duplex(INPUT_RELAY_BYTES);
    let input_cancellation = cancellation.clone();
    let input_task = tokio::spawn(async move {
        let result = loop {
            let chunk = tokio::select! {
                biased;
                _ = input_cancellation.cancelled() => break Ok(()),
                chunk = input_receiver.recv() => chunk,
            };
            let Some(chunk) = chunk else {
                break Ok(());
            };
            if let Err(error) = input_sink.write_all(&chunk).await {
                break Err(error);
            }
        };
        input_cancellation.cancel();
        result
    });
    (session_input, input_task)
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
