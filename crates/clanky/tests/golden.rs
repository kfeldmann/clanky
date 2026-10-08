//! Golden-file transcripts (M2).
//!
//! A scripted mock provider drives the agentic loop: each chat request the
//! loop issues is recorded as JSON and compared against a golden file in
//! `tests/golden/`. This pins the full request sequence — context, prompt,
//! assistant tool calls, and fed-back tool results — exactly as it would be
//! sent to a real provider.
//!
//! Regenerate goldens with: `CLANKY_UPDATE_GOLDEN=1 cargo test --test golden`

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;

use clanky::tools::{Tool, ToolError, ToolSet};
use clanky::turn::{TurnConfig, run_turn};
use clanky_protocol::{
    Capabilities, ChatDone, ChatMessage, ChatRequest, ChunkPayload, Error as ProtocolError,
    FinishReason, Handler, PluginInfo, Usage,
};
use serde_json::{Value, json};

/// One scripted provider response.
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
            prompt_tokens: Some(7),
            completion_tokens: Some(3),
            cached_tokens: None,
        }),
    }
}

/// Scripted mock provider: consumes its script one chat per turn and records
/// every request for the golden transcript.
struct ScriptedProvider {
    requests: Rc<RefCell<Vec<Value>>>,
    script: RefCell<VecDeque<ScriptedTurn>>,
}

impl ScriptedProvider {
    fn new(script: Vec<ScriptedTurn>) -> (Self, Rc<RefCell<Vec<Value>>>) {
        let requests = Rc::new(RefCell::new(Vec::new()));
        (
            Self {
                requests: Rc::clone(&requests),
                script: RefCell::new(script.into()),
            },
            requests,
        )
    }
}

impl Handler for ScriptedProvider {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: "mock".into(),
            capabilities: Capabilities {
                list_models: false,
                thinking: true,
                tools: true,
            },
        }
    }

    fn list_models(
        &mut self,
    ) -> std::result::Result<Vec<clanky_protocol::ModelInfo>, ProtocolError> {
        Ok(Vec::new())
    }

    fn chat(
        &mut self,
        request: &ChatRequest,
        sink: &mut dyn FnMut(ChunkPayload),
    ) -> std::result::Result<ChatDone, ProtocolError> {
        self.requests.borrow_mut().push(transcript_value(request));
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

/// One chat request as a JSON transcript record (the wire shape Clanky would
/// actually send to the provider plugin).
fn transcript_value(request: &ChatRequest) -> Value {
    json!({
        "model": request.model,
        "messages": request.messages,
        "tools": request.tools,
        "sampling": request.sampling,
        "thinking": request.thinking,
    })
}

/// A deterministic fake tool, so the golden transcript needs no real shell.
struct FakeTool;

impl Tool for FakeTool {
    fn name(&self) -> &str {
        "fake"
    }

    fn description(&self) -> &str {
        "A fake tool for tests"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"input": {"type": "string"}},
            "required": ["input"]
        })
    }

    fn execute(&self, arguments: &Value) -> std::result::Result<String, ToolError> {
        match arguments.get("input").and_then(Value::as_str) {
            Some(input) => Ok(format!("fake ran with input={input}")),
            None => Err(ToolError::InvalidArguments {
                tool: "fake".into(),
                message: "missing `input`".into(),
            }),
        }
    }
}

fn fake_tools() -> ToolSet {
    vec![Box::new(FakeTool)]
}

fn bash_tools() -> ToolSet {
    clanky::tools::default_tools()
}

fn config() -> TurnConfig {
    TurnConfig {
        model: Some("mock/model".into()),
        sampling: Some(
            [
                ("temperature", "0.7"),
                ("top_p", "0.9"),
                ("max_tokens", "4096"),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ),
        thinking: Some("2048".into()),
        max_tool_rounds: None,
        max_retries: Some(0),
        retry_sleep: Some(|_| {}),
    }
}

fn tool_call_chunks(id: &str, name: &str, args: &str) -> Vec<ChunkPayload> {
    vec![
        ChunkPayload::ToolCallStart {
            index: 0,
            id: id.into(),
            name: name.into(),
        },
        ChunkPayload::ToolCallArgs {
            index: 0,
            args_chunk: args.into(),
        },
    ]
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(format!("{name}.json"))
}

fn compare_or_update(name: &str, actual: &Value) {
    let path = golden_path(name);
    if std::env::var_os("CLANKY_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().expect("golden dir")).expect("create golden dir");
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(actual).unwrap()),
        )
        .expect("write golden file");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden file {}: regenerate with CLANKY_UPDATE_GOLDEN=1 ({e})",
            path.display()
        )
    });
    let expected: Value = serde_json::from_str(&expected).expect("golden file is valid JSON");
    assert_eq!(
        *actual, expected,
        "transcript mismatch for {name}; regenerate with CLANKY_UPDATE_GOLDEN=1"
    );
}

