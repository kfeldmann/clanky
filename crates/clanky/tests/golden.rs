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
#[derive(Clone)]
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
            default_model: None,
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
    clanky::tools::default_tools(None)
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
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
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

/// Tool-call and tool-result previews pinned as text so the visual
/// contract of the transcript (how a call and its result render at a
/// given width, including the post-wrap line cap) is caught on change.
/// `Line`/`Span` are not `Serialize`, so the rows are compared as plain
/// strings.
#[test]
fn tool_preview_rendering_matches_golden() {
    // The fake tool with a long input: the cut call preview must keep
    // its shape, and the wide result exercises the cap.
    let script = vec![
        ScriptedTurn {
            chunks: tool_call_chunks(
                "call_1",
                "fake",
                r#"{"input": "aaaa bbbb cccc dddd eeee ffff gggg hhhh iiii jjjj kkkk llll mmmm nnnn oooo pppp qqqq rrrr ssss tttt"}"#,
            ),
            finish_reason: FinishReason::ToolCalls,
            usage: Some(Usage {
                prompt_tokens: Some(12),
                completion_tokens: Some(9),
                cached_tokens: None,
            }),
        },
        // A second call whose echoed result wraps into many rows at
        // transcript width, exercising the post-wrap cap.
        ScriptedTurn {
            chunks: tool_call_chunks(
                "call_2",
                "fake",
                &format!(r#"{{"input": "{}"}}"#, "word ".repeat(400)),
            ),
            finish_reason: FinishReason::ToolCalls,
            usage: Some(Usage {
                prompt_tokens: Some(30),
                completion_tokens: Some(9),
                cached_tokens: None,
            }),
        },
        text_turn("done"),
    ];
    let (provider, _requests) = ScriptedProvider::new(script);
    let mut messages = vec![ChatMessage::user("long input")];
    let events = Rc::new(RefCell::new(Vec::new()));
    let sink = events.clone();
    run_turn(
        Box::new(provider),
        &fake_tools(),
        &mut messages,
        &config(),
        &mut |event| sink.borrow_mut().push(event),
    )
    .unwrap();

    let mut app = clanky::tui::App::new(None);
    app.push_user("long input");
    for event in events.borrow().iter() {
        app.on_turn_event(event.clone());
    }

    // Each entry's rows at transcript width, as plain strings.
    let rows: Vec<String> = (0..app.entries.len())
        .flat_map(|i| app.entry_lines(i, 80))
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.clone())
                .collect::<String>()
        })
        .collect();
    let transcript = json!({ "rows": rows });
    compare_or_update("tool-preview", &transcript);
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

