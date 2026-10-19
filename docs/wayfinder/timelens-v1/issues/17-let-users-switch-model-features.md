# 由用户开关模型功能

Type: grilling
Status: resolved
Blocked by: 09

## Question

用户反馈：连接设置让人判断“能看图片吗？”“返回思考过程”“工具调用”“模型支持 reasoning_effort”，这些是模型能力的事实，用户通常不知道；DeepSeek `deepseek-flash` 默认开启思考，在默认 4096 的最大输出里把额度全部用于思考，正文为空。用户希望改成自己选择是否开启。票据 09 的“能力未知时由用户手动覆盖视觉、公开推理及工具能力”是否保留？

## Answer

连接设置不再询问模型能力，改为用户开关的功能。此结论取代票据 09 中“允许用户对当前提供商手动覆盖视觉、公开推理及工具能力”一条；能力元数据仍可来自提供商与内置目录，只用于默认值和模型列表标识。

- **允许附带快照**：开关。从模型列表选择时按提供商元数据或内置目录给出初值，用户可改；关闭时手动总结不能附带快照，开启后若模型不能看图，由服务商直接报错。快照仍逐次授权，定时总结仍不带快照。
- **深度思考**：跟随模型 / 开启 / 关闭，开启时可选强度（默认、低、中、高）。跟随模型不发送任何思考参数。开启或关闭时，适配器按服务地址所属家族发送其文档化的类型化参数：OpenAI 官方（含区域地址）与 Azure 只对推理模型发送 `reasoning_effort`（o 系列、gpt-5 及之后各代与元数据标明支持推理的模型；名称无法辨认的 Azure 部署只在用户选了强度时发送该强度；开启而未选强度时发送 `medium`，关闭时取该模型的最低档：gpt-5 为 `minimal`，gpt-5.x 为 `none`，GPT-6 起与 o 系列为 `low`；gpt-5-pro 只支持 `high`，不发送）；OpenRouter 用 `reasoning`；硅基流动与阿里云百炼（DashScope 及工作空间地址）用 `enable_thinking`（强度换成 `thinking_budget`）；Anthropic 4.6 及以后用自适应思考与 `output_config.effort`（关闭时一般发送 `disabled`，Sonnet 5.5 用 `between_tools`；Opus 5.5 与 Fable 无法关闭，只把强度降到 `low`），更早的模型用 `thinking.budget_tokens`（须给正文留出至少 1024 Token），开启时不发送温度；Gemini 用 `thinkingConfig`（Gemini 3 用 `thinkingLevel` 且无法关闭，关闭时取最低档（初代 3 Flash 为 `minimal`，其余为 `low`）；2.5 指定强度时的预算同样给正文留出 1024 Token；Pro 无法关闭，关闭时取最低预算 128）；其余 OpenAI 兼容服务（DeepSeek、Kimi、GLM、豆包等）用 `thinking.type` 与 `reasoning_effort`。仍不开放原始 JSON 注入。
- **工具调用**：Timelens 没有向模型提供任何工具，移除该项。将来如需让模型按需查询本机统计，另开票据决定工具范围及其与“发送前列出全部内容”的关系。
- 新连接的最大输出默认 8192 Token。提供商未返回上下文大小时，内置目录为已知模型补上上下文窗口（如 DeepSeek `deepseek-flash` 为 1M）；从列表选中这些模型时同时填入较宽裕的最大输出（如 32768），用户仍可修改。
- 思考把正文挤空时，任务失败并说明“思考用完了最大输出”，提示调高最大输出或关闭深度思考，不再显示为数据库完整性错误。
