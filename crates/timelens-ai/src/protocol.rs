use crate::*;
use serde_json::{Value, json};
use url::Url;

pub fn validate_endpoint(value: &str, allow_local_http: bool) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| "API 地址无效")?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || value.chars().any(char::is_control)
    {
        return Err("API 地址不得含凭据、查询参数或片段".into());
    }
    let host = url.host_str().ok_or("API 地址缺少主机")?;
    if url.scheme() != "https"
        && !(url.scheme() == "http"
            && allow_local_http
            && matches!(host, "localhost" | "127.0.0.1"))
    {
        return Err("远程 API 必须使用 HTTPS；本机 HTTP 需要先确认提示".into());
    }
    Ok(url)
}

pub struct HttpRequest {
    pub url: Url,
    pub method: &'static str,
    pub body: Option<Vec<u8>>,
}

pub fn build_request(request: &WorkerRequest) -> Result<HttpRequest, String> {
    if request.version != WORKER_PROTOCOL {
        return Err("AI 工作进程协议不兼容".into());
    }
    let profile = &request.profile;
    profile.validate()?;
    let mut url = validate_endpoint(&profile.base_url, profile.allow_local_http)?;
    let mut path = url.path().trim_end_matches('/').to_owned();
    if matches!(request.operation, WorkerOperation::Models) {
        path.push_str("/models");
        url.set_path(&path);
        return Ok(HttpRequest {
            url,
            method: "GET",
            body: None,
        });
    }
    let WorkerOperation::Generate {
        system,
        messages,
        images,
    } = &request.operation
    else {
        unreachable!()
    };
    if !images.is_empty() && profile.model.vision != Capability::Supported {
        return Err("该模型尚未确认支持视觉输入".into());
    }
    if images.len() > 20
        || messages.is_empty()
        || messages.last().is_none_or(|m| m.role != Role::User)
    {
        return Err("对话必须以用户问题结尾，每次最多 20 张已授权快照".into());
    }
    for image in images {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&image.webp_base64)
            .map_err(|_| "图片编码无效")?;
        if bytes.len() > 4 * 1024 * 1024
            || bytes.len() < 12
            || &bytes[..4] != b"RIFF"
            || &bytes[8..12] != b"WEBP"
        {
            return Err("图片必须是有效且不超过 4 MiB 的 WebP".into());
        }
    }
    let max_output = profile
        .parameters
        .max_output_tokens
        .unwrap_or(4096)
        .min(profile.model.context_tokens / 2);
    let mut body = match profile.protocol {
        Protocol::OpenAi => {
            path.push_str("/chat/completions");
            url.set_path(&path);
            let mut wire = vec![json!({"role":"system","content":system})];
            wire.extend(
                messages
                    .iter()
                    .map(|m| json!({"role":m.role,"content":m.text})),
            );
            if !images.is_empty() {
                let last = wire.last_mut().expect("nonempty messages");
                let mut parts = vec![json!({"type":"text","text":messages.last().unwrap().text})];
                for image in images {
                    parts.push(json!({"type":"text","text":format!("Snapshot captured at {} UTC ms", image.captured_utc_ms)}));
                    parts.push(json!({"type":"image_url","image_url":{"url":format!("data:image/webp;base64,{}", image.webp_base64)}}));
                }
                last["content"] = json!(parts);
            }
            let mut body = json!({"model":profile.model.id,"messages":wire,"stream":true,"stream_options":{"include_usage":true},"store":false});
            let key = if profile.model.supports_reasoning_effort {
                "max_completion_tokens"
            } else {
                "max_tokens"
            };
            body[key] = json!(max_output);
            if profile.model.supports_reasoning_effort
                && let Some(effort) = profile.parameters.reasoning_effort
            {
                body["reasoning_effort"] = json!(effort);
            }
            body
        }
        Protocol::Anthropic => {
            path.push_str("/messages");
            url.set_path(&path);
            let mut wire = messages
                .iter()
                .map(|m| json!({"role":m.role,"content":m.text}))
                .collect::<Vec<_>>();
            if !images.is_empty() {
                let mut parts = vec![json!({"type":"text","text":messages.last().unwrap().text})];
                for image in images {
                    parts.push(json!({"type":"text","text":format!("Snapshot captured at {} UTC ms", image.captured_utc_ms)}));
                    parts.push(json!({"type":"image","source":{"type":"base64","media_type":"image/webp","data":image.webp_base64}}));
                }
                wire.last_mut().unwrap()["content"] = json!(parts);
            }
            json!({"model":profile.model.id,"system":system,"messages":wire,"max_tokens":max_output,"stream":true})
        }
        Protocol::Gemini => {
            path.push_str("/models/");
            url.set_path(&path);
            url.path_segments_mut()
                .map_err(|_| "API 地址不支持路径")?
                .pop_if_empty()
                .push(&format!(
                    "{}:streamGenerateContent",
                    profile.model.id.trim_start_matches("models/")
                ));
            url.set_query(Some("alt=sse"));
            let mut wire = messages.iter().map(|m| json!({"role":if m.role == Role::Assistant {"model"} else {"user"},"parts":[{"text":m.text}]})).collect::<Vec<_>>();
            let parts = wire.last_mut().unwrap()["parts"].as_array_mut().unwrap();
            for image in images {
                parts.push(json!({"text":format!("Snapshot captured at {} UTC ms", image.captured_utc_ms)}));
                parts
                    .push(json!({"inlineData":{"mimeType":"image/webp","data":image.webp_base64}}));
            }
            json!({"systemInstruction":{"parts":[{"text":system}]},"contents":wire,"generationConfig":{"maxOutputTokens":max_output}})
        }
    };
    if profile.model.supports_temperature
        && let Some(temperature) = profile.parameters.temperature
    {
        if profile.protocol == Protocol::Gemini {
            body["generationConfig"]["temperature"] = json!(temperature);
        } else {
            body["temperature"] = json!(temperature);
        }
    }
    let body = serde_json::to_vec(&body).map_err(|_| "无法编码 AI 请求")?;
    if body.len() > MAX_FRAME_BYTES {
        return Err("请求过大，请缩小范围或减少图片".into());
    }
    Ok(HttpRequest {
        url,
        method: "POST",
        body: Some(body),
    })
}