/// The edit tool in the agentic loop (M19): a scripted model edits a real
/// scratch file through one round, then reads it back and answers. The
/// golden pins the `edit` tool definition as offered on the wire and the
/// exact fed-back result text ("Replaced N occurrence(s) in <path>") in
/// the second round's messages — any change to either breaks the golden.
///
/// Regenerate with: `CLANKY_UPDATE_GOLDEN=1 cargo test --test golden`
#[test]
fn edit_tool_transcript_matches_golden() {
    // A deterministic scratch file, like the bash golden's `echo 42`.
    // The path is relative to the test's working directory (the crate
    // root) so the golden stays machine-independent; `target/tmp` is
    // workspace scratch and never committed.
    let path = "target/tmp/edit-golden/hello.txt".to_string();
    std::fs::create_dir_all(
        std::path::Path::new(&path)
            .parent()
            .expect("scratch parent"),
    )
    .unwrap();
    std::fs::write(&path, "Hello, world!\n").unwrap();
    let arguments = json!({
        "path": path,
        "old_string": "world",
        "new_string": "clanky"
    })
    .to_string();

    let script = vec![
        // Round 1: the model edits the file.
        ScriptedTurn {
            chunks: tool_call_chunks("call_1", "edit", &arguments),
            finish_reason: FinishReason::ToolCalls,
            usage: Some(Usage {
                prompt_tokens: Some(20),
                completion_tokens: Some(8),
                cached_tokens: None,
            }),
        },
        // Round 2: it reads the file back with bash and answers.
        ScriptedTurn {
            chunks: [tool_call_chunks(
                "call_2",
                "bash",
                &json!({"command": format!("cat {path}")}).to_string(),
            )]
            .concat(),
            finish_reason: FinishReason::ToolCalls,
            usage: Some(Usage {
                prompt_tokens: Some(45),
                completion_tokens: Some(8),
                cached_tokens: None,
            }),
        },
        text_turn("The file now says: Hello, clanky!"),
    ];
    let (provider, requests) = ScriptedProvider::new(script);
    let mut messages = vec![ChatMessage::user(
        "change \"world\" to \"clanky\" in hello.txt",
    )];
    let output = run_turn(
        Box::new(provider),
        &clanky::tools::default_tools(None),
        &mut messages,
        &config(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(output.text, "The file now says: Hello, clanky!");

    // The edit actually happened on disk.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "Hello, clanky!\n");

    // The edit tool definition offered on the wire is pinned (name,
    // description, schema) so a schema change cannot pass unnoticed.
    let transcript = requests.borrow();
    let tools = transcript[0].get("tools").unwrap().as_array().unwrap();
    let edit_tool = tools
        .iter()
        .find(|t| t.get("name").and_then(Value::as_str) == Some("edit"))
        .expect("edit tool offered to the model");
    let expected_wire = json!({
        "name": "edit",
        "description": clanky::tools::EditTool.description().to_string(),
        "parameters": clanky::tools::EditTool.parameters(),
    });
    assert_eq!(*edit_tool, expected_wire);

    // The fed-back tool result must be exactly the confirmation text.
    let round2 = transcript[1].get("messages").unwrap().as_array().unwrap();
    let tool_message = round2
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        .expect("edit result message in round 2");
    assert_eq!(
        tool_message.get("content").and_then(Value::as_str),
        Some(format!("Replaced 1 occurrence(s) in {path}").as_str())
    );

    let transcript = json!({
        "requests": *transcript,
        "file": std::fs::read_to_string(&path).unwrap(),
    });
    compare_or_update("edit-tool", &transcript);
    std::fs::remove_file(&path).ok();
}

/// A `-p` run and a TUI run over the same transcript must leave the same
/// session records (M14): print mode feeds its turn events through the same
/// shared recorder the TUI uses, records the prompt the same way, and closes
/// the turn with the same `record_done`. This drives both front ends'
/// recording wirings over one event stream (the tool-loop transcript, so
/// thinking, tool calls, results, and usage all occur) and pins the joint
/// records as a golden: any change to either front end's wiring or to the
/// recorder's event mapping that makes one diverge from the other fails
/// here. `created` is stamped at write time in production, so the records
/// (not the header) are what the golden pins.
///
/// Regenerate with: `CLANKY_UPDATE_GOLDEN=1 cargo test --test golden`
#[test]
fn pipe_and_tui_session_records_match_golden() {
    use clanky::recorder::{self, Recorder};
    use clanky::session::Record;

    let script = vec![
        // Round 1: streamed thinking, then a tool call.
        ScriptedTurn {
            chunks: [
                vec![ChunkPayload::Thinking {
                    text: "Running the tool.".into(),
                }],
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
        // Round 2: the final answer.
        text_turn("The tool said: fake ran with input=hello"),
    ];

    // Print mode's recording wiring (main::run_pipe): a fresh recorder,
    // the user prompt recorded before the turn, every turn event through
    // `record_turn_event`, then `record_done` for the terminal state.
    let pipe_dir = temp_session_dir("pipe");
    let mut pipe_recorder = Recorder::with_dir(
        pipe_dir.clone(),
        "session",
        clanky::settings::SessionSettings {
            provider: Some("deepinfra".into()),
            model: Some("mock/model".into()),
            ..Default::default()
        },
    );
    pipe_recorder.record(
        &mut NoSink,
        Record::User {
            text: "use the tool on hello".into(),
        },
    );
    let (provider, _requests) = ScriptedProvider::new(script.clone());
    let mut pipe_messages = vec![ChatMessage::user("use the tool on hello")];
    let pipe_output = run_turn(
        Box::new(provider),
        &fake_tools(),
        &mut pipe_messages,
        &config(),
        &mut |event| {
            recorder::record_turn_event(&mut pipe_recorder, &mut NoSink, &event);
        },
    )
    .unwrap();
    pipe_recorder.record_done(&mut NoSink, &Ok(pipe_output.clone()));

    // The TUI's recording wiring (event_loop): the same recorder type fed
    // the same sequence — prompt record, per-event records, `record_done`.
    let tui_dir = temp_session_dir("tui");
    let mut tui_recorder = Recorder::with_dir(
        tui_dir.clone(),
        "session",
        clanky::settings::SessionSettings {
            provider: Some("deepinfra".into()),
            model: Some("mock/model".into()),
            ..Default::default()
        },
    );
    tui_recorder.record(
        &mut NoSink,
        Record::User {
            text: "use the tool on hello".into(),
        },
    );
    let (provider, _requests) = ScriptedProvider::new(script);
    let mut tui_messages = vec![ChatMessage::user("use the tool on hello")];
    let tui_output = run_turn(
        Box::new(provider),
        &fake_tools(),
        &mut tui_messages,
        &config(),
        &mut |event| {
            recorder::record_turn_event(&mut tui_recorder, &mut NoSink, &event);
        },
    )
    .unwrap();
    tui_recorder.record_done(&mut NoSink, &Ok(tui_output));

    // Both files hold the same records.
    let pipe_records = written_records(&mut pipe_recorder);
    let tui_records = written_records(&mut tui_recorder);
    assert_eq!(pipe_records, tui_records, "front ends must agree");

    let transcript = json!({
        "records": pipe_records,
        "output": {
            "text": pipe_output.text,
            "finishReason": pipe_output.finish_reason,
            "usage": pipe_output.usage,
        },
    });
    compare_or_update("session-records", &transcript);
    std::fs::remove_dir_all(pipe_dir).ok();
    std::fs::remove_dir_all(tui_dir).ok();
}

/// The records the recorder has on disk so far (its session file exists
/// after the first record).
fn written_records(recorder: &mut clanky::recorder::Recorder) -> Vec<Value> {
    let file = recorder
        .open_file()
        .expect("recording recorder opened a session file")
        .to_path_buf();
    let data = clanky::session::load(&file).unwrap();
    // The header is write-once now (M18): the provider/model live in the
    // leading settings record.
    assert_eq!(data.header.provider.as_deref(), None);
    assert_eq!(
        data.records.first(),
        Some(&clanky::session::Record::Settings {
            settings: clanky::session::SessionSettingsRecord {
                provider: Some("deepinfra".into()),
                model: Some("mock/model".into()),
                ..Default::default()
            }
        })
    );
    data.records
        .iter()
        .map(|record| serde_json::to_value(record).unwrap())
        .collect()
}

/// Recorder sink for tests that expect no session-write failures.
struct NoSink;
impl clanky::recorder::ErrorSink for NoSink {
    fn report(&mut self, message: String) {
        panic!("unexpected session failure: {message}");
    }
}

fn temp_session_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "clanky-golden-session-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
