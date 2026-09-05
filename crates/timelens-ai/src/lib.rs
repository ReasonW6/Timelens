//! Ordinary-privilege AI types and adapters. This crate has no access to activity storage.
pub mod context;
#[cfg(windows)]
pub mod credentials;
pub mod protocol;
pub mod schedule;
#[cfg(windows)]
pub mod transport;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const WORKER_PROTOCOL: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_PROMPT: &str = "你是 Timelens 的使用情况分析助手。仅依据提供的结构化事实回答。说明时间范围、覆盖率和缺失；区分显示中、聚焦中、后台，重叠时长不能相加为一天总长。不要推断应用中的具体内容、用户意图或未观测的活动。先给出简洁总结，再列出主要应用、时段趋势和可选建议。建议与事实分开。用 {{language}} 回答，范围 {{range}}，覆盖率 {{coverage}}。";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    #[default]
    OpenAi,
    Anthropic,
    Gemini,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Supported,
    Unsupported,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub id: String,
    pub vision: Capability,
    pub reasoning: Capability,
    pub tools: Capability,
    pub context_tokens: u32,
    pub supports_temperature: bool,
    pub supports_reasoning_effort: bool,
}

impl Model {
    pub fn unknown(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            context_tokens: 32_768,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameters {
    pub temperature: Option<f64>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderProfile {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub model: Model,
    pub models: Vec<Model>,
    pub parameters: Parameters,
    pub retries: u8,
    pub idle_timeout_seconds: Option<u32>,
    pub credential_revision: u64,
    pub tested_revision: Option<String>,
    pub allow_local_http: bool,
    pub azure: bool,
}

impl ProviderProfile {
    pub fn preset(index: usize, id: String) -> Self {
        let (name, protocol, base_url) = match index {
            1 => (
                "Anthropic Claude",
                Protocol::Anthropic,
                "https://api.anthropic.com/v1",
            ),
            2 => (
                "Google Gemini",
                Protocol::Gemini,
                "https://generativelanguage.googleapis.com/v1beta",
            ),
            3 => (
                "Azure OpenAI",
                Protocol::OpenAi,
                "https://YOUR-RESOURCE.openai.azure.com/openai/v1",
            ),
            4 => (
                "OpenRouter",
                Protocol::OpenAi,
                "https://openrouter.ai/api/v1",
            ),
            5 => ("DeepSeek", Protocol::OpenAi, "https://api.deepseek.com/v1"),
            6 => (
                "硅基流动",
                Protocol::OpenAi,
                "https://api.siliconflow.cn/v1",
            ),
            7 => (
                "Moonshot / Kimi",
                Protocol::OpenAi,
                "https://api.moonshot.cn/v1",
            ),
            8 => ("自定义提供商", Protocol::OpenAi, "https://example.com/v1"),
            _ => ("OpenAI", Protocol::OpenAi, "https://api.openai.com/v1"),
        };
        Self {
            id,
            name: name.into(),
            protocol,
            base_url: base_url.into(),
            model: Model::unknown(""),
            models: vec![],
            parameters: Parameters::default(),
            retries: 3,
            idle_timeout_seconds: None,
            credential_revision: 0,
            tested_revision: None,
            allow_local_http: false,
            azure: index == 3,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !valid_id(&self.id) || self.name.trim().is_empty() || self.name.len() > 256 {
            return Err("提供商名称或标识无效".into());
        }
        protocol::validate_endpoint(&self.base_url, self.allow_local_http)?;
        if self.model.id.is_empty()
            || self.model.id.len() > 256
            || self.model.id.chars().any(char::is_control)
        {
            return Err("请填写模型 ID".into());
        }
        if !(1024..=4_000_000).contains(&self.model.context_tokens) || self.retries > 10 {
            return Err("上下文或重试次数超出范围".into());
        }
        if self
            .parameters
            .temperature
            .is_some_and(|v| !v.is_finite() || !(0.0..=2.0).contains(&v))
        {
            return Err("温度需在 0–2 之间".into());
        }
        if self
            .parameters
            .max_output_tokens
            .is_some_and(|n| n == 0 || n >= self.model.context_tokens)
        {
            return Err("最大输出须小于模型上下文".into());
        }
        if self
            .idle_timeout_seconds
            .is_some_and(|n| !(10..=86_400).contains(&n))
        {
            return Err("无响应超时需在 10–86400 秒之间".into());
        }
        Ok(())
    }

    /// An exact public configuration stamp, never a hash or copy of a credential.
    pub fn revision(&self) -> String {
        serde_json::to_string(&(
            self.protocol,
            &self.base_url,
            &self.model,
            &self.parameters,
            self.credential_revision,
            self.allow_local_http,
            self.azure,
        ))
        .expect("finite validated configuration")
    }

    pub fn is_tested(&self) -> bool {
        self.tested_revision.as_deref() == Some(self.revision().as_str())
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptPreset {
    pub id: String,
    pub name: String,
    pub text: String,
    pub language: String,
    pub version: u32,
}
impl Default for PromptPreset {
    fn default() -> Self {
        Self {
            id: "default".into(),
            name: "Timelens 默认".into(),
            text: DEFAULT_PROMPT.into(),
            language: "简体中文".into(),
            version: 1,
        }
    }
}

impl PromptPreset {
    pub fn render(&self, envelope: &AiEnvelope) -> String {
        self.text
            .replace("{{language}}", &self.language)
            .replace(
                "{{range}}",
                &format!(
                    "{}–{} UTC ms",
                    envelope.started_utc_ms, envelope.ended_utc_ms
                ),
            )
            .replace(
                "{{coverage}}",
                &format!(
                    "{:.1}%",
                    envelope.covered_ms as f64 * 100.0
                        / (envelope.ended_utc_ms - envelope.started_utc_ms).max(1) as f64
                ),
            )
    }
}

/// The only serialized activity data accepted by an AI request. No internal identity,
/// path, icon, minute bucket, handle, credential or diagnostic can be represented.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AiEnvelope {
    pub schema_version: u32,
    pub started_utc_ms: i64,
    pub ended_utc_ms: i64,
    pub timezone: String,
    pub covered_ms: u64,
    pub available: bool,
    pub applications: Vec<AiApplication>,
    pub system_ms: BTreeMap<String, u64>,
    pub missing: Vec<MissingInterval>,
    pub input: InputTotals,
    pub hourly: Vec<HourlyTrend>,
    pub top_keys: Vec<KeyFrequency>,
    pub key_frequency_scope: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InputTotals {
    pub keyboard: u64,
    pub left: u64,
    pub middle: u64,
    pub right: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AiApplication {
    pub name: String,
    pub opened_ms: u64,
    pub displayed_ms: u64,
    pub focused_ms: u64,
    pub background_ms: u64,
    pub window_count: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HourlyTrend {
    pub started_utc_ms: i64,
    pub focused_ms: u64,
    pub input: InputTotals,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyFrequency {
    pub physical_position: u32,
    pub count: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MissingInterval {
    pub started_utc_ms: i64,
    pub ended_utc_ms: i64,
    pub category: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cached: Option<u64>,
    pub reasoning: Option<u64>,
    pub cost: Option<String>,
}
impl TokenUsage {
    pub fn add(&mut self, other: &Self) {
        fn sum(a: &mut Option<u64>, b: Option<u64>) {
            if let Some(b) = b {
                *a = Some(a.unwrap_or(0).saturating_add(b));
            }
        }
        sum(&mut self.input, other.input);
        sum(&mut self.output, other.output);
        sum(&mut self.cached, other.cached);
        sum(&mut self.reasoning, other.reasoning);
        if let Some(cost) = other.cost.as_deref() {
            self.cost = match (self.cost.as_deref(), cost.parse::<f64>()) {
                (None, _) => Some(cost.into()),
                (Some(previous), Ok(amount)) => previous
                    .parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite() && amount.is_finite())
                    .map(|n| format!("{:.8}", n + amount))
                    .or_else(|| Some(format!("{previous} + {cost}"))),
                (Some(previous), _) => Some(format!("{previous} + {cost}")),
            };
        }
    }
    pub fn update(&mut self, other: Self) {
        if other.input.is_some() {
            self.input = other.input;
        }
        if other.output.is_some() {
            self.output = other.output;
        }
        if other.cached.is_some() {
            self.cached = other.cached;
        }
        if other.reasoning.is_some() {
            self.reasoning = other.reasoning;
        }
        if other.cost.is_some() {
            self.cost = other.cost;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageInput {
    pub webp_base64: String,
    pub captured_utc_ms: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum WorkerOperation {
    Models,
    Generate {
        system: String,
        messages: Vec<ChatMessage>,
        images: Vec<ImageInput>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerRequest {
    pub version: u32,
    pub profile: ProviderProfile,
    pub operation: WorkerOperation,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AiFailure {
    pub status: Option<u32>,
    pub message: String,
    pub retry_after_seconds: Option<u64>,
    pub retryable: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum WorkerEvent {
    Text(String),
    Reasoning(String),
    Usage(TokenUsage),
    Models(Vec<Model>),
    Failure(AiFailure),
    Complete,
}

pub fn retry_delay(
    retries_used: u8,
    limit: u8,
    has_body: bool,
    has_images: bool,
    failure: &AiFailure,
) -> Option<u64> {
    // An image approval is consumed by one request, including a failed request.
    if has_body || has_images || !failure.retryable || retries_used >= limit.min(10) {
        return None;
    }
    Some(failure.retry_after_seconds.unwrap_or(match retries_used {
        0 => 60,
        1 => 300,
        _ => 900,
    }))
}
