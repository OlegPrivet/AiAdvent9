use serde_json::Value;
use thiserror::Error;

use crate::api::ApiMessage;

// No universal tokenizer exists for all selectable models. Count UTF-8 bytes as
// tokens conservatively, plus chat-template overhead. This intentionally trims
// earlier than model-specific BPE tokenization; it is not an exact usage count.
const TEMPLATE_RESERVE: usize = 1024;

#[derive(Debug, Error)]
#[error(
    "текущий запрос, системные инструкции и резерв ответа превышают контекст {limit} токенов (консервативная оценка: {required}); уменьшите запрос/max_tokens или измените «Контекст» в /settings"
)]
pub(crate) struct ContextError {
    limit: u32,
    required: usize,
}

fn message_cost(message: &ApiMessage) -> usize {
    let mut cost = 16_usize.saturating_add(message.role.len());
    cost = cost.saturating_add(message.content.as_ref().map_or(0, String::len));
    cost = cost.saturating_add(message.tool_call_id.as_ref().map_or(0, String::len));
    for call in &message.tool_calls {
        cost = cost
            .saturating_add(call.id.len())
            .saturating_add(call.function.name.len())
            .saturating_add(call.function.arguments.len())
            .saturating_add(16);
    }
    cost
}

/// Remove complete oldest user turns only. System messages and the current
/// user request with its assistant/tool transcript are always retained intact.
pub(crate) fn fit_messages(
    messages: &[ApiMessage],
    tools: Option<&Value>,
    limit: u32,
    output_tokens: u32,
) -> Result<Vec<ApiMessage>, ContextError> {
    let costs: Vec<_> = messages.iter().map(message_cost).collect();
    let reserve = TEMPLATE_RESERVE
        .saturating_add(output_tokens as usize)
        .saturating_add(tools.map_or(0, |value| value.to_string().len()));
    let mut total = costs
        .iter()
        .fold(reserve, |sum, cost| sum.saturating_add(*cost));
    let user_indices: Vec<_> = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| (message.role == "user").then_some(index))
        .collect();
    let mut keep = vec![true; messages.len()];
    for users in user_indices.windows(2) {
        if total <= limit as usize {
            break;
        }
        for index in users[0]..users[1] {
            if !matches!(messages[index].role.as_str(), "system" | "developer") {
                keep[index] = false;
                total = total.saturating_sub(costs[index]);
            }
        }
    }
    if total > limit as usize {
        return Err(ContextError {
            limit,
            required: total,
        });
    }
    Ok(messages
        .iter()
        .zip(keep)
        .filter(|(_, keep)| *keep)
        .map(|(message, _)| message.clone())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ToolCall, ToolFunction};

    #[test]
    fn drops_oldest_complete_exchanges_and_keeps_current_tool_transcript() {
        let mut assistant = ApiMessage::text("assistant", "");
        assistant.tool_calls = vec![ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: ToolFunction {
                name: "delegate_task".into(),
                arguments: "{}".into(),
            },
        }];
        let mut tool = ApiMessage::text("tool", "Результат");
        tool.tool_call_id = Some("call_1".into());
        let messages = vec![
            ApiMessage::text("system", "Инструкции"),
            ApiMessage::text("user", "x".repeat(1000)),
            ApiMessage::text("assistant", "y".repeat(1000)),
            ApiMessage::text("user", "Свежий запрос"),
            assistant,
            tool,
        ];
        let fitted = fit_messages(&messages, None, 2000, 100).expect("fit");
        assert_eq!(fitted.len(), 4);
        assert_eq!(fitted[0].role, "system");
        assert_eq!(fitted[1].content.as_deref(), Some("Свежий запрос"));
        assert_eq!(
            fitted[2].tool_calls[0].id,
            fitted[3].tool_call_id.as_deref().expect("id")
        );
        assert_eq!(messages.len(), 6, "input history must remain unchanged");
    }

    #[test]
    fn rejects_oversized_current_question_system_or_tools_without_truncation() {
        for (messages, tools) in [
            (vec![ApiMessage::text("user", "x".repeat(2000))], None),
            (
                vec![
                    ApiMessage::text("system", "x".repeat(2000)),
                    ApiMessage::text("user", "Hi"),
                ],
                None,
            ),
            (
                vec![ApiMessage::text("user", "Hi")],
                Some(serde_json::json!("x".repeat(2000))),
            ),
        ] {
            assert!(fit_messages(&messages, tools.as_ref(), 2000, 100).is_err());
        }
    }

    #[test]
    fn default_window_accepts_large_history_and_reserves_answer_tokens() {
        let messages = vec![
            ApiMessage::text("user", "x".repeat(100_000)),
            ApiMessage::text("assistant", "y".repeat(50_000)),
            ApiMessage::text("user", "Next"),
        ];
        assert_eq!(
            fit_messages(&messages, None, 200_000, 10_000)
                .expect("200k")
                .len(),
            3
        );
        assert_eq!(
            fit_messages(&messages, None, 200_000, 60_000)
                .expect("reserve")
                .len(),
            1
        );
    }
}
