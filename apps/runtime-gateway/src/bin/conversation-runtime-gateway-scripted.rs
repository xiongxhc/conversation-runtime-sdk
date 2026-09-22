//! Scripted E2E executable, not the production CLI.
//!
//! This binary exists so an end-to-end run can reach language, transcription, speech and
//! diarization peers on loopback while every engine path — capture, recognition, alignment,
//! reconciliation and finalization — stays the real one. It composes the same public
//! `GatewaySession`, the same framed stdio control, the same `MediaServer` and the same
//! provider adapters as `conversation-runtime-gateway`, and it admits the `online` profile, so
//! a client that pins `online` is accepted unchanged.
//!
//! The one difference is the configuration surface: this binary reads a file that carries a
//! `[scripted_providers]` table, which the production loader refuses. The production binary is
//! not built with the `scripted-providers` feature and has no flag that reaches this mode.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use conversation_memory::{MemoryClock, MemoryStore, SystemMemoryClock};
use conversation_runtime::TextTurnRuntime;
use conversation_runtime_gateway::{
    input_relay, GatewayAdapters, GatewaySession, MediaServer, ProviderSupervisor,
    ProviderSupervisorError, ScriptedGatewayConfig,
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
    eprintln!("scripted E2E executable, not the production CLI");
    let (config_path, protocol_v4) = parse_arguments(env::args_os().skip(1)).ok_or_else(|| {
        eprintln!("gateway arguments are invalid");
    })?;
    let adapters = ScriptedGatewayConfig::load(&config_path).map_err(|error| {
        eprintln!("gateway configuration failed: {error}");
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
        recording,
        diarizer,
        media,
        status: _,
    } = adapters;
    let media_server = match media {
        Some(config) => Some(Arc::new(MediaServer::bind(config).await.map_err(|_| {
            eprintln!("gateway media server failed");
        })?)),
        None => None,
    };
    let cancellation = CancellationToken::new();
    let relay = input_relay(std::io::stdin(), cancellation.clone());
    let mut providers = match ProviderSupervisor::start(provider_hosts, relay.ended.clone()).await {
        Ok(providers) => providers,
        Err(error) if error.is_startup_cancelled() => {
            if let Some(server) = media_server.as_ref() {
                server.shutdown().await;
            }
            return Ok(());
        }
        Err(error) => {
            if let Some(server) = media_server.as_ref() {
                server.shutdown().await;
            }
            eprintln!(
                "gateway provider supervision failed: {}",
                error.diagnostic_code()
            );
            return Err(());
        }
    };
    let runtime = TextTurnRuntime::new(context, language.clone());
    let provider_failure = CancellationToken::new();
    let mut session =
        GatewaySession::new(runtime, status).with_provider_failure(provider_failure.clone());
    if protocol_v4 {
        session = session.with_v4_control();
    }
    session = session.with_control_closed(relay.ended.clone());
    if let Some(voice_adapters) = voice {
        session = session.with_voice(voice_adapters, language);
    }
    if let Some(media_server) = media_server.as_ref() {
        session = session.with_media_server(Arc::clone(media_server));
    }
    if let Some(recording) = recording {
        session = session.with_recording(recording);
    }
    if let Some(diarizer) = diarizer {
        session = session.with_reconciliation(diarizer);
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
    if let Some(server) = media_server.as_ref() {
        server.shutdown().await;
    }
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

fn parse_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Option<(PathBuf, bool)> {
    if arguments.next()?.to_str()? != "--config" {
        return None;
    }
    let path = PathBuf::from(arguments.next()?);
    let protocol_v4 = match arguments.next() {
        None => false,
        Some(option) if option == "--client-protocol-version" => {
            if arguments.next()? != "4" {
                return None;
            }
            true
        }
        Some(_) => return None,
    };
    if arguments.next().is_some() {
        return None;
    }
    Some((path, protocol_v4))
}
