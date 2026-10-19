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
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
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
            let host = url.host_str().unwrap_or("");
            // OpenAI's own reasoning models refuse max_tokens; others expect it.
            // Regional endpoints such as eu.api.openai.com are official too.
            let official =
                profile.azure || host == "api.openai.com" || host.ends_with(".api.openai.com");
            let key = if official {
                "max_completion_tokens"
            } else {
                "max_tokens"
            };
            body[key] = json!(max_output);
            openai_thinking(&mut body, profile, host, official);
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
            let mut body = json!({"model":profile.model.id,"system":system,"messages":wire,"max_tokens":max_output,"stream":true});
            if let Some(on) = profile.parameters.thinking {
                let effort = profile.parameters.reasoning_effort;
                if claude_takes_budget(&profile.model.id) {
                    body["thinking"] = if on {
                        // The budget counts against max_tokens and must leave room to answer.
                        let budget = thinking_budget(effort).min(max_output.saturating_sub(1024));
                        if budget < 1024 {
                            return Err("开启思考时最大输出至少要 2048 Token".into());
                        }
                        json!({"type":"enabled","budget_tokens":budget})
                    } else {
                        json!({"type":"disabled"})
                    };
                } else if on {
                    // Claude 4.6 and later reject budgets and size their own thinking.
                    body["thinking"] = json!({"type":"adaptive","display":"summarized"});
                    if let Some(effort) = effort {
                        body["output_config"] = json!({"effort":effort});
                    }
                } else {
                    let id = &profile.model.id;
                    if id.contains("opus-5-5") || id.contains("fable") || id.contains("mythos") {
                        // These cannot stop thinking; low effort keeps it brief.
                        body["output_config"] = json!({"effort":"low"});
                    } else if id.contains("sonnet-5-5") {
                        // Sonnet 5.5 spells its lowest setting this way.
                        body["thinking"] = json!({"type":"between_tools"});
                    } else {
                        body["thinking"] = json!({"type":"disabled"});
                    }
                }
            }
            body
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
            let mut body = json!({"systemInstruction":{"parts":[{"text":system}]},"contents":wire,"generationConfig":{"maxOutputTokens":max_output}});
            let id = profile.model.id.trim_start_matches("models/");
            if let Some(on) = profile.parameters.thinking
                && id.starts_with("gemini-3")
            {
                // Gemini 3 takes a level instead of a budget and cannot stop thinking.
                let pro = id.contains("-pro");
                let level = if on {
                    profile
                        .parameters
                        .reasoning_effort
                        .map(|effort| match effort {
                            ReasoningEffort::Low => "low",
                            ReasoningEffort::Medium if !pro => "medium",
                            _ => "high",
                        })
                } else {
                    // Only the original 3 Flash goes down to `minimal`; `low` is
                    // the floor every other Gemini 3 model accepts.
                    Some(if id.starts_with("gemini-3-flash") {
                        "minimal"
                    } else {
                        "low"
                    })
                };
                let config = &mut body["generationConfig"]["thinkingConfig"];
                *config = json!({"includeThoughts":on});
                if let Some(level) = level {
                    config["thinkingLevel"] = json!(level);
                }
            } else if let Some(on) = profile.parameters.thinking {
                body["generationConfig"]["thinkingConfig"] = if on {
                    // -1 lets the model size its own thinking; a fixed budget counts
                    // against maxOutputTokens and must leave room to answer.
                    let budget = match profile.parameters.reasoning_effort {
                        None => -1,
                        Some(effort) => {
                            let budget =
                                thinking_budget(Some(effort)).min(max_output.saturating_sub(1024));
                            if budget < 1024 {
                                return Err("开启思考时最大输出至少要 2048 Token".into());
                            }
                            i64::from(budget)
                        }
                    };
                    json!({"includeThoughts":true,"thinkingBudget":budget})
                } else if profile.model.id.contains("-pro") {
                    // Pro models cannot stop thinking; 128 is their smallest budget.
                    json!({"thinkingBudget":128})
                } else {
                    json!({"thinkingBudget":0})
                };
            }
            body
        }
    };
    // Claude refuses a temperature while it thinks.
    if profile.model.supports_temperature
        && !(profile.protocol == Protocol::Anthropic && profile.parameters.thinking == Some(true))
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

