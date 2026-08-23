# Cherry Studio 普通聊天上下文行为核验

Research date: 2026-08-23
Upstream snapshot: Cherry Studio `main` commit `174398b43f7670b7a3655ca29daa122dcc9d35ad`

## Scope

本记录只核验 Cherry Studio 普通聊天如何持久化消息、构造追问上下文、裁剪或压缩上下文，以及是否依赖提供商服务端会话。Claude Code 等 Agent Session 不在范围内。

## Confirmed facts

- 消息保存在本地 SQLite。每条消息包含 `parentId`，正文、模型、状态、统计和压缩摘要也持久化，因此对话是可分支的本地消息树。[官方消息表定义](https://github.com/CherryHQ/cherry-studio/blob/174398b43f7670b7a3655ca29daa122dcc9d35ad/src/main/data/db/schemas/message.ts#L8-L46)
- 每次追问从当前节点沿父节点读取到根，只构造当前分支，不把兄弟分支一起发送。[官方 MessageService](https://github.com/CherryHQ/cherry-studio/blob/174398b43f7670b7a3655ca29daa122dcc9d35ad/src/main/data/services/MessageService.ts#L1995-L2051)
- 请求上下文先排除最近一次“清除上下文”标记之前的消息，再应用消息数量限制或自动压缩。[官方 PersistentChatContextProvider](https://github.com/CherryHQ/cherry-studio/blob/174398b43f7670b7a3655ca29daa122dcc9d35ad/src/main/ai/streamManager/context/PersistentChatContextProvider.ts#L836-L903)
- 手动消息数量窗口保留最近 N 条，并向前扩展到用户消息，避免上下文只剩孤立的 AI 回复。[官方 maxMessagesWindow](https://github.com/CherryHQ/cherry-studio/blob/174398b43f7670b7a3655ca29daa122dcc9d35ad/src/main/ai/messages/maxMessagesWindow.ts#L1-L45)
- 当前默认不限消息数量并启用自动压缩。[官方上下文默认设置](https://github.com/CherryHQ/cherry-studio/blob/174398b43f7670b7a3655ca29daa122dcc9d35ad/src/shared/data/types/contextSettings.ts#L65-L75)
- 上下文约达到可用窗口 80% 时触发压缩，最近约 30% 保留原文；更早内容连同旧摘要再次总结。摘要写回本地压缩边界消息，原始消息树不删除。[官方阈值常量](https://github.com/CherryHQ/cherry-studio/blob/174398b43f7670b7a3655ca29daa122dcc9d35ad/src/main/ai/constants.ts#L5-L11)、[官方压缩实现](https://github.com/CherryHQ/cherry-studio/blob/174398b43f7670b7a3655ca29daa122dcc9d35ad/src/main/ai/streamManager/context/PersistentChatContextProvider.ts#L917-L1009)
- 每次普通聊天请求把本地构造的上下文转换成完整 `messages` 数组发给模型。[官方 Agent 请求构造](https://github.com/CherryHQ/cherry-studio/blob/174398b43f7670b7a3655ca29daa122dcc9d35ad/src/main/ai/runtime/aiSdk/Agent.ts#L248-L262)
- OpenAI Responses 请求明确设置 `store:false`；Anthropic 与 Gemini 普通聊天参数构造也没有会话续接 ID。[官方提供商参数](https://github.com/CherryHQ/cherry-studio/blob/174398b43f7670b7a3655ca29daa122dcc9d35ad/src/main/ai/utils/options.ts#L303-L373)
- 正常“重新生成”在同一用户问题下创建新的 AI 兄弟节点，保留旧回答并切换到新分支。[官方上下文节点创建](https://github.com/CherryHQ/cherry-studio/blob/174398b43f7670b7a3655ca29daa122dcc9d35ad/src/main/ai/streamManager/context/PersistentChatContextProvider.ts#L290-L305)、[官方重新生成实现](https://github.com/CherryHQ/cherry-studio/blob/174398b43f7670b7a3655ca29daa122dcc9d35ad/src/main/data/services/MessageService.ts#L1239-L1273)
- 只有失败回答的“重试”会原位清空并复用原消息节点，保持父节点、兄弟组、后续分支和当前活动分支不变。[官方失败重试实现](https://github.com/CherryHQ/cherry-studio/blob/174398b43f7670b7a3655ca29daa122dcc9d35ad/src/main/data/services/MessageService.ts#L1464-L1525)

## Inference

普通聊天采用客户端管理上下文、提供商侧无状态的设计。当前核验路径没有发现 OpenAI `previousResponseId`、Anthropic 会话 ID 或 Gemini 会话 ID 的保存与续接。这个结论不适用于 Cherry Studio 的 Agent Session。

## Timelens consequence

- 每个成功总结版本建立本地加密对话树，每次追问发送当前分支。
- 提供清除上下文、最近 N 条限制和默认启用的自动压缩；自动压缩阈值及近期原文比例对齐 Cherry Studio 的 80% 与 30%。
- 不依赖供应商保存会话；OpenAI、Anthropic、Gemini 适配器使用同一客户端上下文语义。
- 正常重新生成创建兄弟分支；只有失败且尚无有效回答的重试原位复用节点。
- 为使源活动被清理后仍可续聊，首次总结使用的隐私过滤结构化数据包应作为隐藏上下文快照本地加密保存，并与总结一起清理。这是 Timelens 为满足既有数据合同所需的延伸，不是 Cherry Studio 活动数据模型的一部分。
