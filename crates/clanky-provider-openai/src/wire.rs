//! Serializers for the OpenAI-compatible request shape: chat messages,
//! tools, and the small helpers they share.
//!
//! Only the *mechanism* lives here. A provider's reasoning passback is
//! policy (DeepInfra documents `reasoning_content`; LiteLLM needs
//! `thinking_blocks` too), so [`openai_messages`] takes a callback that
//! decides how an assistant message's reasoning blocks go on the wire.

use clanky_protocol::{ChatMessage, ImagePart, ThinkingBlock, Tool};
use serde_json::{Map, Value, json};

/// Callback for an assistant message's reasoning passback: given the
/// message's JSON object (already carrying `role`, `content` and
/// `tool_calls`) and its reasoning blocks, add whatever fields the provider
/// documents. Called only for assistant messages that carry blocks.
pub type ReasoningPassback<'a> = &'a dyn Fn(&mut Map<String, Value>, &[ThinkingBlock]);

/// Serialize protocol chat messages into OpenAI-compatible JSON messages.
///
/// A tool result's inline images cannot ride in the `tool` message: the
/// OpenAI-compatible tool-message schema accepts only a plain string (or a
/// text-only array) for `content` — an `image_url` part there is a 422
/// ("messages.N.tool.content.str: Input should be a valid string", observed
/// live against a DeepInfra vision model). The images instead ride in a
/// *trailing `user` message* with `image_url` data-URI parts, emitted only
/// when the tool result carries images (so every image-free request is
/// byte-identical to the image-free shape) and deterministically ordered —
/// directly after the `tool` message it belongs to — so replay reconstructs
/// exactly what the model saw.
///
/// `reasoning` receives each assistant message that carries thinking blocks;
/// a message with no blocks never calls it, so a blockless request stays
/// byte-identical to the pre-M23 shape.
pub fn openai_messages(messages: &[ChatMessage], reasoning: ReasoningPassback<'_>) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m {
            ChatMessage::System { content } | ChatMessage::User { content } => {
                out.push(json!({ "role": role_name(m), "content": content }));
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
                images,
            } => {
                // `role: "tool"` takes only a plain string `content`
                // (see the function doc); the text is the result, the
                // images (if any) follow in a synthetic user turn.
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content,
                }));
                if let Some(images) = images.as_deref().filter(|imgs| !imgs.is_empty()) {
                    out.push(image_parts_user_message(images));
                }
            }
            ChatMessage::Assistant {
                content,
                tool_calls,
                thinking,
            } => {
                let mut msg = json!({ "role": "assistant" });
                let obj = msg.as_object_mut().expect("object literal");
                if !content.is_empty() {
                    obj.insert("content".into(), json!(content));
                }
                if let Some(blocks) = thinking.as_deref().filter(|blocks| !blocks.is_empty()) {
                    reasoning(obj, blocks);
                }
                if let Some(calls) = tool_calls {
                    obj.insert(
                        "tool_calls".into(),
                        Value::Array(
                            calls
                                .iter()
                                .map(|c| {
                                    json!({
                                        "id": c.id,
                                        "type": "function",
                                        "function": {
                                            "name": c.name,
                                            // OpenAI carries arguments as a JSON *string*.
                                            "arguments": c.arguments.to_string(),
                                        }
                                    })
                                })
                                .collect(),
                        ),
                    );
                }
                out.push(msg);
            }
        }
    }
    out
}

/// The `reasoning_content` an assistant message carries back to an
/// OpenAI-compatible backend: its reasoning blocks' concatenated text (M23
/// session 2). An empty string when the blocks carry no text, so a
/// no-text message stays byte-identical to sending nothing (an empty
/// `reasoning_content` would be a forged field).
pub fn reasoning_content(blocks: &[ThinkingBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| block.text.as_deref())
        .collect::<Vec<_>>()
        .join("")
}

/// The `user` message that carries a tool result's images: an array of
/// bare `image_url` data-URI parts (no text — the result's text stays in
/// the `tool` message; duplicating it would send it twice). `role:
/// "user"` is the only role whose schema accepts `image_url` parts.
pub fn image_parts_user_message(images: &[ImagePart]) -> Value {
    json!({
        "role": "user",
        "content": images
            .iter()
            .map(|image| json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{};base64,{}", image.media_type.as_str(), image.data_base64),
                },
            }))
            .collect::<Vec<_>>(),
    })
}

