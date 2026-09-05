use std::io::{self, Read, Write};
use timelens_ai::{AiFailure, MAX_FRAME_BYTES, WorkerEvent, WorkerRequest};

fn main() {
    let result = run();
    if let Err(message) = result {
        let _ = emit(&WorkerEvent::Failure(AiFailure {
            status: None,
            message,
            retry_after_seconds: None,
            retryable: false,
        }));
        std::process::exit(1);
    }
}
fn emit(event: &WorkerEvent) -> Result<(), String> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, event).map_err(|_| "AI 输出管道关闭")?;
    out.write_all(b"\n")
        .and_then(|_| out.flush())
        .map_err(|_| "AI 输出管道关闭".into())
}
#[cfg(windows)]
fn run() -> Result<(), String> {
    timelens_ai::credentials::require_ordinary_privilege()?;
    let mut bytes = vec![];
    io::stdin()
        .lock()
        .take(MAX_FRAME_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "无法读取 AI 请求")?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err("AI 请求超过管道限制".into());
    }
    let request: WorkerRequest = serde_json::from_slice(&bytes).map_err(|_| "AI 请求协议无效")?;
    drop(bytes);
    timelens_ai::transport::execute(&request, emit)
}
#[cfg(not(windows))]
fn run() -> Result<(), String> {
    Err("Timelens AI requires Windows 11".into())
}