#[test]
fn tool_loop_transcript_matches_golden() {
    let script = vec![
        // Round 1: the model asks for the fake tool.
        ScriptedTurn {
            chunks: [
                vec![ChunkPayload::Text {
                    text: "Let me use the tool.".into(),
                }],
                tool_call_chunks("call_1", "fake", r#"{"input": "hello"}"#),
            ]
            .concat(),
            finish_reason: FinishReason::ToolCalls,
            usage: Some(Usage {
                prompt_tokens: Some(12),
                completion_tokens: Some(9),
                cached_tokens: None,
            }),
        },
        // Round 2: it answers with the tool result in hand.
        text_turn("The tool said: fake ran with input=hello"),
    ];
    let (provider, requests) = ScriptedProvider::new(script);
    let mut messages = [
        vec![ChatMessage::system("You are a terminal coding agent.")],
        vec![ChatMessage::user("use the tool on hello")],
    ]
    .concat();
    let output = run_turn(
        Box::new(provider),
        &fake_tools(),
        &mut messages,
        &config(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        output.text,
        "Let me use the tool.The tool said: fake ran with input=hello"
    );

    let transcript = json!({
        "requests": *requests.borrow(),
        "output": {
            "text": output.text,
            "finishReason": output.finish_reason,
            "usage": output.usage,
        }
    });
    compare_or_update("tool-loop", &transcript);
}

#[test]
fn bash_tool_transcript_matches_golden() {
    // The real bash tool, driven by a deterministic command.
    let script = vec![
        ScriptedTurn {
            chunks: tool_call_chunks("call_1", "bash", r#"{"command": "echo 42"}"#),
            finish_reason: FinishReason::ToolCalls,
            usage: Some(Usage {
                prompt_tokens: Some(15),
                completion_tokens: Some(6),
                cached_tokens: None,
            }),
        },
        text_turn("The command printed 42."),
    ];
    let (provider, requests) = ScriptedProvider::new(script);
    let mut messages = vec![ChatMessage::user(
        "run `echo 42` and tell me what it printed",
    )];
    let output = run_turn(
        Box::new(provider),
        &bash_tools(),
        &mut messages,
        &config(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(output.text, "The command printed 42.");

    // The fed-back tool result must be exactly what bash produced.
    let transcript = requests.borrow();
    let round2 = transcript
        .last()
        .expect("second round recorded")
        .get("messages")
        .unwrap()
        .as_array()
        .unwrap();
    let tool_message = round2
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        .expect("tool result message in round 2");
    assert_eq!(
        tool_message.get("content").and_then(Value::as_str),
        Some("42")
    );

    let transcript = json!({ "requests": *transcript });
    compare_or_update("bash-tool", &transcript);
}

#[test]
fn sampling_and_thinking_are_plumbed_through_every_round() {
    // Same script as the tool loop; the transcript assertions already pin
    // `sampling` and `thinking` on both rounds (see tool-loop.json). This
    // test additionally checks the values directly for a clearer failure.
    let script = vec![
        ScriptedTurn {
            chunks: tool_call_chunks("call_1", "fake", r#"{"input": "x"}"#),
            finish_reason: FinishReason::ToolCalls,
            usage: None,
        },
        text_turn("done"),
    ];
    let (provider, requests) = ScriptedProvider::new(script);
    let mut messages = vec![ChatMessage::user("go")];
    run_turn(
        Box::new(provider),
        &fake_tools(),
        &mut messages,
        &config(),
        &mut |_| {},
    )
    .unwrap();
    for request in requests.borrow().iter() {
        assert_eq!(request["sampling"]["temperature"].as_f64(), Some(0.7));
        assert_eq!(request["thinking"]["budgetTokens"], 2048);
    }
}
