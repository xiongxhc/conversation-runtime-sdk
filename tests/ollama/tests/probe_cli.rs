use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::thread;
use std::time::Duration;

#[test]
fn exits_non_zero_with_structured_first_delta_timeout() {
    let server = StalledOllamaServer::start();
    let output = Command::new(env!("CARGO_BIN_EXE_conversation-ollama-probe"))
        .args(["test-model", "prompt"])
        .env("OLLAMA_ENDPOINT", server.endpoint())
        .env("OLLAMA_FIRST_DELTA_TIMEOUT_MS", "20")
        .env("OLLAMA_IDLE_TIMEOUT_MS", "100")
        .env("OLLAMA_TOTAL_TIMEOUT_MS", "100")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8(output.stdout).unwrap().is_empty());

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("model=test-model\n"));
    assert!(stderr.contains("status=timeout\n"));
    assert!(stderr.contains("timeout_stage=first_delta\n"));
    assert!(stderr.contains("elapsed_ms="));
}

struct StalledOllamaServer {
    endpoint: String,
    worker: thread::JoinHandle<()>,
}

impl StalledOllamaServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n\r\n")
                .unwrap();
            thread::sleep(Duration::from_millis(100));
        });

        Self { endpoint, worker }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl Drop for StalledOllamaServer {
    fn drop(&mut self) {
        let worker = std::mem::replace(&mut self.worker, thread::spawn(|| {}));
        worker.join().unwrap();
    }
}

fn read_request(stream: &mut TcpStream) {
    let mut buffer = [0_u8; 1024];
    let mut request = Vec::new();
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buffer).unwrap();
        request.extend_from_slice(&buffer[..read]);
    }
}