/// OpenAI-compatible services each spell deep thinking their own way. Only the
/// typed parameter of the endpoint's family is sent; following the model sends none.
fn openai_thinking(body: &mut Value, profile: &ProviderProfile, host: &str, official: bool) {
    let parameters = &profile.parameters;
    let Some(on) = parameters.thinking else {
        return;
    };
    let effort = parameters.reasoning_effort.filter(|_| on);
    if official {
        // Only reasoning models take the field; chat models reject it.
        let id = profile.model.id.trim_start_matches("openai/");
        let o_series = id.starts_with('o') && id[1..].starts_with(|c: char| c.is_ascii_digit());
        // GPT-5 and every later generation reason.
        let generation = id
            .strip_prefix("gpt-")
            .map(|rest| {
                rest.split(|c: char| !c.is_ascii_digit())
                    .next()
                    .unwrap_or("")
            })
            .and_then(|major| major.parse::<u32>().ok());
        let gpt5 = generation.is_some_and(|major| major >= 5);
        let reasoning = o_series
            || gpt5
            || profile.model.supports_reasoning_effort
            || profile.model.reasoning == Capability::Supported;
        // gpt-5-pro only accepts `high`, its default.
        if id.starts_with("gpt-5-pro") {
            return;
        }
        if !reasoning {
            // An Azure deployment name hides its model; a chosen strength is the
            // user's word that it reasons, anything less is left to the model.
            let chat = id.starts_with("gpt-4") || id.starts_with("gpt-3");
            if profile.azure
                && !chat
                && let Some(effort) = effort
            {
                body["reasoning_effort"] = json!(effort);
            }
            return;
        }
        if !on {
            // `none` arrived with gpt-5.1; earlier models stop at their lowest level.
            body["reasoning_effort"] =
                json!(if generation == Some(5) && !id.starts_with("gpt-5.") {
                    "minimal"
                } else if gpt5 {
                    "none"
                } else {
                    "low"
                });
        } else {
            // gpt-5.1 and later default to `none`, so switching on needs a level.
            body["reasoning_effort"] = json!(effort.unwrap_or(ReasoningEffort::Medium));
        }
    } else if host == "openrouter.ai" {
        body["reasoning"] = match effort {
            Some(effort) => json!({"effort":effort}),
            None => json!({"enabled":on}),
        };
    } else if host.ends_with("siliconflow.cn")
        || host.ends_with("siliconflow.com")
        || host.starts_with("dashscope")
        || host.ends_with(".maas.aliyuncs.com")
    {
        body["enable_thinking"] = json!(on);
        if effort.is_some() {
            body["thinking_budget"] = json!(thinking_budget(effort));
        }
    } else {
        // DeepSeek, Kimi, GLM and Doubao share this switch.
        body["thinking"] = json!({"type":if on {"enabled"} else {"disabled"}});
        if let Some(effort) = effort {
            body["reasoning_effort"] = json!(effort);
        }
    }
}

/// Claude models before 4.6 take a fixed thinking budget; later ones reject it.
fn claude_takes_budget(model: &str) -> bool {
    model.starts_with("claude-3")
        || ["-4-0", "-4-1", "-4-5", "-4-2025"]
            .iter()
            .any(|old| model.contains(old))
}

