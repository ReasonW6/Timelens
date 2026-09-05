use crate::{ChatMessage, Role};

/// Conservative bound used for preflight and local fallback, not billed token usage.
pub fn estimated_tokens(text: &str) -> usize {
    let ascii = text.chars().filter(char::is_ascii).count();
    ascii.div_ceil(3) + (text.chars().count() - ascii) * 2 + 8
}
pub fn messages_tokens(messages: &[ChatMessage]) -> usize {
    messages.iter().map(|m| estimated_tokens(&m.text) + 8).sum()
}

#[derive(Default)]
pub struct ContextPlan {
    pub older: Vec<ChatMessage>,
    pub recent: Vec<ChatMessage>,
    pub needs_compression: bool,
    pub omitted: bool,
}

pub fn plan_context(
    messages: &[ChatMessage],
    model_window: u32,
    reserved_tokens: usize,
    recent_limit: Option<usize>,
    compression_enabled: bool,
) -> ContextPlan {
    let available = (model_window as usize)
        .saturating_sub(reserved_tokens)
        .max(1);
    let mut start = recent_limit.map_or(0, |n| messages.len().saturating_sub(n.max(1)));
    while start > 0 && messages.get(start).is_some_and(|m| m.role != Role::User) {
        start -= 1;
    }
    let chosen = &messages[start..];
    if messages_tokens(chosen) < available * 8 / 10 {
        return ContextPlan {
            recent: chosen.to_vec(),
            omitted: start > 0,
            ..ContextPlan::default()
        };
    }
    let keep_budget = if compression_enabled {
        available * 3 / 10
    } else {
        available
    };
    let mut keep_start = chosen.len();
    let mut used = 0;
    for i in (0..chosen.len()).rev() {
        used += estimated_tokens(&chosen[i].text) + 8;
        if chosen[i].role == Role::User {
            if used > keep_budget && keep_start != chosen.len() {
                break;
            }
            keep_start = i;
        }
    }
    let older = chosen[..keep_start].to_vec();
    let recent = chosen[keep_start..].to_vec();
    ContextPlan {
        needs_compression: compression_enabled && !older.is_empty(),
        omitted: start > 0 || (!compression_enabled && !older.is_empty()),
        older,
        recent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_count_window_expands_to_a_complete_user_turn_and_preserves_the_source() {
        let messages = vec![
            ChatMessage {
                role: Role::User,
                text: "first".into(),
            },
            ChatMessage {
                role: Role::Assistant,
                text: "first answer".into(),
            },
            ChatMessage {
                role: Role::User,
                text: "second".into(),
            },
        ];
        let plan = plan_context(&messages, 4096, 100, Some(2), true);
        assert_eq!(plan.recent[0].role, Role::User);
        assert_eq!(plan.recent.len(), 3);
        assert_eq!(messages[1].text, "first answer");
    }
    #[test]
    fn compression_keeps_recent_complete_turns_without_deleting_original_messages() {
        let messages = (0..30)
            .map(|i| ChatMessage {
                role: if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                text: "long context ".repeat(50),
            })
            .collect::<Vec<_>>();
        let plan = plan_context(&messages, 4096, 500, None, true);
        assert!(plan.needs_compression);
        assert_eq!(plan.recent[0].role, Role::User);
        assert_eq!(plan.older.len() + plan.recent.len(), 30);
    }
}
