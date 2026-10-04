//! Assembling a stream of [`ChunkPayload`]s into a complete assistant turn
//! (spec §6).
//!
//! `text` deltas append to the assistant text; tool calls are announced by
//! `toolCallStart` (index → id + name) and their JSON argument string is
//! assembled from `toolCallArgs` fragments, then parsed once the turn is
//! done. This is a client-side concern, shared by every transport.

use serde_json::Value;

use crate::messages::{ChunkPayload, ToolCall};

/// Accumulates one chat turn's stream events.
#[derive(Debug, Default)]
pub struct StreamAssembler {
    text: String,
    thinking: String,
    calls: Vec<AssembledCall>,
}

#[derive(Debug, Default)]
struct AssembledCall {
    id: String,
    name: String,
    args: String,
    started: bool,
}

impl StreamAssembler {
    /// Ingest one stream event.
    pub fn ingest(&mut self, payload: &ChunkPayload) {
        match payload {
            ChunkPayload::Text { text } => self.text.push_str(text),
            // Thinking deltas are assembled for display/session storage;
            // they are not part of the assistant message sent back.
            ChunkPayload::Thinking { text } => self.thinking.push_str(text),
            ChunkPayload::ToolCallStart { index, id, name } => {
                let slot = self.slot(*index);
                slot.id = id.clone();
                slot.name = name.clone();
                slot.started = true;
            }
            ChunkPayload::ToolCallArgs { index, args_chunk } => {
                self.slot(*index).args.push_str(args_chunk);
            }
        }
    }

    fn slot(&mut self, index: u32) -> &mut AssembledCall {
        while self.calls.len() <= index as usize {
            self.calls.push(AssembledCall::default());
        }
        &mut self.calls[index as usize]
    }

    /// The assistant's visible text, assembled from `text` deltas.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The assistant's thinking text, assembled from `thinking` deltas.
    pub fn thinking(&self) -> &str {
        &self.thinking
    }

    /// The tool calls announced during the turn, with argument strings
    /// parsed to JSON. Malformed argument strings surface as a plain
    /// string value (not an object) so the caller can react; empty
    /// argument strings become an empty object.
    ///
    /// Calls that were never announced (`toolCallStart`) are dropped —
    /// they can only result from indices arriving out of order.
    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.calls
            .iter()
            .enumerate()
            .filter(|(_, call)| call.started)
            .map(|(index, call)| ToolCall {
                id: if call.id.is_empty() {
                    format!("call_{index}")
                } else {
                    call.id.clone()
                },
                name: call.name.clone(),
                arguments: parse_arguments(&call.args),
            })
            .collect()
    }
}

fn parse_arguments(args: &str) -> Value {
    if args.trim().is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str(args).unwrap_or_else(|_| Value::String(args.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ingest_all(assembler: &mut StreamAssembler, payloads: &[ChunkPayload]) {
        for payload in payloads {
            assembler.ingest(payload);
        }
    }

    #[test]
    fn text_deltas_concatenate() {
        let mut assembler = StreamAssembler::default();
        ingest_all(
            &mut assembler,
            &[
                ChunkPayload::Text {
                    text: "There ".into(),
                },
                ChunkPayload::Thinking { text: "hmm".into() },
                ChunkPayload::Text {
                    text: "are 42".into(),
                },
            ],
        );
        assert_eq!(assembler.text(), "There are 42");
        assert!(assembler.tool_calls().is_empty());
    }

    #[test]
    fn thinking_deltas_concatenate_separately() {
        let mut assembler = StreamAssembler::default();
        ingest_all(
            &mut assembler,
            &[
                ChunkPayload::Thinking {
                    text: "need to".into(),
                },
                ChunkPayload::Text { text: "hi".into() },
                ChunkPayload::Thinking {
                    text: " check".into(),
                },
            ],
        );
        assert_eq!(assembler.text(), "hi");
        assert_eq!(assembler.thinking(), "need to check");
    }

    #[test]
    fn tool_call_is_assembled_from_fragments() {
        let mut assembler = StreamAssembler::default();
        ingest_all(
            &mut assembler,
            &[
                ChunkPayload::ToolCallStart {
                    index: 0,
                    id: "call_1".into(),
                    name: "bash".into(),
                },
                ChunkPayload::ToolCallArgs {
                    index: 0,
                    args_chunk: "{\"command\":".into(),
                },
                ChunkPayload::ToolCallArgs {
                    index: 0,
                    args_chunk: " \"ls\"}".into(),
                },
            ],
        );
        assert_eq!(
            assembler.tool_calls(),
            vec![ToolCall {
                id: "call_1".into(),
                name: "bash".into(),
                arguments: json!({"command": "ls"}),
            }]
        );
    }

    #[test]
    fn multiple_calls_and_synthetic_ids() {
        let mut assembler = StreamAssembler::default();
        ingest_all(
            &mut assembler,
            &[
                ChunkPayload::ToolCallStart {
                    index: 0,
                    id: "".into(),
                    name: "a".into(),
                },
                ChunkPayload::ToolCallStart {
                    index: 1,
                    id: "call_2".into(),
                    name: "b".into(),
                },
                ChunkPayload::ToolCallArgs {
                    index: 1,
                    args_chunk: "{}".into(),
                },
            ],
        );
        let calls = assembler.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_0");
        assert_eq!(calls[0].arguments, json!({}));
        assert_eq!(calls[1].id, "call_2");
        assert_eq!(calls[1].arguments, json!({}));
    }

    #[test]
    fn malformed_arguments_surface_as_string_value() {
        let mut assembler = StreamAssembler::default();
        ingest_all(
            &mut assembler,
            &[
                ChunkPayload::ToolCallStart {
                    index: 0,
                    id: "call_1".into(),
                    name: "bash".into(),
                },
                ChunkPayload::ToolCallArgs {
                    index: 0,
                    args_chunk: "not json".into(),
                },
            ],
        );
        let calls = assembler.tool_calls();
        assert_eq!(calls[0].arguments, json!("not json"));
    }

    #[test]
    fn empty_arguments_become_empty_object() {
        let mut assembler = StreamAssembler::default();
        ingest_all(
            &mut assembler,
            &[ChunkPayload::ToolCallStart {
                index: 0,
                id: "call_1".into(),
                name: "noop".into(),
            }],
        );
        assert_eq!(assembler.tool_calls()[0].arguments, json!({}));
    }

    #[test]
    fn orphan_args_without_start_are_dropped() {
        let mut assembler = StreamAssembler::default();
        assembler.ingest(&ChunkPayload::ToolCallArgs {
            index: 3,
            args_chunk: "{}".into(),
        });
        assert!(assembler.tool_calls().is_empty());
    }
}
