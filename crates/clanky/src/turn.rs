//! The agentic loop (M2): chat turns with tools.
//!
//! Each round streams one chat request through the provider client; if the
//! model asks for tool calls, they are executed via the [`Tool`] registry,
//! their results appended as `tool` messages, and the loop continues until
//! the model answers in plain text (or a safety limit is hit). Streaming
//! deltas and tool activity are surfaced to the caller as [`TurnEvent`]s;
//! in `-p` mode they are printed, in M3 the TUI will render them.

use clanky_protocol::{
    ChatMessage, ChatRequest, ChunkPayload, FinishReason, Handler, LoopbackTransport, Sampling,
    StreamAssembler, Thinking, ToolCall, Usage,
};

use crate::error::{Error, Result};
use crate::settings::SamplingParams;
use crate::tools::ToolSet;

/// Maximum number of chat rounds (tool calls included) in one turn, so a
/// model stuck in a tool-calling cycle cannot run forever.
pub const MAX_TOOL_ROUNDS: usize = 25;

/// Per-turn configuration taken from settings/CLI.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TurnConfig {
    pub model: Option<String>,
    pub sampling: Option<SamplingParams>,
    pub thinking: Option<String>,
}

impl TurnConfig {
    /// Build from merged settings for the chosen provider; the model falls
    /// back to the provider default when unset. Used by both pipe and TUI
    /// modes (M3).
    pub fn from_settings(provider: &str, settings: &crate::settings::Settings) -> Self {
        Self {
            model: settings
                .model
                .clone()
                .or_else(|| crate::provider::default_model(provider).map(str::to_string)),
            sampling: settings.sampling.clone(),
            thinking: settings.thinking.clone(),
        }
    }
}

/// Events emitted while a turn runs.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEvent {
    /// Delta of the assistant's visible text.
    Text { delta: String },
    /// Delta of the assistant's thinking text.
    Thinking { delta: String },
    /// The model asked to call a tool; `arguments` is compact JSON.
    ToolCall { name: String, arguments: String },
    /// The tool finished; `output` is the result fed back to the model.
    ToolResult { name: String, output: String },
}

/// The outcome of one turn.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutput {
    pub text: String,
    pub finish_reason: clanky_protocol::FinishReason,
    pub usage: Option<clanky_protocol::Usage>,
}

/// Run one full agentic turn against `handler` (in-process until M8).
pub fn run_turn(
    handler: Box<dyn Handler>,
    tools: &ToolSet,
    context_messages: Vec<ChatMessage>,
    prompt: &str,
    config: &TurnConfig,
    on_event: &mut dyn FnMut(TurnEvent),
) -> Result<TurnOutput> {
    let mut client = clanky_protocol::ProviderClient::new(LoopbackTransport::new(handler));
    client.handshake()?;

    let tool_capable = client.peer().is_some_and(|peer| peer.capabilities.tools);
    let wire_tools = if tool_capable && !tools.is_empty() {
        Some(tools.iter().map(|tool| tool.wire()).collect::<Vec<_>>())
    } else {
        None
    };

    let model = config.model.clone().ok_or(Error::NoModel)?;
    let sampling = sampling_from(&config.sampling)?;
    let thinking = thinking_from(&config.thinking)?;

    let mut messages = context_messages;
    messages.push(ChatMessage::user(prompt));

    let mut text = String::new();
    let mut usage_total = Usage::default();
    let mut usage_seen = false;

    for _round in 0..MAX_TOOL_ROUNDS {
        let mut assembler = StreamAssembler::default();
        let done = {
            let mut on_chunk = |payload: &ChunkPayload| {
                match &payload {
                    ChunkPayload::Text { text } => {
                        on_event(TurnEvent::Text {
                            delta: text.clone(),
                        });
                    }
                    ChunkPayload::Thinking { text } => {
                        on_event(TurnEvent::Thinking {
                            delta: text.clone(),
                        });
                    }
                    _ => {}
                }
                assembler.ingest(payload);
            };
            let request = ChatRequest {
                model: model.clone(),
                messages: messages.clone(),
                tools: wire_tools.clone(),
                sampling,
                thinking,
            };
            client.chat(request, &mut on_chunk)?
        };

        text.push_str(assembler.text());
        if let Some(usage) = done.usage {
            usage_seen = true;
            usage_total.prompt_tokens =
                Some(usage_total.prompt_tokens.unwrap_or(0) + usage.prompt_tokens.unwrap_or(0));
            usage_total.completion_tokens = Some(
                usage_total.completion_tokens.unwrap_or(0) + usage.completion_tokens.unwrap_or(0),
            );
        }

        let calls = assembler.tool_calls();
        if done.finish_reason != FinishReason::ToolCalls || calls.is_empty() {
            return Ok(TurnOutput {
                text,
                finish_reason: done.finish_reason,
                usage: usage_seen.then_some(usage_total),
            });
        }

        // Record the assistant's tool calls, execute them, feed results back.
        messages.push(ChatMessage::Assistant {
            content: assembler.text().to_string(),
            tool_calls: Some(calls.clone()),
        });
        for call in &calls {
            let output = execute_tool_call(tools, call, on_event);
            messages.push(ChatMessage::tool(call.id.clone(), output));
        }
    }

    Err(Error::ToolLoopLimit(MAX_TOOL_ROUNDS))
}