pub fn parse_models(protocol: Protocol, value: &Value) -> Vec<Model> {
    let list = if protocol == Protocol::Gemini {
        value.get("models")
    } else {
        value.get("data")
    };
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item
                .get("id")
                .or_else(|| item.get("name"))?
                .as_str()?
                .trim_start_matches("models/");
            if id.is_empty() || id.len() > 256 {
                return None;
            }
            if protocol == Protocol::Gemini
                && item
                    .get("supportedGenerationMethods")
                    .and_then(Value::as_array)
                    .is_some_and(|methods| !methods.iter().any(|v| v == "generateContent"))
            {
                return None;
            }
            let mut model = Model::unknown(id);
            if let Some(n) = item
                .get("context_length")
                .or_else(|| item.get("inputTokenLimit"))
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
            {
                model.context_tokens = n.clamp(1024, 4_000_000);
            }
            for (key, capability) in [
                ("vision", &mut model.vision),
                ("reasoning", &mut model.reasoning),
                ("tools", &mut model.tools),
            ] {
                if let Some(supports) = item
                    .get("capabilities")
                    .and_then(|v| v.get(key))
                    .and_then(Value::as_bool)
                {
                    *capability = if supports {
                        Capability::Supported
                    } else {
                        Capability::Unsupported
                    };
                }
            }
            if let Some(modalities) = item
                .pointer("/architecture/input_modalities")
                .and_then(Value::as_array)
            {
                model.vision = if modalities.iter().any(|v| v == "image") {
                    Capability::Supported
                } else {
                    Capability::Unsupported
                };
            }
            if let Some(parameters) = item.get("supported_parameters").and_then(Value::as_array) {
                model.supports_temperature = parameters.iter().any(|v| v == "temperature");
                model.supports_reasoning_effort = parameters
                    .iter()
                    .any(|v| v == "reasoning" || v == "reasoning_effort");
            }
            // Stable catalog entries only; unfamiliar models remain explicitly unknown.
            if model.vision == Capability::Unknown
                && matches!(
                    id,
                    "gpt-4o"
                        | "gpt-4o-mini"
                        | "gpt-4.1"
                        | "gpt-4.1-mini"
                        | "gemini-2.5-pro"
                        | "gemini-2.5-flash"
                        | "claude-sonnet-4-20250514"
                )
            {
                model.vision = Capability::Supported;
            }
            Some(model)
        })
        .collect()
}

