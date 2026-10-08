//! M4 acceptance tests: session autosave + resume with full context.
//!
//! Drives the agentic loop with a scripted mock provider, saves the turn
//! events to a session file the way the TUI does (one record per event),
//! then "kills the session" (drops the writer), reloads the file, rebuilds
//! the history and continues the conversation — the rebuilt history must
//! be exactly what the model would have seen without the restart.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;

use clanky::session::{self, Record, SessionWriter};
use clanky::turn::{TurnConfig, TurnEvent, run_turn};
use clanky_protocol::{
    Capabilities, ChatDone, ChatMessage, ChatRequest, ChunkPayload, Error as ProtocolError,
    FinishReason, Handler, PluginInfo, Usage,
};
use serde_json::{Value, json};

/// A scripted provider: plays back recorded responses in order.
struct ScriptedProvider {
    requests: std::rc::Rc<RefCell<Vec<ChatRequest>>>,
    script: RefCell<VecDeque<ScriptedRound>>,
}

struct ScriptedRound {
    chunks: Vec<ChunkPayload>,
    finish_reason: FinishReason,
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

    fn list_models(&mut self) -> Result<Vec<clanky_protocol::ModelInfo>, ProtocolError> {
        Ok(Vec::new())
    }

    fn chat(
        &mut self,
        request: &ChatRequest,
        sink: &mut dyn FnMut(ChunkPayload),
    ) -> Result<ChatDone, ProtocolError> {
        self.requests.borrow_mut().push(request.clone());
        let round = self
            .script
            .borrow_mut()
            .pop_front()
            .expect("mock script exhausted");
        for chunk in round.chunks {
            sink(chunk);
        }
        Ok(ChatDone {
            finish_reason: round.finish_reason,
            usage: Some(Usage {
                prompt_tokens: Some(3),
                completion_tokens: Some(2),
                cached_tokens: None,
            }),
        })
    }
}

fn text_chunks(text: &str) -> Vec<ChunkPayload> {
    vec![ChunkPayload::Text { text: text.into() }]
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

fn text_round(text: &str) -> ScriptedRound {
    ScriptedRound {
        chunks: text_chunks(text),
        finish_reason: FinishReason::Stop,
    }
}

fn config() -> TurnConfig {
    TurnConfig {
        model: Some("mock/model".into()),
        sampling: None,
        thinking: None,
        max_tool_rounds: None,
        max_retries: Some(0),
        retry_sleep: Some(|_| {}),
    }
}

/// An echo tool mirroring the TUI's real tools.
struct EchoTool;

impl clanky::tools::Tool for EchoTool {
    fn name(&self) -> &str {
        "echo_tool"
    }

    fn description(&self) -> &str {
        "Echoes its input"
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"input": {"type": "string"}}})
    }

    fn execute(&self, arguments: &Value) -> Result<String, clanky::tools::ToolError> {
        Ok(format!(
            "echoed: {}",
            arguments["input"].as_str().unwrap_or("")
        ))
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("clanky-sessions-it-{name}"));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The session recorder, exactly as `tui/mod.rs` wires it: events from the
/// turn loop become session records.
struct Recorder {
    writer: SessionWriter,
}

impl Recorder {
    fn new(path: PathBuf) -> Self {
        Self {
            writer: SessionWriter::new(
                path,
                session::Header {
                    version: session::FORMAT_VERSION,
                    created: 0,
                    provider: Some("mock".into()),
                    model: Some("mock/model".into()),
                },
            ),
        }
    }

    fn on_turn_event(&mut self, event: &TurnEvent) {
        let record = match event {
            TurnEvent::Round {
                text,
                thinking,
                calls,
            } => {
                let mut records = Vec::new();
                if !thinking.is_empty() {
                    records.push(Record::Thinking {
                        text: thinking.clone(),
                    });
                }
                if !text.is_empty() || !calls.is_empty() {
                    records.push(Record::Assistant {
                        text: text.clone(),
                        calls: calls.clone(),
                    });
                }
                for record in records {
                    self.writer.append(&record).unwrap();
                }
                return;
            }
            TurnEvent::ToolResult { name, output } => Record::ToolResult {
                name: name.clone(),
                output: output.clone(),
            },
            _ => return,
        };
        self.writer.append(&record).unwrap();
    }

    fn on_done(&mut self, result: &clanky::error::Result<clanky::turn::TurnOutput>) {
        if let Ok(output) = result
            && let Some(usage) = output.usage
        {
            self.writer.append(&session::usage_record(usage)).unwrap();
        }
    }
}

