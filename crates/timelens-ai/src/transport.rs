use crate::{
    credentials::{self, Secrets},
    protocol::{self, SseDecoder, StreamParser},
    *,
};
use std::{
    ffi::c_void,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{Foundation::GetLastError, Networking::WinHttp::*};
use zeroize::Zeroizing;

struct Internet(*mut c_void);
impl Drop for Internet {
    fn drop(&mut self) {
        unsafe {
            WinHttpCloseHandle(self.0);
        }
    }
}
fn handle(raw: *mut c_void) -> Result<Internet, AiFailure> {
    if raw.is_null() {
        Err(network_failure())
    } else {
        Ok(Internet(raw))
    }
}
fn check(result: i32) -> Result<(), AiFailure> {
    if result == 0 {
        Err(network_failure())
    } else {
        Ok(())
    }
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn network_failure() -> AiFailure {
    AiFailure {
        status: None,
        message: format!("网络传输失败（Windows {}）", unsafe {
            GetLastError()
        }),
        retry_after_seconds: None,
        retryable: true,
    }
}
fn invalid(message: impl Into<String>) -> AiFailure {
    AiFailure {
        status: None,
        message: message.into(),
        retry_after_seconds: None,
        retryable: false,
    }
}

pub fn execute(
    request: &WorkerRequest,
    mut emit: impl FnMut(&WorkerEvent) -> Result<(), String>,
) -> Result<(), String> {
    credentials::require_ordinary_privilege()?;
    let secrets = credentials::load(&request.profile.id)?;
    match execute_with_secrets(request, &secrets, &mut emit) {
        Ok(()) => Ok(()),
        Err(mut failure) => {
            failure.message = protocol::redact_error(&failure.message, &secrets.values());
            emit(&WorkerEvent::Failure(failure))
        }
    }
}

fn execute_with_secrets(
    request: &WorkerRequest,
    secrets: &Secrets,
    emit: &mut impl FnMut(&WorkerEvent) -> Result<(), String>,
) -> Result<(), AiFailure> {
    secrets.validate().map_err(invalid)?;
    let wire = protocol::build_request(request).map_err(invalid)?;
    let local = matches!(wire.url.host_str(), Some("localhost" | "127.0.0.1"));
    let session = handle(unsafe {
        WinHttpOpen(
            wide("Timelens/0.1").as_ptr(),
            if local {
                WINHTTP_ACCESS_TYPE_NO_PROXY
            } else {
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY
            },
            null(),
            null(),
            0,
        )
    })?;
    // No overall generation deadline. Only transport inactivity is optional.
    let idle_ms = request
        .profile
        .idle_timeout_seconds
        .map_or(0, |seconds| (seconds * 1000) as i32);
    check(unsafe { WinHttpSetTimeouts(session.0, 15_000, 15_000, 30_000, idle_ms) })?;
    let connection = handle(unsafe {
        WinHttpConnect(
            session.0,
            wide(wire.url.host_str().unwrap()).as_ptr(),
            wire.url.port_or_known_default().unwrap(),
            0,
        )
    })?;
    let path = format!(
        "{}{}",
        wire.url.path(),
        wire.url.query().map_or(String::new(), |q| format!("?{q}"))
    );
    let http = handle(unsafe {
        WinHttpOpenRequest(
            connection.0,
            wide(wire.method).as_ptr(),
            wide(&path).as_ptr(),
            null(),
            null(),
            null(),
            if wire.url.scheme() == "https" {
                WINHTTP_FLAG_SECURE
            } else {
                0
            },
        )
    })?;
    // Never forward credentials or activity to a redirected host.
    let redirects = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
    check(unsafe {
        WinHttpSetOption(
            http.0,
            WINHTTP_OPTION_REDIRECT_POLICY,
            (&redirects as *const u32).cast(),
            4,
        )
    })?;
    let mut headers = Zeroizing::new(
        "Content-Type: application/json\r\nAccept: text/event-stream, application/json\r\n"
            .to_owned(),
    );
    if !secrets.api_key.is_empty() {
        let (name, prefix) = match request.profile.protocol {
            Protocol::OpenAi if request.profile.azure => ("api-key", ""),
            Protocol::OpenAi => ("Authorization", "Bearer "),
            Protocol::Anthropic => ("x-api-key", ""),
            Protocol::Gemini => ("x-goog-api-key", ""),
        };
        headers.push_str(&format!("{name}: {prefix}{}\r\n", secrets.api_key));
    }
    if request.profile.protocol == Protocol::Anthropic {
        headers.push_str("anthropic-version: 2023-06-01\r\n");
    }
    for (name, value) in &secrets.headers {
        headers.push_str(&format!("{name}: {value}\r\n"));
    }
    let wide_headers = Zeroizing::new(wide(&headers));
    let body = wire.body.as_deref().unwrap_or(&[]);
    check(unsafe {
        WinHttpSendRequest(
            http.0,
            wide_headers.as_ptr(),
            (wide_headers.len() - 1) as u32,
            if body.is_empty() {
                null()
            } else {
                body.as_ptr().cast()
            },
            body.len() as u32,
            body.len() as u32,
            0,
        )
    })?;
    check(unsafe { WinHttpReceiveResponse(http.0, null_mut()) })?;
    let status = header(&http, WINHTTP_QUERY_STATUS_CODE)
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or_else(|| invalid("提供商响应缺少状态码"))?;
    let successful = (200..300).contains(&status);
    let is_models = matches!(request.operation, WorkerOperation::Models);
    let is_sse = header(&http, WINHTTP_QUERY_CONTENT_TYPE)
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"));
    let mut decoder = SseDecoder::default();
    let mut parser = StreamParser::new(request.profile.protocol);
    let mut buffered = Vec::new();
    let mut received = 0usize;
    loop {
        let mut buffer = [0u8; 8192];
        let mut read = 0;
        check(unsafe {
            WinHttpReadData(
                http.0,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                &mut read,
            )
        })?;
        if read == 0 {
            break;
        }
        received = received.saturating_add(read as usize);
        if received > MAX_OUTPUT_BYTES {
            return Err(invalid("提供商输出超过 16 MiB，已停止本次请求"));
        }
        if successful && !is_models && is_sse {
            for data in decoder.push(&buffer[..read as usize]).map_err(invalid)? {
                for event in parser.parse(&data).map_err(invalid)? {
                    emit(&event).map_err(invalid)?;
                }
            }
        } else {
            buffered.extend_from_slice(&buffer[..read as usize]);
            if !successful && buffered.len() >= 32_768 {
                break;
            }
        }
    }
    if !successful {
        let retry_after_seconds = header(&http, WINHTTP_QUERY_RETRY_AFTER).and_then(|s| {
            s.parse().ok().or_else(|| {
                chrono::DateTime::parse_from_rfc2822(&s)
                    .ok()
                    .map(|time| (time.timestamp() - chrono::Utc::now().timestamp()).max(0) as u64)
            })
        });
        return Err(AiFailure {
            status: Some(status),
            message: protocol::redact_error(&String::from_utf8_lossy(&buffered), &secrets.values()),
            retry_after_seconds,
            retryable: status == 429 || status >= 500,
        });
    }
    if is_models {
        let json =
            serde_json::from_slice(&buffered).map_err(|_| invalid("模型目录不是有效 JSON"))?;
        emit(&WorkerEvent::Models(protocol::parse_models(
            request.profile.protocol,
            &json,
        )))
        .map_err(invalid)?;
    } else {
        if is_sse {
            for data in decoder.finish().map_err(invalid)? {
                for event in parser.parse(&data).map_err(invalid)? {
                    emit(&event).map_err(invalid)?;
                }
            }
        } else {
            for event in parser
                .parse(std::str::from_utf8(&buffered).map_err(|_| invalid("提供商返回无效 UTF-8"))?)
                .map_err(invalid)?
            {
                emit(&event).map_err(invalid)?;
            }
        }
        if !parser.finished {
            return Err(invalid(
                "提供商在完成标记前断开；已收到内容保留，可手动重新生成",
            ));
        }
    }
    emit(&WorkerEvent::Complete).map_err(invalid)
}

fn header(request: &Internet, kind: u32) -> Option<String> {
    let mut buffer = [0u16; 2048];
    let mut bytes = std::mem::size_of_val(&buffer) as u32;
    if unsafe {
        WinHttpQueryHeaders(
            request.0,
            kind,
            null(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
            null_mut(),
        )
    } == 0
    {
        return None;
    }
    Some(
        String::from_utf16_lossy(&buffer[..bytes as usize / 2])
            .trim_end_matches('\0')
            .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };
    fn server(response: &'static [u8]) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let join = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = vec![];
            let mut buffer = [0; 4096];
            loop {
                let n = stream.read(&mut buffer).unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..n]);
                if let Some(end) = request.windows(4).position(|s| s == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            stream.write_all(response).unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (url, join)
    }
    fn request(url: String) -> WorkerRequest {
        let mut profile = ProviderProfile::preset(0, "test".into());
        profile.base_url = url;
        profile.model = Model::unknown("synthetic");
        profile.allow_local_http = true;
        WorkerRequest {
            version: 1,
            profile,
            operation: WorkerOperation::Generate {
                system: "synthetic".into(),
                messages: vec![ChatMessage {
                    role: Role::User,
                    text: "ping".into(),
                }],
                images: vec![],
            },
        }
    }
    #[test]
    fn actual_http_stream_is_decoded_and_redirects_are_not_followed() {
        let (url, join) = server(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"pong\"}}]}\n\ndata: [DONE]\n\n");
        let mut events = vec![];
        execute_with_secrets(&request(url), &Secrets::default(), &mut |e| {
            events.push(e.clone());
            Ok(())
        })
        .unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, WorkerEvent::Text(s) if s == "pong"))
        );
        assert!(
            join.join()
                .unwrap()
                .starts_with("POST /v1/chat/completions")
        );
        let (url, join) = server(b"HTTP/1.1 307 Temporary Redirect\r\nLocation: https://example.invalid/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let failure =
            execute_with_secrets(&request(url), &Secrets::default(), &mut |_| Ok(())).unwrap_err();
        assert_eq!(failure.status, Some(307));
        assert!(!failure.retryable);
        join.join().unwrap();
    }
}
