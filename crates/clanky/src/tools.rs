//! Internal tool interface (M2) and the built-in `bash` tool.
//!
//! The agentic loop consumes tools behind the [`Tool`] trait: name, JSON
//! Schema parameters, and execution. MCP servers (M9) join the same trait as
//! additional implementations; the loop never knows the difference.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wait_timeout::ChildExt as _;

/// Why a tool execution failed. Failures are data, not crashes: they are fed
/// back to the model as the tool result so it can recover.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments for tool `{tool}`: {message}")]
    InvalidArguments { tool: String, message: String },
    #[error("tool `{tool}` failed: {message}")]
    Failed { tool: String, message: String },
}

/// A tool the agent can call.
pub trait Tool {
    /// Unique name the model uses to call this tool.
    fn name(&self) -> &str;

    /// One-line description shown to the model.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's arguments object.
    fn parameters(&self) -> Value;

    /// Execute the tool with parsed arguments.
    fn execute(&self, arguments: &Value) -> Result<String, ToolError>;

    /// The wire-format tool definition offered to the model.
    fn wire(&self) -> clanky_protocol::Tool {
        clanky_protocol::Tool {
            name: self.name().into(),
            description: Some(self.description().into()),
            parameters: Some(self.parameters()),
        }
    }
}

/// Tools available to the agentic loop.
pub type ToolSet = Vec<Box<dyn Tool>>;

/// The default tool set: `bash`.
pub fn default_tools() -> ToolSet {
    vec![Box::new(BashTool::default())]
}

/// The built-in `bash` tool: run a shell command, return stdout and stderr.
pub struct BashTool {
    timeout: Duration,
    max_output_chars: usize,
}

impl Default for BashTool {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(600),
            max_output_chars: 20_000,
        }
    }
}

impl BashTool {
    pub fn new(timeout: Duration, max_output_chars: usize) -> Self {
        Self {
            timeout,
            max_output_chars,
        }
    }
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a bash shell command in the project directory and return its stdout and stderr."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to run"
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "Optional per-call timeout override in seconds",
                    "minimum": 1,
                    "maximum": 600
                }
            },
            "required": ["command"]
        })
    }

    fn execute(&self, arguments: &Value) -> Result<String, ToolError> {
        let Some(command) = arguments.get("command").and_then(Value::as_str) else {
            return Err(ToolError::InvalidArguments {
                tool: self.name().into(),
                message: "expected a string field `command`".into(),
            });
        };
        let timeout = match arguments.get("timeout_seconds") {
            None | Some(Value::Null) => self.timeout,
            Some(value) => {
                let Some(seconds) = value.as_u64() else {
                    return Err(ToolError::InvalidArguments {
                        tool: self.name().into(),
                        message: "expected an integer field `timeout_seconds`".into(),
                    });
                };
                if seconds == 0 || seconds > self.timeout.as_secs() {
                    return Err(ToolError::InvalidArguments {
                        tool: self.name().into(),
                        message: format!(
                            "`timeout_seconds` must be between 1 and {}",
                            self.timeout.as_secs()
                        ),
                    });
                }
                Duration::from_secs(seconds)
            }
        };
        self.run_with_timeout(command, timeout)
    }
}

impl BashTool {
    fn run_with_timeout(&self, command: &str, timeout: Duration) -> Result<String, ToolError> {
        let mut child = Command::new("bash")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ToolError::Failed {
                tool: "bash".into(),
                message: format!("failed to spawn bash: {e}"),
            })?;
        let stdout_pipe = child.stdout.take().ok_or_else(|| pipe_error("stdout"))?;
        let stderr_pipe = child.stderr.take().ok_or_else(|| pipe_error("stderr"))?;

        // Both pipes are drained on threads: a large stderr would otherwise
        // deadlock the wait loop below.
        let stdout_reader = std::thread::spawn(move || read_to_bytes(stdout_pipe));
        let stderr_reader = std::thread::spawn(move || read_to_bytes(stderr_pipe));

        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.wait_timeout(Duration::from_millis(100)) {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break Err(format!(
                            "command exceeded its {}s timeout and was killed",
                            timeout.as_secs()
                        ));
                    }
                }
                Err(e) => break Err(format!("waiting for bash failed: {e}")),
            }
        };

        // Killing the child closes the pipes, so the readers finish even on
        // timeout (unless grandchildren inherited them).
        let stdout = stdout_reader.join().unwrap_or_default();
        let stderr = stderr_reader.join().unwrap_or_default();
        let status = status.map_err(|message| ToolError::Failed {
            tool: "bash".into(),
            message,
        })?;

        Ok(self.format(status, &stdout, &stderr))
    }

    /// Combine stdout, stderr, and the exit status into the tool result.
    fn format(&self, status: std::process::ExitStatus, stdout: &[u8], stderr: &[u8]) -> String {
        let stdout = String::from_utf8_lossy(stdout);
        let stderr = String::from_utf8_lossy(stderr);
        let mut output = String::new();
        if !stdout.trim_end_matches('\n').is_empty() {
            output.push_str(stdout.trim_end_matches('\n'));
        }
        if !stderr.trim_end_matches('\n').is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str("stderr: ");
            output.push_str(stderr.trim_end_matches('\n'));
        }
        if !status.success() {
            let code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into());
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&format!("[exit status: {code}]"));
        }
        if output.is_empty() {
            output.push_str("(no output)");
        }
        truncate_chars(&output, self.max_output_chars)
    }
}

fn pipe_error(stream: &str) -> ToolError {
    ToolError::Failed {
        tool: "bash".into(),
        message: format!("bash {stream} pipe was not captured"),
    }
}