#[test]
fn killed_session_resumes_with_full_context() {
    let dir = temp_dir("resume");
    let path = dir.join("session.jsonl");
    let mut recorder = Recorder::new(path.clone());

    // --- first turn: one tool call, then the answer ---
    let provider_requests = std::rc::Rc::new(RefCell::new(Vec::new()));
    let provider = ScriptedProvider {
        requests: std::rc::Rc::clone(&provider_requests),
        script: RefCell::new(VecDeque::from(vec![
            ScriptedRound {
                chunks: [
                    vec![ChunkPayload::Text {
                        text: "Checking.".into(),
                    }],
                    tool_call_chunks("call_1", "echo_tool", r#"{"input": "hello"}"#),
                ]
                .concat(),
                finish_reason: FinishReason::ToolCalls,
            },
            text_round("It said: echoed hello."),
        ])),
    };
    let tools: clanky::tools::ToolSet = vec![Box::new(EchoTool)];
    let mut messages = vec![
        ChatMessage::system("Be terse."),
        ChatMessage::user("run it"),
    ];

    // The TUI records the user prompt when it submits it.
    recorder
        .writer
        .append(&Record::User {
            text: "run it".into(),
        })
        .unwrap();

    let output1 = run_turn(
        Box::new(provider),
        &tools,
        &mut messages,
        &config(),
        &mut |event| recorder.on_turn_event(&event),
    )
    .unwrap();
    recorder.on_done(&Ok(output1.clone()));
    assert_eq!(output1.text, "Checking.It said: echoed hello.");

    // --- kill the session: drop everything, reload from disk ---
    drop(recorder);
    let data = session::load(&path).unwrap();
    let rebuilt_conversation = session::history_from_records(&data.records);

    // Rebuilt conversation == what the live turn produced (minus the
    // system message, which is re-assembled fresh on resume).
    let expected_conversation: Vec<ChatMessage> = messages[1..].to_vec();
    assert_eq!(rebuilt_conversation, expected_conversation);

    // --- continue the conversation on a fresh process ---
    let resumed_messages = vec![ChatMessage::system("Be terse.")]
        .into_iter()
        .chain(rebuilt_conversation)
        .collect::<Vec<_>>();
    assert_eq!(resumed_messages, messages, "full context restored");

    let provider2_requests = std::rc::Rc::new(RefCell::new(Vec::new()));
    let provider2 = ScriptedProvider {
        requests: std::rc::Rc::clone(&provider2_requests),
        script: RefCell::new(VecDeque::from(vec![text_round("Continuing.")])),
    };
    let mut messages2 = resumed_messages;
    messages2.push(ChatMessage::user("and again"));
    let output2 = run_turn(
        Box::new(provider2),
        &tools,
        &mut messages2,
        &config(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(output2.text, "Continuing.");

    // The provider saw the whole prior conversation, tool activity included.
    let requests = provider2_requests.borrow().clone();
    let sent = &requests[0].messages;
    assert_eq!(
        sent.len(),
        6,
        "system, user, assistant+call, tool, assistant, user"
    );
    assert_eq!(sent[3].content(), "echoed: hello");
    assert!(matches!(
        &sent[2],
        ChatMessage::Assistant { tool_calls: Some(calls), .. } if calls[0].id == "call_1"
    ));

    // --- the resumed session appends to the same file ---
    let mut resumed_writer = SessionWriter::reopen(path.clone());
    resumed_writer
        .append(&Record::User {
            text: "and again".into(),
        })
        .unwrap();
    let data = session::load(&path).unwrap();
    assert_eq!(data.records.last().and_then(r_user_text), Some("and again"));
    std::fs::remove_dir_all(dir).ok();
}

fn r_user_text(record: &Record) -> Option<&str> {
    match record {
        Record::User { text } => Some(text),
        _ => None,
    }
}

/// `/md` acceptance: the document exported from a session file is the
/// same conversation a resume would rebuild, and the flags gate exactly
/// the record types they name.
#[test]
fn markdown_export_covers_the_session_and_honors_its_flags() {
    use clanky::export::{self, ExportOptions};

    let dir = std::env::temp_dir().join(format!(
        "clanky-export-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("conversation.jsonl");

    let records = vec![
        Record::User {
            text: "count files".into(),
        },
        Record::Thinking {
            text: "run ls first".into(),
        },
        Record::Assistant {
            text: "Let me check.".into(),
            calls: vec![clanky_protocol::ToolCall {
                id: "call_1".into(),
                name: "bash".into(),
                arguments: json!({"command": "ls | wc -l"}),
            }],
        },
        Record::ToolResult {
            name: "bash".into(),
            output: "42".into(),
        },
        Record::Assistant {
            text: "42 files.".into(),
            calls: vec![],
        },
        Record::Usage {
            prompt_tokens: Some(120),
            completion_tokens: Some(30),
        },
    ];
    let mut writer = SessionWriter::new(
        path.clone(),
        session::Header {
            version: session::FORMAT_VERSION,
            created: 1_700_000_000_000,
            provider: Some("deepinfra".into()),
            model: Some("mock/model".into()),
        },
    );
    for record in &records {
        writer.append(record).unwrap();
    }
    drop(writer);

    let data = session::load(&path).unwrap();

    // Default: user + assistant text only.
    let default = export::render(None, &data.header, &data.records, ExportOptions::default());
    assert!(default.contains("## User\n\ncount files"), "{default}");
    assert!(
        default.contains("## Assistant\n\nLet me check."),
        "{default}"
    );
    assert!(default.contains("42 files."), "{default}");
    assert!(!default.contains("run ls first"), "{default}");
    assert!(!default.contains("Tool call"), "{default}");
    assert!(!default.contains("\"command\""), "{default}");

    // --all: everything.
    let all = export::render(None, &data.header, &data.records, ExportOptions::ALL);
    assert!(all.contains("## Thinking"), "{all}");
    assert!(all.contains("run ls first"), "{all}");
    assert!(all.contains("### Tool call: `bash`"), "{all}");
    assert!(all.contains("\"command\": \"ls | wc -l\""), "{all}");
    assert!(all.contains("### Tool result: `bash`"), "{all}");
    assert!(all.contains("42"), "{all}");

    // The exported conversation matches what resume rebuilds.
    let history = session::history_from_records(&data.records);
    assert_eq!(history.len(), 4, "user, assistant+call, tool, assistant");
    assert_eq!(history[0].content(), "count files");
    assert_eq!(history[3].content(), "42 files.");

    std::fs::remove_dir_all(dir).ok();
}
