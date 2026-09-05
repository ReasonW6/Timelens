#![cfg(windows)]
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use timelens_ai::{
    ChatMessage, Model, Protocol, ProviderProfile, Role, WORKER_PROTOCOL, WorkerEvent,
    WorkerOperation, WorkerRequest,
    credentials::{self, Secrets},
};

struct Credential(String);
impl Drop for Credential {
    fn drop(&mut self) {
        let _ = credentials::delete(&self.0);
    }
}
fn profile(protocol: Protocol, url: String) -> (ProviderProfile, Credential) {
    let id = format!(
        "acceptance-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let secret = Secrets {
        api_key: "synthetic-key-for-local-fixture".into(),
        headers: Default::default(),
    };
    credentials::save(&id, &secret).unwrap();
    let mut p = ProviderProfile::preset(0, id.clone());
    p.protocol = protocol;
    p.base_url = url;
    p.allow_local_http = true;
    p.model = Model::unknown("synthetic-model");
    p.idle_timeout_seconds = Some(10);
    p.validate().unwrap();
    (p, Credential(id))
}
fn request(profile: ProviderProfile) -> WorkerRequest {
    WorkerRequest {
        version: WORKER_PROTOCOL,
        profile,
        operation: WorkerOperation::Generate {
            system: "Synthetic acceptance data only".into(),
            messages: vec![ChatMessage {
                role: Role::User,
                text: "fictional usage summary".into(),
            }],
            images: vec![],
        },
    }
}
fn worker(request: &WorkerRequest) -> std::process::Child {
    let mut child = Command::new(env!("CARGO_BIN_EXE_timelens-ai-worker"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    serde_json::to_writer(child.stdin.take().unwrap(), request).unwrap();
    child
}
fn read_request(stream: &mut std::net::TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut bytes = vec![];
    let mut chunk = [0; 8192];
    loop {
        let read = stream.read(&mut chunk).unwrap();
        assert_ne!(read, 0);
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&bytes[..split]);
            let length = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|s| s.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if bytes.len() >= split + 4 + length {
                break;
            }
        }
    }
    String::from_utf8(bytes).unwrap()
}
fn accept(listener: &TcpListener) -> std::net::TcpStream {
    listener.set_nonblocking(true).unwrap();
    let started = std::time::Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                return stream;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && started.elapsed() < Duration::from_secs(15) =>
            {
                thread::sleep(Duration::from_millis(20))
            }
            other => panic!("worker did not connect to local fixture: {other:?}"),
        }
    }
}
fn roundtrip(
    protocol: Protocol,
    status: &str,
    extra: &str,
    body: &str,
) -> (String, Vec<WorkerEvent>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let response = format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{extra}\r\n{body}",
        body.len()
    );
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        let input = read_request(&mut stream);
        stream.write_all(response.as_bytes()).unwrap();
        input
    });
    let (profile, _credential) = profile(protocol, url);
    let output = worker(&request(profile)).wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    (server.join().unwrap(), events)
}

#[test]
fn ordinary_worker_streams_all_three_protocols_over_real_loopback_http() {
    let cases = [
        (
            Protocol::OpenAi,
            "Authorization: Bearer synthetic-key-for-local-fixture",
            "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\ndata: [DONE]\n\n",
        ),
        (
            Protocol::Anthropic,
            "x-api-key: synthetic-key-for-local-fixture",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
        ),
        (
            Protocol::Gemini,
            "x-goog-api-key: synthetic-key-for-local-fixture",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"你好\"}]},\"finishReason\":\"STOP\"}]}\n\n",
        ),
    ];
    for (protocol, header, body) in cases {
        let (input, events) = roundtrip(
            protocol,
            "200 OK",
            "Content-Type: text/event-stream\r\n",
            body,
        );
        assert!(input.contains(header));
        assert!(input.contains("fictional usage summary"));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, WorkerEvent::Text(s) if s == "你好"))
        );
        assert!(matches!(events.last(), Some(WorkerEvent::Complete)));
    }
}

#[test]
fn worker_preserves_retry_after_and_redacts_credential_errors() {
    let (_, events) = roundtrip(
        Protocol::OpenAi,
        "429 Too Many Requests",
        "Retry-After: 23\r\nContent-Type: application/json\r\n",
        "synthetic-key-for-local-fixture rate limited",
    );
    let failure = events
        .iter()
        .find_map(|e| {
            if let WorkerEvent::Failure(f) = e {
                Some(f)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(failure.retry_after_seconds, Some(23));
    assert_eq!(failure.status, Some(429));
    assert!(failure.retryable);
    assert!(!failure.message.contains("synthetic-key-for-local-fixture"));
    assert!(!events.iter().any(|e| matches!(e, WorkerEvent::Complete)));
}

#[test]
fn canceling_worker_closes_a_stalled_stream_without_waiting_for_provider() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let (ready, received) = mpsc::channel();
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        read_request(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        ready.send(()).unwrap();
        let mut byte = [0];
        loop {
            match stream.read(&mut byte) {
                Ok(0) => break true,
                Ok(_) => continue,
                Err(error) => {
                    break matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    );
                }
            }
        }
    });
    let (profile, _credential) = profile(Protocol::OpenAi, url);
    let mut child = worker(&request(profile));
    received.recv_timeout(Duration::from_secs(10)).unwrap();
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    assert!(server.join().unwrap());
}