fn read_to_bytes(mut pipe: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = pipe.read_to_end(&mut bytes);
    bytes
}

/// Cut `text` to `max_chars` characters, noting the truncation.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str("\n… (output truncated)");
    out
}

/// Whether a tool result reports a failure. Bash embeds an exit-status
/// marker on a non-zero status (see `BashTool::format`), and the agentic
/// loop prefixes `ERROR:` when the call itself fails (unknown tool,
/// malformed arguments, timeout, interruption).
pub fn is_failed_result(output: &str) -> bool {
    output.starts_with("ERROR:") || output.contains("[exit status: ")
}

/// Shorten a tool result for activity display (stderr); the full result is
/// always what the model receives.
pub fn truncate_for_display(text: &str, max_chars: usize) -> String {
    let single = text.replace('\n', " ⏎ ");
    if single.chars().count() <= max_chars {
        return single;
    }
    let mut out: String = single.chars().take(max_chars).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(command: &str) -> String {
        BashTool::default()
            .execute(&json!({"command": command}))
            .unwrap()
    }

    fn run_err(command: &str) -> ToolError {
        BashTool::default()
            .execute(&json!({"command": command}))
            .unwrap_err()
    }


    #[test]
    fn failure_detection_covers_exit_status_and_errors() {
        assert!(!is_failed_result("out\nstderr: warn"));
        assert!(!is_failed_result("(no output)"));
        assert!(is_failed_result("out\n[exit status: 1]"));
        assert!(is_failed_result("[exit status: 143]"));
        assert!(is_failed_result("ERROR: unknown tool: nope"));
    }

    #[test]
    fn bash_tool_metadata_matches_protocol_shape() {
        let tool = BashTool::default();
        assert_eq!(tool.name(), "bash");
        let wire = tool.wire();
        assert_eq!(wire.name, "bash");
        assert_eq!(wire.parameters.unwrap()["required"][0], "command");
        assert!(!wire.description.unwrap().is_empty());
    }

    #[test]
    fn runs_a_command_and_captures_output() {
        assert_eq!(run("echo hello"), "hello");
    }

    #[test]
    fn captures_stderr_and_exit_status() {
        let output = run("echo boom >&2; false");
        assert_eq!(output, "stderr: boom\n[exit status: 1]");
    }

    #[test]
    fn combines_stdout_stderr_and_status() {
        let output = run("echo out; echo err >&2; exit 3");
        assert_eq!(output, "out\nstderr: err\n[exit status: 3]");
    }

    #[test]
    fn empty_output_gets_a_marker() {
        assert_eq!(run("true"), "(no output)");
    }

    #[test]
    fn rejects_missing_or_non_string_command() {
        let err = BashTool::default().execute(&json!({})).unwrap_err();
        assert!(
            err.to_string()
                .contains("expected a string field `command`")
        );
        let err = BashTool::default()
            .execute(&json!({"command": 42}))
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("expected a string field `command`")
        );
        let _ = run_err; // keep helper referenced
    }

    #[test]
    fn per_call_timeout_extends_or_shortens_deadline() {
        // A per-call override longer than needed succeeds even though it
        // could exceed nothing here; a short one kills the same sleep.
        let tool = BashTool::default();
        let ok = tool
            .execute(&json!({"command": "true", "timeout_seconds": 5}))
            .unwrap();
        assert_eq!(ok, "(no output)");

        let short = BashTool::new(Duration::from_secs(600), 20_000);
        let err = short
            .execute(&json!({"command": "sleep 30", "timeout_seconds": 1}))
            .unwrap_err();
        assert!(err.to_string().contains("timeout"), "{err}");
    }

    #[test]
    fn per_call_timeout_is_validated() {
        let tool = BashTool::default();
        for bad in [0u64, 601, u64::MAX] {
            let err = tool
                .execute(&json!({"command": "true", "timeout_seconds": bad}))
                .unwrap_err();
            assert!(
                err.to_string().contains("`timeout_seconds`"),
                "{err}"
            );
        }
        let err = tool
            .execute(&json!({"command": "true", "timeout_seconds": "ten"}))
            .unwrap_err();
        assert!(err.to_string().contains("integer"), "{err}");
        // Absent and explicit null both fall back to the tool default.
        assert!(tool.execute(&json!({"command": "true"})).is_ok());
        assert!(tool
            .execute(&json!({"command": "true", "timeout_seconds": null}))
            .is_ok());
    }

    #[test]
    fn times_out_hanging_commands() {
        let tool = BashTool::new(Duration::from_millis(300), 20_000);
        let err = tool.execute(&json!({"command": "sleep 30"})).unwrap_err();
        assert!(err.to_string().contains("timeout"), "{err}");
    }

    #[test]
    fn truncates_long_output() {
        let tool = BashTool::new(Duration::from_secs(30), 10);
        let output = tool
            .execute(&json!({"command": "printf '%s' $(seq 1 500 | tr '\\n' ' ')"}))
            .unwrap();
        assert!(output.chars().count() < 40);
        assert!(output.ends_with("… (output truncated)"));
    }

    #[test]
    fn missing_binary_reports_spawn_failure() {
        // A command that does not exist: bash itself reports it (exit 127).
        let output = run("definitely-not-a-real-command-xyz");
        assert!(
            output.contains("command not found") || output.contains("[exit status: 127]"),
            "{output}"
        );
    }

    #[test]
    fn truncate_chars_notes_cut() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        let cut = truncate_chars("hello world", 5);
        assert!(cut.starts_with("hello"));
        assert!(cut.contains("truncated"));
    }
}
