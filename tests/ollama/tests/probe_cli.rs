use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn exits_non_zero_with_structured_first_delta_timeout() {
    let server = StalledOllamaServer::start();
    let output = run_probe(
        Command::new(env!("CARGO_BIN_EXE_conversation-ollama-probe"))
            .args(["test-model", "prompt"])
            .env("OLLAMA_ENDPOINT", server.endpoint())
            .env("OLLAMA_FIRST_DELTA_TIMEOUT_MS", "20")
            .env("OLLAMA_IDLE_TIMEOUT_MS", "100")
            .env("OLLAMA_TOTAL_TIMEOUT_MS", "100"),
    );

    assert!(!output.status.success());
    assert!(String::from_utf8(output.stdout).unwrap().is_empty());

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("model=test-model\n"));
    assert!(stderr.contains("status=timeout\n"), "{stderr}");
    assert!(stderr.contains("timeout_stage=first_delta\n"));
    assert!(stderr.contains("elapsed_ms="));
    server.finish();
}

#[test]
fn reports_an_overflowing_timeout_as_a_structured_configuration_failure() {
    let output = run_probe(
        Command::new(env!("CARGO_BIN_EXE_conversation-ollama-probe"))
            .args(["test-model", "prompt"])
            .env("OLLAMA_TOTAL_TIMEOUT_MS", "18446744073709551615"),
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("model=test-model\n"));
    assert!(stderr.contains("status=error\nstage=configuration\n"));
    assert!(stderr.contains("error=OLLAMA_TOTAL_TIMEOUT_MS"));
}

#[test]
fn rejects_control_character_model_identifiers_without_breaking_structured_output() {
    let output = run_probe(
        Command::new(env!("CARGO_BIN_EXE_conversation-ollama-probe"))
            .args(["test\nmodel", "prompt"]),
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().next(), Some("model=unavailable"));
    assert!(stderr.contains("status=error\nstage=arguments\n"));
}

struct StalledOllamaServer {
    endpoint: String,
    worker: thread::JoinHandle<()>,
    completed: mpsc::Receiver<std::io::Result<()>>,
}

impl StalledOllamaServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (completed_sender, completed) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = accept_and_stall(listener);
            let _ = completed_sender.send(result);
        });

        Self {
            endpoint,
            worker,
            completed,
        }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }
    fn finish(self) {
        self.completed
            .recv_timeout(Duration::from_secs(1))
            .expect("stalled server must finish within its deadline")
            .unwrap();
        self.worker.join().unwrap();
    }
}

fn run_probe(command: &mut Command) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);

    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("probe subprocess exceeded its test deadline");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn accept_and_stall(listener: TcpListener) -> std::io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "probe never connected to stalled server",
                    ));
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    };
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    read_request(&mut stream)?;
    stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: 1\r\n\r\n",
    )?;
    thread::sleep(Duration::from_millis(250));
    Ok(())
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut buffer = [0_u8; 1024];
    let mut request = Vec::new();
    let header_end = loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "probe closed before sending request headers",
            ));
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(header_end) = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
        {
            break header_end;
        }
    };
    let content_length = request[..header_end]
        .windows("Content-Length:".len())
        .position(|window| window.eq_ignore_ascii_case(b"content-length:"))
        .map(|start| {
            let value = &request[start + "Content-Length:".len()..header_end];
            std::str::from_utf8(value)
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap()
        })
        .unwrap();

    while request.len() - header_end < content_length {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "probe closed before sending its request body",
            ));
        }
        request.extend_from_slice(&buffer[..read]);
    }
    Ok(())
}
