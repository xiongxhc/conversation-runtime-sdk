//! Listener ownership tests; no non-loopback socket or TLS material is used here.

use std::net::SocketAddr;
use std::time::Duration;

use conversation_protocol::SessionId;
use conversation_runtime_gateway::{MediaServer, MediaServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Catches the production break where the gateway cancels only its accept
/// loop, leaving an authenticated media-bridge task detached after shutdown.
#[tokio::test]
async fn shutdown_closes_an_authenticated_loopback_connection_and_joins_it() {
    let server = MediaServer::bind(MediaServerConfig::loopback())
        .await
        .unwrap();
    let lease = server.reserve(SessionId::new(601)).await.unwrap();
    let mut bridge = TcpStream::connect(lease.endpoint()).await.unwrap();
    bridge
        .write_all(&authentication(lease.session_id(), lease.credential()))
        .await
        .unwrap();

    server.shutdown().await;

    let mut byte = [0_u8; 1];
    match timeout(Duration::from_millis(250), bridge.read(&mut byte))
        .await
        .unwrap()
    {
        Ok(0) | Err(_) => {}
        Ok(count) => panic!("shutdown left media bytes readable: {count}"),
    }
}

/// Catches unbounded pre-authentication tasks and a shutdown that waits forever on peers that
/// never send a token. This is loopback-only and deliberately exercises no TLS material.
#[tokio::test]
async fn shutdown_reaps_bounded_idle_connection_handshakes() {
    let server = MediaServer::bind(MediaServerConfig::loopback())
        .await
        .unwrap();
    let endpoint: SocketAddr = server
        .advertised_endpoint()
        .strip_prefix("tcp://")
        .unwrap()
        .parse()
        .unwrap();
    let mut peers = Vec::new();
    for _ in 0..65 {
        peers.push(TcpStream::connect(endpoint).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    timeout(Duration::from_millis(500), server.shutdown())
        .await
        .expect("idle bridge handshakes prevented bounded media shutdown");
    drop(peers);
}

fn authentication(session_id: SessionId, credential: &str) -> Vec<u8> {
    let mut bytes = vec![b'M', b'E', b'D', b'1', 1, 1];
    bytes.extend_from_slice(&session_id.get().to_be_bytes());
    bytes.push(credential.len().try_into().unwrap());
    bytes.extend_from_slice(credential.as_bytes());
    bytes
}