#[derive(Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    data: Vec<String>,
}
impl SseDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, String> {
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() + self.data.iter().map(String::len).sum::<usize>() > 2 * 1024 * 1024 {
            return Err("提供商单个流事件超过限制".into());
        }
        let mut events = vec![];
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<_> = self.buffer.drain(..=end).collect();
            let line = std::str::from_utf8(&line)
                .map_err(|_| "提供商返回无效 UTF-8")?
                .trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(self.data.join("\n"));
                    self.data.clear();
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                self.data
                    .push(data.strip_prefix(' ').unwrap_or(data).to_owned());
            }
        }
        Ok(events)
    }
    pub fn finish(&mut self) -> Result<Vec<String>, String> {
        self.push(b"\n\n")
    }
}

pub struct StreamParser {
    pub protocol: Protocol,
    pub finished: bool,
}
impl StreamParser {
    pub fn new(protocol: Protocol) -> Self {
        Self {
            protocol,
            finished: false,
        }
    }
    pub fn parse(&mut self, data: &str) -> Result<Vec<WorkerEvent>, String> {
        if data.trim() == "[DONE]" {
            self.finished = true;
            return Ok(vec![]);
        }
        let value: Value = serde_json::from_str(data).map_err(|_| "提供商流事件不是有效 JSON")?;
        if let Some(error) = value.get("error") {
            return Err(error.to_string());
        }
        let mut events = vec![];
        let mut usage = TokenUsage::default();
        match self.protocol {
            Protocol::OpenAi => {
                if let Some(choices) = value.get("choices").and_then(Value::as_array)
                    && let Some(choice) = choices.first()
                {
                    let delta = choice
                        .get("delta")
                        .or_else(|| choice.get("message"))
                        .unwrap_or(&Value::Null);
                    emit_string(&mut events, delta.get("content"), false);
                    emit_string(
                        &mut events,
                        delta
                            .get("reasoning_content")
                            .or_else(|| delta.get("reasoning")),
                        true,
                    );
                    if choice.get("finish_reason").is_some_and(|v| !v.is_null()) {
                        self.finished = true;
                    }
                }
                if let Some(u) = value.get("usage") {
                    usage.input = u.get("prompt_tokens").and_then(Value::as_u64);
                    usage.output = u.get("completion_tokens").and_then(Value::as_u64);
                    usage.cached = u
                        .pointer("/prompt_tokens_details/cached_tokens")
                        .and_then(Value::as_u64);
                    usage.reasoning = u
                        .pointer("/completion_tokens_details/reasoning_tokens")
                        .and_then(Value::as_u64);
                    usage.cost = u
                        .get("cost")
                        .filter(|c| c.is_number())
                        .map(|c| format!("{c} USD (provider)"));
                }
            }
            Protocol::Anthropic => {
                match value.get("type").and_then(Value::as_str).unwrap_or("") {
                    "content_block_delta" => {
                        emit_string(&mut events, value.pointer("/delta/text"), false);
                        emit_string(&mut events, value.pointer("/delta/thinking"), true);
                    }
                    "message_stop" => self.finished = true,
                    "message" => {
                        if let Some(content) = value.get("content").and_then(Value::as_array) {
                            for block in content {
                                emit_string(&mut events, block.get("text"), false);
                                emit_string(&mut events, block.get("thinking"), true);
                            }
                        }
                        self.finished = true;
                    }
                    _ => {}
                }
                let u = value
                    .get("usage")
                    .or_else(|| value.pointer("/message/usage"));
                if let Some(u) = u {
                    usage.input = u.get("input_tokens").and_then(Value::as_u64);
                    usage.output = u.get("output_tokens").and_then(Value::as_u64);
                    usage.cached = u.get("cache_read_input_tokens").and_then(Value::as_u64);
                    usage.reasoning = u
                        .pointer("/output_tokens_details/thinking_tokens")
                        .and_then(Value::as_u64);
                }
            }
            Protocol::Gemini => {
                if let Some(candidate) = value
                    .get("candidates")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                {
                    if let Some(parts) = candidate
                        .pointer("/content/parts")
                        .and_then(Value::as_array)
                    {
                        for part in parts {
                            emit_string(
                                &mut events,
                                part.get("text"),
                                part.get("thought").and_then(Value::as_bool) == Some(true),
                            );
                        }
                    }
                    if candidate.get("finishReason").is_some() {
                        self.finished = true;
                    }
                }
                if let Some(u) = value.get("usageMetadata") {
                    usage.input = u.get("promptTokenCount").and_then(Value::as_u64);
                    usage.output = u.get("candidatesTokenCount").and_then(Value::as_u64);
                    usage.cached = u.get("cachedContentTokenCount").and_then(Value::as_u64);
                    usage.reasoning = u.get("thoughtsTokenCount").and_then(Value::as_u64);
                }
                if value.pointer("/promptFeedback/blockReason").is_some() {
                    return Err("提供商拒绝了本次请求".into());
                }
            }
        }
        if usage != TokenUsage::default() {
            events.push(WorkerEvent::Usage(usage));
        }
        Ok(events)
    }
}