/// A thinking token budget for providers that take one instead of a level.
fn thinking_budget(effort: Option<ReasoningEffort>) -> u32 {
    match effort {
        Some(ReasoningEffort::Low) => 2048,
        Some(ReasoningEffort::High) => 24_576,
        Some(ReasoningEffort::Medium) | None => 8192,
    }
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
            } else if let Some((context, _)) = known_limits(id) {
                model.context_tokens = context;
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
    fn thinking_switch_uses_each_provider_family_spelling() {
        let profile = |protocol: Protocol, url: &str, model: &str, thinking: Option<bool>| {
            let mut profile = ProviderProfile::preset(0, "test".into());
            profile.protocol = protocol;
            profile.base_url = url.into();
            profile.model = Model::unknown(model);
            profile.parameters.max_output_tokens = Some(8192);
            profile.parameters.thinking = thinking;
            profile.parameters.reasoning_effort = Some(ReasoningEffort::Low);
            profile
        };
        let encode = |profile: ProviderProfile| {
            let request = WorkerRequest {
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
            serde_json::from_slice::<Value>(&build_request(&request).unwrap().body.unwrap())
                .unwrap()
        };
        let body = |protocol: Protocol, url: &str, model: &str, thinking: Option<bool>| {
            encode(profile(protocol, url, model, thinking))
        };
        let deepseek = "https://api.deepseek.com/v1";
        let followed = body(Protocol::OpenAi, deepseek, "m", None);
        assert!(followed.get("thinking").is_none() && followed.get("reasoning_effort").is_none());
        let off = body(Protocol::OpenAi, deepseek, "m", Some(false));
        assert_eq!(off["thinking"]["type"], "disabled");
        assert!(off.get("reasoning_effort").is_none());
        let on = body(Protocol::OpenAi, deepseek, "m", Some(true));
        assert_eq!(on["thinking"]["type"], "enabled");
        assert_eq!(on["reasoning_effort"], "low");
        assert_eq!(on["max_tokens"], 8192);
        let openai = "https://api.openai.com/v1";
        let gpt51 = body(Protocol::OpenAi, openai, "gpt-5.1", Some(false));
        assert_eq!(gpt51["reasoning_effort"], "none");
        assert_eq!(gpt51["max_completion_tokens"], 8192);
        assert!(gpt51.get("thinking").is_none());
        let gpt5 = body(Protocol::OpenAi, openai, "gpt-5-mini", Some(false));
        assert_eq!(gpt5["reasoning_effort"], "minimal");
        let regional = body(
            Protocol::OpenAi,
            "https://eu.api.openai.com/v1",
            "gpt-6-sol",
            Some(false),
        );
        assert_eq!(regional["reasoning_effort"], "none");
        assert_eq!(regional["max_completion_tokens"], 8192);
        let o3 = body(Protocol::OpenAi, openai, "o3", Some(false));
        assert_eq!(o3["reasoning_effort"], "low");
        let chat = body(Protocol::OpenAi, openai, "gpt-4o", Some(false));
        assert!(chat.get("reasoning_effort").is_none());
        let pro = body(Protocol::OpenAi, openai, "gpt-5-pro", Some(false));
        assert!(pro.get("reasoning_effort").is_none());
        let mut default_on = profile(Protocol::OpenAi, openai, "gpt-5.1", Some(true));
        default_on.parameters.reasoning_effort = None;
        assert_eq!(encode(default_on)["reasoning_effort"], "medium");
        let mut alias = profile(
            Protocol::OpenAi,
            "https://example.openai.azure.com/openai/v1",
            "production-reasoner",
            Some(true),
        );
        alias.azure = true;
        assert_eq!(encode(alias.clone())["reasoning_effort"], "low");
        alias.parameters.thinking = Some(false);
        assert!(encode(alias)["reasoning_effort"].is_null());
        let workspace = body(
            Protocol::OpenAi,
            "https://ws.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
            "qwen-plus",
            Some(true),
        );
        assert_eq!(workspace["enable_thinking"], true);
        let silicon = body(
            Protocol::OpenAi,
            "https://api.siliconflow.cn/v1",
            "m",
            Some(true),
        );
        assert_eq!(silicon["enable_thinking"], true);
        let router = body(
            Protocol::OpenAi,
            "https://openrouter.ai/api/v1",
            "m",
            Some(false),
        );
        assert_eq!(router["reasoning"]["enabled"], false);
        let anthropic = "https://api.anthropic.com/v1";
        let legacy = body(
            Protocol::Anthropic,
            anthropic,
            "claude-sonnet-4-20250514",
            Some(true),
        );
        assert_eq!(legacy["thinking"]["budget_tokens"], 2048);
        let current = body(
            Protocol::Anthropic,
            anthropic,
            "claude-opus-5-5",
            Some(true),
        );
        assert_eq!(current["thinking"]["type"], "adaptive");
        assert_eq!(current["output_config"]["effort"], "low");
        let current_off = body(
            Protocol::Anthropic,
            anthropic,
            "claude-opus-5-5",
            Some(false),
        );
        assert!(current_off.get("thinking").is_none());
        let sonnet55 = body(
            Protocol::Anthropic,
            anthropic,
            "claude-sonnet-5-5",
            Some(false),
        );
        assert_eq!(sonnet55["thinking"]["type"], "between_tools");
        let sonnet5 = body(
            Protocol::Anthropic,
            anthropic,
            "claude-sonnet-5",
            Some(false),
        );
        assert_eq!(sonnet5["thinking"]["type"], "disabled");
        let gemini = "https://example.com/v1beta";
        let flash = body(Protocol::Gemini, gemini, "gemini-2.5-flash", Some(false));
        assert_eq!(
            flash["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            0
        );
        let pro = body(Protocol::Gemini, gemini, "gemini-2.5-pro", Some(false));
        assert_eq!(
            pro["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            128
        );
        let mut high = profile(Protocol::Gemini, gemini, "gemini-2.5-flash", Some(true));
        high.parameters.reasoning_effort = Some(ReasoningEffort::High);
        assert_eq!(
            encode(high)["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            7168
        );
        let flash38 = body(Protocol::Gemini, gemini, "gemini-3.8-flash", Some(false));
        assert_eq!(
            flash38["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "low"
        );
        let pro3 = body(
            Protocol::Gemini,
            gemini,
            "gemini-3-pro-preview",
            Some(false),
        );
        assert_eq!(
            pro3["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "low"
        );
        assert!(pro3["generationConfig"]["thinkingConfig"]["thinkingBudget"].is_null());
        let flash3 = body(Protocol::Gemini, gemini, "gemini-3-flash", Some(true));
        assert_eq!(
            flash3["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "low"
        );
    }
    #[test]
    fn catalog_fills_context_only_where_the_provider_is_silent() {
        let models = parse_models(
            Protocol::OpenAi,
            &json!({"data":[{"id":"deepseek-flash"},{"id":"gpt-4o","context_length":64000},{"id":"custom"}]}),
        );
        assert_eq!(models[0].context_tokens, 1_000_000);
        assert_eq!(models[1].context_tokens, 64_000);
        assert_eq!(models[2].context_tokens, 32_768);
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