pub fn role_name(message: &ChatMessage) -> &'static str {
    match message {
        ChatMessage::System { .. } => "system",
        ChatMessage::User { .. } => "user",
        ChatMessage::Assistant { .. } => "assistant",
        ChatMessage::Tool { .. } => "tool",
    }
}

pub fn openai_tools(tools: &[Tool]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clanky_protocol::{MediaType, ToolCall};
    use serde_json::json;

    fn no_reasoning() -> impl Fn(&mut Map<String, Value>, &[ThinkingBlock]) {
        |_, _| panic!("no reasoning blocks in these messages")
    }

    #[test]
    fn messages_serialize_roles_content_and_tool_calls() {
        let messages = vec![
            ChatMessage::system("be terse"),
            ChatMessage::user("hi"),
            ChatMessage::Assistant {
                content: String::new(),
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "ls"}),
                }]),
                thinking: None,
            },
            ChatMessage::tool("call_1", "a.txt"),
        ];
        let out = openai_messages(&messages, &no_reasoning());
        assert_eq!(out[0], json!({"role": "system", "content": "be terse"}));
        assert_eq!(out[1], json!({"role": "user", "content": "hi"}));
        // An empty assistant content is omitted; args ride as a JSON string.
        assert_eq!(
            out[2],
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"},
                }],
            })
        );
        assert_eq!(
            out[3],
            json!({"role": "tool", "tool_call_id": "call_1", "content": "a.txt"})
        );
    }

    #[test]
    fn tool_images_ride_in_a_trailing_user_message() {
        let messages = vec![ChatMessage::Tool {
            tool_call_id: "call_1".into(),
            content: "screenshot".into(),
            images: Some(vec![ImagePart {
                media_type: MediaType::Png,
                data_base64: "QUJD".into(),
            }]),
        }];
        let out = openai_messages(&messages, &no_reasoning());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(
            out[1],
            json!({
                "role": "user",
                "content": [{
                    "type": "image_url",
                    "image_url": {"url": "data:image/png;base64,QUJD"},
                }],
            })
        );
    }

    #[test]
    fn a_tool_result_without_images_adds_no_user_message() {
        let messages = vec![ChatMessage::tool("call_1", "plain")];
        assert_eq!(openai_messages(&messages, &no_reasoning()).len(), 1);
    }

    #[test]
    fn reasoning_content_joins_text_and_skips_textless_blocks() {
        let blocks = vec![
            ThinkingBlock::thinking("a", Some("sig".into())),
            ThinkingBlock::redacted("blob"),
            ThinkingBlock::thinking("b", None),
        ];
        assert_eq!(reasoning_content(&blocks), "ab");
        assert_eq!(reasoning_content(&[ThinkingBlock::redacted("blob")]), "");
    }

    #[test]
    fn the_reasoning_callback_runs_only_for_messages_with_blocks() {
        let messages = vec![
            ChatMessage::user("hi"),
            ChatMessage::assistant("no blocks"),
            ChatMessage::Assistant {
                content: "with blocks".into(),
                tool_calls: None,
                thinking: Some(vec![ThinkingBlock::thinking("t", None)]),
            },
        ];
        let seen = std::cell::RefCell::new(Vec::new());
        let out = openai_messages(&messages, &|obj, blocks| {
            seen.borrow_mut().push(reasoning_content(blocks));
            obj.insert("reasoning_content".into(), json!(reasoning_content(blocks)));
        });
        assert_eq!(seen.borrow().as_slice(), ["t"]);
        assert_eq!(out[1], json!({"role": "assistant", "content": "no blocks"}));
        assert_eq!(out[2]["reasoning_content"], "t");
    }

    #[test]
    fn tools_serialize_as_openai_functions() {
        let tools = vec![Tool {
            name: "bash".into(),
            description: Some("run a command".into()),
            parameters: Some(json!({"type": "object"})),
        }];
        assert_eq!(
            openai_tools(&tools),
            json!([{
                "type": "function",
                "function": {
                    "name": "bash",
                    "description": "run a command",
                    "parameters": {"type": "object"},
                },
            }])
        );
    }
}