/// Execute one parsed tool call and return the content for the `tool`
/// message. Unknown tools and malformed arguments become error results the
/// model can react to.
fn execute_tool_call(
    tools: &ToolSet,
    call: &ToolCall,
    on_event: &mut dyn FnMut(TurnEvent),
) -> String {
    on_event(TurnEvent::ToolCall {
        name: call.name.clone(),
        arguments: call.arguments.to_string(),
    });
    let outcome = match tools.iter().find(|tool| tool.name() == call.name) {
        None => Err(crate::tools::ToolError::Failed {
            tool: call.name.clone(),
            message: format!(
                "unknown tool `{}`; available tools: {}",
                call.name,
                tool_names(tools)
            ),
        }),
        Some(_tool) if !call.arguments.is_object() => {
            Err(crate::tools::ToolError::InvalidArguments {
                tool: call.name.clone(),
                message: format!("arguments must be a JSON object, got: {}", call.arguments),
            })
        }
        Some(tool) => tool.execute(&call.arguments),
    };
    let output = match outcome {
        Ok(output) => output,
        Err(err) => format!("ERROR: {err}"),
    };
    on_event(TurnEvent::ToolResult {
        name: call.name.clone(),
        output: output.clone(),
    });
    output
}

fn tool_names(tools: &ToolSet) -> String {
    tools
        .iter()
        .map(|tool| tool.name())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Convert settings sampling parameters (string key/value pairs) into
/// protocol sampling. Unknown keys are rejected so typos surface loudly.
fn sampling_from(params: &Option<SamplingParams>) -> Result<Option<Sampling>> {
    let Some(params) = params else {
        return Ok(None);
    };
    let mut sampling = Sampling::default();
    for (key, raw) in params {
        match key.as_str() {
            "temperature" => sampling.temperature = Some(parse_f64(raw, key)?),
            "top_p" => sampling.top_p = Some(parse_f64(raw, key)?),
            "max_tokens" => {
                sampling.max_tokens = Some(raw.trim().parse().map_err(|_| {
                    Error::InvalidSampling(format!("{key} must be an integer, got `{raw}`"))
                })?)
            }
            other => {
                return Err(Error::InvalidSampling(format!(
                    "unknown sampling parameter `{other}`; supported: temperature, top_p, max_tokens"
                )));
            }
        }
    }
    Ok(Some(sampling))
}

fn parse_f64(raw: &str, key: &str) -> Result<f64> {
    raw.trim()
        .parse()
        .map_err(|_| Error::InvalidSampling(format!("{key} must be a number, got `{raw}`")))
}

/// Interpret the `--thinking` value: `off` disables thinking; a non-negative
/// integer is a token budget. Named levels map onto budgets in the provider
/// adapter (M1 keeps the protocol shaped by budget).
fn thinking_from(value: &Option<String>) -> Result<Option<Thinking>> {
    let Some(value) = value else {
        return Ok(None);
    };
    match value.trim() {
        "" | "off" => Ok(None),
        raw => {
            let budget: u32 = raw
                .parse()
                .map_err(|_| Error::InvalidThinking(raw.into()))?;
            Ok(Some(Thinking {
                budget_tokens: Some(budget),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clanky_protocol::{Capabilities, ChatDone, ErrorCode, ModelInfo, PluginInfo};
    use serde_json::{Value, json};
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// Records the requests it receives and plays back scripted responses.
    struct MockHandler {
        requests: RefCell<Vec<ChatRequest>>,
        script: RefCell<VecDeque<ScriptedTurn>>,
        fail_with: Option<clanky_protocol::Error>,
        caps: Capabilities,
    }

    struct ScriptedTurn {
        chunks: Vec<ChunkPayload>,
        finish_reason: FinishReason,
        usage: Option<Usage>,
    }

    fn text_turn(text: &str) -> ScriptedTurn {
        ScriptedTurn {
            chunks: vec![ChunkPayload::Text { text: text.into() }],
            finish_reason: FinishReason::Stop,
            usage: Some(Usage {
                prompt_tokens: Some(3),
                completion_tokens: Some(2),
            }),
        }
    }

    fn tool_turn(name: &str, args: &str) -> ScriptedTurn {
        ScriptedTurn {
            chunks: vec![
                ChunkPayload::ToolCallStart {
                    index: 0,
                    id: "call_1".into(),
                    name: name.into(),
                },
                ChunkPayload::ToolCallArgs {
                    index: 0,
                    args_chunk: args.into(),
                },
            ],
            finish_reason: FinishReason::ToolCalls,
            usage: Some(Usage {
                prompt_tokens: Some(5),
                completion_tokens: Some(4),
            }),
        }
    }

    impl MockHandler {
        fn scripted(turns: Vec<ScriptedTurn>) -> Self {
            Self {
                requests: RefCell::new(Vec::new()),
                script: RefCell::new(turns.into()),
                fail_with: None,
                caps: Capabilities {
                    list_models: true,
                    thinking: false,
                    tools: true,
                },
            }
        }

        fn new() -> Self {
            Self::scripted(vec![text_turn("echo")])
        }

        fn failing(err: clanky_protocol::Error) -> Self {
            Self {
                requests: RefCell::new(Vec::new()),
                script: RefCell::new(VecDeque::new()),
                fail_with: Some(err),
                caps: Capabilities {
                    list_models: true,
                    thinking: false,
                    tools: true,
                },
            }
        }
    }

    impl Handler for MockHandler {
        fn info(&self) -> PluginInfo {
            PluginInfo {
                name: "mock".into(),
                capabilities: self.caps,
            }
        }

        fn list_models(&mut self) -> std::result::Result<Vec<ModelInfo>, clanky_protocol::Error> {
            Ok(vec![ModelInfo {
                id: "mock/model".into(),
                display_name: None,
                context_window: None,
                supports_thinking: None,
            }])
        }

        fn chat(
            &mut self,
            request: &ChatRequest,
            sink: &mut dyn FnMut(ChunkPayload),
        ) -> std::result::Result<ChatDone, clanky_protocol::Error> {
            if let Some(err) = self.fail_with.take() {
                return Err(err);
            }
            self.requests.borrow_mut().push(request.clone());
            let turn = self
                .script
                .borrow_mut()
                .pop_front()
                .expect("mock script exhausted");
            for chunk in turn.chunks {
                sink(chunk);
            }
            Ok(ChatDone {
                finish_reason: turn.finish_reason,
                usage: turn.usage,
            })
        }
    }

    /// A deterministic echo tool for tests.
    struct EchoTool;

    impl crate::tools::Tool for EchoTool {
        fn name(&self) -> &str {
            "echo_tool"
        }

        fn description(&self) -> &str {
            "Echoes its input"
        }

        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {"input": {"type": "string"}}, "required": ["input"]})
        }

        fn execute(
            &self,
            arguments: &Value,
        ) -> std::result::Result<String, crate::tools::ToolError> {
            match arguments.get("input").and_then(Value::as_str) {
                Some(input) => Ok(format!("echoed: {input}")),
                None => Err(crate::tools::ToolError::InvalidArguments {
                    tool: "echo_tool".into(),
                    message: "missing `input`".into(),
                }),
            }
        }
    }

    fn toolset() -> ToolSet {
        vec![Box::new(EchoTool)]
    }

    fn config(model: &str) -> TurnConfig {
        TurnConfig {
            model: Some(model.into()),
            sampling: None,
            thinking: None,
        }
    }

    /// Run a turn and collect both the output and the emitted events.
    fn run(
        handler: Box<dyn Handler>,
        tools: &ToolSet,
        context: Vec<ChatMessage>,
        prompt: &str,
        config: &TurnConfig,
    ) -> (Result<TurnOutput>, RefCell<Vec<TurnEvent>>) {
        let sink: RefCell<Vec<TurnEvent>> = RefCell::new(Vec::new());
        let mut on_event = |event: TurnEvent| sink.borrow_mut().push(event);
        let output = run_turn(handler, tools, context, prompt, config, &mut on_event);
        (output, sink)
    }

    #[test]
    fn turn_sends_context_then_prompt_and_collects_text() {
        let context = vec![ChatMessage::system("Be terse.")];
        let (output, sink) = run(
            Box::new(MockHandler::new()),
            &toolset(),
            context,
            "say hi",
            &config("mock/model"),
        );
        let output = output.unwrap();
        assert_eq!(output.text, "echo");
        assert_eq!(output.finish_reason, FinishReason::Stop);
        assert_eq!(output.usage.and_then(|u| u.completion_tokens), Some(2));
        assert_eq!(
            sink.into_inner(),
            vec![TurnEvent::Text {
                delta: "echo".into()
            }]
        );
    }

    #[test]
    fn tool_calls_are_executed_and_results_fed_back() {
        let handler = Box::new(MockHandler::scripted(vec![
            tool_turn("echo_tool", r#"{"input": "hello"}"#),
            text_turn("done"),
        ]));
        let (output, sink) = run(
            handler,
            &toolset(),
            vec![],
            "use the tool",
            &config("mock/model"),
        );
        let output = output.unwrap();
        assert_eq!(output.text, "done");

        // The second request must contain the assistant tool call + result.
        // (Recorded requests are checked via the golden test; here we check
        // the event stream.)
        let sink = sink.into_inner();
        assert_eq!(
            sink,
            vec![
                TurnEvent::ToolCall {
                    name: "echo_tool".into(),
                    arguments: r#"{"input":"hello"}"#.into(),
                },
                TurnEvent::ToolResult {
                    name: "echo_tool".into(),
                    output: "echoed: hello".into(),
                },
                TurnEvent::Text {
                    delta: "done".into()
                },
            ]
        );
    }

    #[test]
    fn unknown_tool_yields_an_error_result_not_a_crash() {
        let handler = Box::new(MockHandler::scripted(vec![
            tool_turn("nonexistent", r#"{}"#),
            text_turn("recovered"),
        ]));
        let (output, sink) = run(
            handler,
            &toolset(),
            vec![],
            "call it",
            &config("mock/model"),
        );
        assert_eq!(output.unwrap().text, "recovered");
        let sink = sink.into_inner();
        assert!(
            matches!(&sink[1], TurnEvent::ToolResult { output, .. } if output.starts_with("ERROR:")),
            "unexpected events: {sink:?}"
        );
    }

    #[test]
    fn malformed_arguments_yield_an_error_result() {
        let handler = Box::new(MockHandler::scripted(vec![
            tool_turn("echo_tool", "not json"),
            text_turn("recovered"),
        ]));
        let (output, sink) = run(
            handler,
            &toolset(),
            vec![],
            "call it",
            &config("mock/model"),
        );
        assert_eq!(output.unwrap().text, "recovered");
        let sink = sink.into_inner();
        assert!(
            matches!(&sink[1], TurnEvent::ToolResult { output, .. } if output.contains("ERROR:")),
            "unexpected events: {sink:?}"
        );
    }

    #[test]
    fn providers_without_tool_capability_get_a_plain_chat() {
        let mut handler = MockHandler::scripted(vec![text_turn("plain")]);
        handler.caps.tools = false;
        let (output, sink) = run(
            Box::new(handler),
            &toolset(),
            vec![],
            "hi",
            &config("mock/model"),
        );
        assert_eq!(output.unwrap().text, "plain");
        assert_eq!(
            sink.into_inner(),
            vec![TurnEvent::Text {
                delta: "plain".into()
            }]
        );
    }

    #[test]
    fn no_tools_registered_means_no_tools_offered() {
        let handler = Box::new(MockHandler::scripted(vec![text_turn("plain")]));
        let (output, _) = run(handler, &Vec::new(), vec![], "hi", &config("mock/model"));
        assert_eq!(output.unwrap().text, "plain");
    }

    #[test]
    fn endless_tool_loop_hits_the_limit() {
        let handler = Box::new(MockHandler::scripted(
            std::iter::repeat_with(|| tool_turn("echo_tool", r#"{}"#))
                .take(MAX_TOOL_ROUNDS + 2)
                .collect(),
        ));
        let (output, _) = run(
            handler,
            &toolset(),
            vec![],
            "loop forever",
            &config("mock/model"),
        );
        let err = output.unwrap_err();
        assert!(
            matches!(err, Error::ToolLoopLimit(MAX_TOOL_ROUNDS)),
            "{err}"
        );
    }

    #[test]
    fn turn_requires_model() {
        let (output, _) = run(
            Box::new(MockHandler::new()),
            &toolset(),
            vec![],
            "hi",
            &TurnConfig::default(),
        );
        assert!(matches!(output.unwrap_err(), Error::NoModel));
    }

    #[test]
    fn provider_errors_surface_with_code() {
        let handler = MockHandler::failing(clanky_protocol::Error::Provider {
            code: ErrorCode::Auth,
            message: "401: invalid API key".into(),
            retryable: false,
        });
        let (output, _) = run(
            Box::new(handler),
            &toolset(),
            vec![],
            "hi",
            &config("mock/model"),
        );
        match output.unwrap_err() {
            Error::Provider(clanky_protocol::Error::Provider {
                code,
                message,
                retryable,
            }) => {
                assert_eq!(code, ErrorCode::Auth);
                assert_eq!(message, "401: invalid API key");
                assert!(!retryable);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn usage_is_summed_across_rounds() {
        let handler = Box::new(MockHandler::scripted(vec![
            tool_turn("echo_tool", r#"{"input": "x"}"#), // 5/4
            text_turn("done"),                           // 3/2
        ]));
        let (output, _) = run(handler, &toolset(), vec![], "hi", &config("mock/model"));
        let output = output.unwrap();
        assert_eq!(
            output.usage,
            Some(Usage {
                prompt_tokens: Some(8),
                completion_tokens: Some(6)
            })
        );
    }

    #[test]
    fn sampling_params_convert() {
        let params: SamplingParams = [
            ("temperature", "0.7"),
            ("top_p", "0.9"),
            ("max_tokens", "512"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let sampling = sampling_from(&Some(params)).unwrap().unwrap();
        assert_eq!(
            sampling,
            Sampling {
                temperature: Some(0.7),
                top_p: Some(0.9),
                max_tokens: Some(512)
            }
        );
    }

    #[test]
    fn sampling_rejects_unknown_keys_and_bad_values() {
        let params: SamplingParams = [("foo", "1")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let err = sampling_from(&Some(params)).unwrap_err();
        assert!(err.to_string().contains("unknown sampling parameter `foo`"));

        let params: SamplingParams = [("temperature", "hot")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let err = sampling_from(&Some(params)).unwrap_err();
        assert!(err.to_string().contains("must be a number"));
    }

    #[test]
    fn thinking_values_convert() {
        assert_eq!(thinking_from(&None).unwrap(), None);
        assert_eq!(thinking_from(&Some("off".into())).unwrap(), None);
        assert_eq!(
            thinking_from(&Some("2048".into())).unwrap(),
            Some(Thinking {
                budget_tokens: Some(2048)
            })
        );
        let err = thinking_from(&Some("lots".into())).unwrap_err();
        assert!(
            err.to_string().contains("invalid --thinking"),
            "unexpected: {err}"
        );
    }
}