fn emit_string(events: &mut Vec<WorkerEvent>, value: Option<&Value>, reasoning: bool) {
    if let Some(text) = value.and_then(Value::as_str).filter(|s| !s.is_empty()) {
        events.push(if reasoning {
            WorkerEvent::Reasoning(text.into())
        } else {
            WorkerEvent::Text(text.into())
        });
    }
}

pub fn redact_error(message: &str, secrets: &[&str]) -> String {
    use base64::Engine;
    let mut clean = message.chars().take(32_768).collect::<String>();
    let mut forms = vec![];
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        forms.push((*secret).to_string());
        forms.push(
            serde_json::to_string(secret)
                .unwrap()
                .trim_matches('"')
                .to_string(),
        );
        forms.push(url::form_urlencoded::byte_serialize(secret.as_bytes()).collect());
        forms.push(base64::engine::general_purpose::STANDARD.encode(secret.as_bytes()));
    }
    forms.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for form in forms {
        clean = clean.replace(&form, "[REDACTED]");
    }
    // Even an unknown echoed authentication header must never reach the side panel.
    clean
        .lines()
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            if [
                "authorization",
                "api-key",
                "api_key",
                "x-goog-api-key",
                "cookie",
                "bearer ",
            ]
            .iter()
            .any(|field| lower.contains(field))
            {
                "[credential-bearing provider error line redacted]".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoints_cannot_smuggle_credentials_or_downgrade_remote_tls() {
        for url in [
            "http://example.com/v1",
            "https://key@example.com",
            "https://example.com?key=secret",
            "file:///tmp/api",
            "http://127.0.0.1.evil.test",
        ] {
            assert!(validate_endpoint(url, true).is_err(), "{url}");
        }
        assert!(validate_endpoint("http://127.0.0.1:3123/v1", false).is_err());
        assert!(validate_endpoint("http://127.0.0.1:3123/v1", true).is_ok());
    }
    #[test]
    fn fragmented_unicode_and_multiline_sse_survive_arbitrary_network_boundaries() {
        let payload =
            "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\r\n\r\ndata: [DONE]\n\n";
        for size in 1..payload.len() {
            let mut decoder = SseDecoder::default();
            let mut parser = StreamParser::new(Protocol::OpenAi);
            let mut text = String::new();
            for chunk in payload.as_bytes().chunks(size) {
                for data in decoder.push(chunk).unwrap() {
                    for event in parser.parse(&data).unwrap() {
                        if let WorkerEvent::Text(s) = event {
                            text.push_str(&s);
                        }
                    }
                }
            }
            assert_eq!(text, "你好");
            assert!(parser.finished);
        }
    }
    #[test]
    fn native_protocols_keep_public_reasoning_and_usage_separate() {
        let mut claude = StreamParser::new(Protocol::Anthropic);
        assert!(
            matches!(&claude.parse(r#"{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"public summary"}}"#).unwrap()[0], WorkerEvent::Reasoning(s) if s == "public summary")
        );
        assert!(!claude.finished);
        claude.parse(r#"{"type":"message_stop"}"#).unwrap();
        assert!(claude.finished);
        let mut gemini = StreamParser::new(Protocol::Gemini);
        let events = gemini.parse(r#"{"candidates":[{"content":{"parts":[{"text":"summary","thought":true},{"text":"answer"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":9,"candidatesTokenCount":4,"thoughtsTokenCount":2}}"#).unwrap();
        assert!(matches!(events[0], WorkerEvent::Reasoning(_)));
        assert!(matches!(events[1], WorkerEvent::Text(_)));
        assert!(gemini.finished);
    }
    #[test]
    fn unknown_capabilities_block_images_and_untyped_parameters() {
        let mut profile = ProviderProfile::preset(0, "test".into());
        profile.model = Model::unknown("custom-model");
        profile.parameters.temperature = Some(0.2);
        profile.parameters.reasoning_effort = Some(ReasoningEffort::High);
        let mut request = WorkerRequest {
            version: 1,
            profile,
            operation: WorkerOperation::Generate {
                system: "s".into(),
                messages: vec![ChatMessage {
                    role: Role::User,
                    text: "u".into(),
                }],
                images: vec![],
            },
        };
        let body: Value =
            serde_json::from_slice(&build_request(&request).unwrap().body.unwrap()).unwrap();
        assert_eq!(body["store"], false);
        assert!(body.get("temperature").is_none());
        assert!(body.get("reasoning_effort").is_none());
        if let WorkerOperation::Generate { images, .. } = &mut request.operation {
            images.push(ImageInput {
                webp_base64: "bad".into(),
                captured_utc_ms: 1,
            });
        }
        assert!(build_request(&request).is_err());
    }
    #[test]
    fn credentials_never_survive_error_rendering() {
        let clean = redact_error(
            "invalid sk-secret-123\nAuthorization: Bearer alien-key\nserver failed",
            &["sk-secret-123"],
        );
        assert!(!clean.contains("sk-secret-123"));
        assert!(!clean.contains("alien-key"));
        assert!(clean.contains("server failed"));
    }
    #[test]
    fn retries_do_not_replay_partial_answers_or_single_use_images() {
        let failure = AiFailure {
            status: Some(429),
            message: "limited".into(),
            retry_after_seconds: Some(77),
            retryable: true,
        };
        assert_eq!(retry_delay(0, 3, false, false, &failure), Some(77));
        assert_eq!(retry_delay(0, 3, true, false, &failure), None);
        assert_eq!(retry_delay(0, 3, false, true, &failure), None);
        assert_eq!(retry_delay(3, 3, false, false, &failure), None);
    }
}
