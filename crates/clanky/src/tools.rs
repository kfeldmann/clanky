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
    /// Default deadline for commands; `None` (the default) runs them
    /// unbounded. A voluntary `timeout_seconds` on the call is the only
    /// deadline.
    timeout: Option<Duration>,
    max_output_chars: usize,
}

impl Default for BashTool {
    fn default() -> Self {
        Self {
            timeout: None,
            max_output_chars: 20_000,
        }
    }
}

impl BashTool {
    /// A `bash` tool with an explicit default deadline (used by tests);
    /// [`BashTool::default`] is unbounded.
    pub fn new(timeout: Duration, max_output_chars: usize) -> Self {
        Self {
            timeout: Some(timeout),
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
                    "description": "Optional per-call timeout override in seconds (commands run unbounded without it)",
                    "minimum": 1
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
                        message: "expected a positive integer field `timeout_seconds`".into(),
                    });
                };
                // Any positive value is a legitimate voluntary timeout;
                // only zero is rejected.
                if seconds == 0 {
                    return Err(ToolError::InvalidArguments {
                        tool: self.name().into(),
                        message: "`timeout_seconds` must be at least 1".into(),
                    });
                }
                Some(Duration::from_secs(seconds))
            }
        };
        self.run_with_timeout(command, timeout)
    }
}

impl BashTool {
    fn run_with_timeout(
        &self,
        command: &str,
        timeout: Option<Duration>,
    ) -> Result<String, ToolError> {
        let mut shell = Command::new("bash");
        shell
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // A command with a deadline runs in its own process group so the
        // timeout can kill the whole tree: the direct `bash -c` child
        // only — never its grandchildren (`cargo test` and friends) —
        // would otherwise survive, hold the output pipes, and wedge the
        // reader joins below forever. Without a deadline there is nothing
        // for us to kill, and sharing Clanky's group keeps the terminal's
        // ctrl+c escape hatch reaching the command.
        #[cfg(unix)]
        if timeout.is_some() {
            use std::os::unix::process::CommandExt as _;
            shell.process_group(0);
        }
        let mut child = shell.spawn().map_err(|e| ToolError::Failed {
            tool: "bash".into(),
            message: format!("failed to spawn bash: {e}"),
        })?;
        let stdout_pipe = child.stdout.take().ok_or_else(|| pipe_error("stdout"))?;
        let stderr_pipe = child.stderr.take().ok_or_else(|| pipe_error("stderr"))?;

        // Both pipes are drained on threads: a large stderr would otherwise
        // deadlock the wait loop below.
        let stdout_reader = std::thread::spawn(move || read_to_bytes(stdout_pipe));
        let stderr_reader = std::thread::spawn(move || read_to_bytes(stderr_pipe));

        // `None` (or a deadline past `Instant`'s range) runs the command
        // unbounded: only a voluntary timeout ever kills it.
        let deadline = timeout.and_then(|timeout| Instant::now().checked_add(timeout));
        let status = loop {
            match child.wait_timeout(Duration::from_millis(100)) {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {
                    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        // Kill the whole process group (TERM, then KILL if
                        // the group lingers): grandchildren inherited the
                        // output pipes, and the reader joins below would
                        // block until every writer is gone. The direct
                        // child's group was set at spawn.
                        #[cfg(unix)]
                        {
                            let _ = kill_process_group(child.id(), "TERM");
                            // A short grace: processes that catch TERM get
                            // a chance to exit before the hard kill.
                            let _ = child.wait_timeout(Duration::from_millis(500));
                            let _ = kill_process_group(child.id(), "KILL");
                        }
                        let _ = child.kill();
                        let _ = child.wait();
                        break Err(format!(
                            "command exceeded its {}s timeout and was killed",
                            timeout.map_or(0, |timeout| timeout.as_secs())
                        ));
                    }
                }
                Err(e) => break Err(format!("waiting for bash failed: {e}")),
            }
        };

        // Killing the group closes the pipes, so the readers finish even
        // on timeout. The joins are still deadline-aware: a backgrounded
        // grandchild can outlive the direct child holding a pipe (the
        // direct child exited before the deadline, `kill -KILL` missed
        // because the child was not in our group, ...) — whatever
        // slipped through must not wedge the call past its deadline.
        let (stdout, stderr) =
            join_with_deadline([stdout_reader, stderr_reader], deadline, child.id());
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

/// How long the reader joins may wait past a missed deadline before
/// giving up on an escaped writer and returning what was captured.
const JOIN_GRACE: Duration = Duration::from_secs(2);

/// Join the output reader threads, but never past `deadline` (plus
/// [`JOIN_GRACE`]): a writer that slipped through every kill — typically
/// a backgrounded grandchild that outlived the direct child — would
/// otherwise wedge the tool call indefinitely. On grace expiry the
/// group is signalled once more (TERM, then KILL) and whatever was
/// captured is returned; the stuck reader threads stay detached and die
/// with the process.
fn join_with_deadline(
    readers: [std::thread::JoinHandle<Vec<u8>>; 2],
    deadline: Option<Instant>,
    pgid: u32,
) -> (Vec<u8>, Vec<u8>) {
    let mut outputs = (Vec::new(), Vec::new());
    let mut pending: Vec<(usize, std::thread::JoinHandle<Vec<u8>>)> =
        readers.into_iter().enumerate().collect();
    let Some(deadline) = deadline else {
        // Unbounded command: the pipes close when the child tree exits;
        // a plain join is the honest wait.
        pending.sort_by_key(|(index, _)| *index);
        for (index, reader) in pending.drain(..) {
            let bytes = reader.join().unwrap_or_default();
            if index == 0 {
                outputs.0 = bytes;
            } else {
                outputs.1 = bytes;
            }
        }
        return outputs;
    };
    let grace_end = deadline + JOIN_GRACE;
    while !pending.is_empty() {
        if Instant::now() >= grace_end {
            // Last resort: the group survived TERM+KILL (or the writer
            // daemonized into another group). Return what was captured;
            // the detached readers can no longer wedge anything.
            #[cfg(unix)]
            {
                let _ = kill_process_group(pgid, "TERM");
                std::thread::sleep(Duration::from_millis(100));
                let _ = kill_process_group(pgid, "KILL");
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
        for i in (0..pending.len()).rev() {
            if pending[i].1.is_finished() {
                let (index, reader) = pending.swap_remove(i);
                let bytes = reader.join().unwrap_or_default();
                if index == 0 {
                    outputs.0 = bytes;
                } else {
                    outputs.1 = bytes;
                }
            }
        }
    }
    outputs
}

/// Signal a command's whole process group by running `kill -<SIG> -PGID`
/// (no `libc` dependency; the same technique as `clanky-subagents`). The
/// group is a real group leader because it was spawned with
/// `process_group(0)`.
#[cfg(unix)]
fn kill_process_group(pgid: u32, signal: &str) -> std::io::Result<()> {
    let status = Command::new("kill")
        .args([format!("-{signal}"), format!("-{pgid}")])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "kill -{signal} -{pgid} exited with {status:?}"
        )))
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

/// Default number of output lines shown when previewing a tool result for
/// display (TUI transcript and pipe-mode stderr); matches the TUI's
/// `TOOL_PREVIEW_LINES`.
pub const RESULT_PREVIEW_LINES: usize = 8;

/// Shorten a tool result for display (TUI transcript, pipe-mode stderr):
/// a few lines verbatim plus an omission note. Newlines are kept — output
/// renders line by line, never collapsed with a symbol. The full result
/// is always what the model receives.
pub fn preview_result(output: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= max_lines {
        return output.to_string();
    }
    let shown = lines[..max_lines].join("\n");
    format!("{shown}\n… (+{} more lines)", lines.len() - max_lines)
}

/// Character budget for displaying tool-call arguments (transcript and
/// stderr); the full arguments always reach the model.
pub const CALL_PREVIEW_CHARS: usize = 600;

/// Shorten tool-call arguments for display (transcript and stderr).
/// A long `command` is cut with an omission note; `timeout_seconds`
/// (often the last field in the JSON, and thus the first thing a naive
/// cut of the raw string would drop) is re-attached after the note.
/// Anything that is not a bash-style arguments object falls back to a
/// plain character cut of the raw string.
pub fn truncate_call_arguments(arguments: &str, max_chars: usize) -> String {
    let value = match serde_json::from_str::<Value>(arguments) {
        Ok(value) => value,
        Err(_) => return cut_with_note(arguments, max_chars),
    };
    let Some(command) = value.get("command").and_then(Value::as_str) else {
        return cut_with_note(arguments, max_chars);
    };

    // Fits as-is: keep the original untouched so short calls render
    // exactly like before.
    if arguments.chars().count() <= max_chars {
        return arguments.to_string();
    }

    let prefix = r#"{"command":"#;
    let note = " … (truncated)";
    // The timeout is re-attached after the note so it survives the cut;
    // the closing brace is always restored.
    let suffix = match value.get("timeout_seconds").and_then(Value::as_u64) {
        Some(seconds) => format!(r#","timeout_seconds":{seconds}}}"#),
        None => "}".to_string(),
    };
    // Fields other than `command`/`timeout_seconds` are dropped; the
    // note covers that too.
    let has_other = value.as_object().is_some_and(|object| {
        object
            .keys()
            .any(|k| k != "command" && k != "timeout_seconds")
    });

    // How much of the command fits: everything except the JSON prefix,
    // the suffix, and (when the command itself overflows) the note.
    let mut budget = max_chars
        .saturating_sub(prefix.chars().count())
        .saturating_sub(suffix.chars().count());
    if command.chars().count() > budget {
        budget = budget.saturating_sub(note.chars().count());
    }
    let kept: String = command.chars().take(budget).collect();
    let cut = kept.chars().count() < command.chars().count() || has_other;
    if !cut {
        return arguments.to_string();
    }

    let mut out = format!(r#"{{"command":"{kept}""#);
    if cut {
        out.push_str(note);
    }
    out.push_str(&suffix);
    out
}

/// Cut raw text to `max_chars` characters with an omission note; used
/// when arguments are not a parseable bash-style object.
fn cut_with_note(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str("\n… (arguments truncated)");
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
    fn preview_result_keeps_newlines_and_notes_truncation() {
        assert_eq!(preview_result("one line", 8), "one line");
        let output = (1..=12)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let shown = preview_result(&output, RESULT_PREVIEW_LINES);
        assert_eq!(shown.lines().count(), RESULT_PREVIEW_LINES + 1);
        assert!(shown.contains("(+4 more lines)"));
        assert!(shown.starts_with("1\n2\n"));
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
    fn default_command_timeout_is_unbounded() {
        // No default deadline: a command runs until it finishes; only a
        // voluntary `timeout_seconds` can kill it.
        let output = run("sleep 1; echo done");
        assert_eq!(output, "done");
    }

    #[test]
    fn per_call_timeout_extends_or_shortens_deadline() {
        // A per-call override is honored on top of the unbounded default:
        // a short one kills a hanging sleep.
        let tool = BashTool::default();
        let ok = tool
            .execute(&json!({"command": "true", "timeout_seconds": 5}))
            .unwrap();
        assert_eq!(ok, "(no output)");

        let err = tool
            .execute(&json!({"command": "sleep 30", "timeout_seconds": 1}))
            .unwrap_err();
        assert!(err.to_string().contains("timeout"), "{err}");
    }

    #[test]
    fn per_call_timeout_is_validated() {
        let tool = BashTool::default();
        // Zero is the only rejected value: any positive second count is a
        // legitimate voluntary timeout, however large.
        let err = tool
            .execute(&json!({"command": "true", "timeout_seconds": 0}))
            .unwrap_err();
        assert!(err.to_string().contains("`timeout_seconds`"), "{err}");
        for ok in [1u64, 601, 86_400, u64::MAX] {
            let out = tool
                .execute(&json!({"command": "true", "timeout_seconds": ok}))
                .unwrap();
            assert_eq!(out, "(no output)");
        }
        for bad in [json!("ten"), json!(-5), json!(1.5)] {
            let err = tool
                .execute(&json!({"command": "true", "timeout_seconds": bad}))
                .unwrap_err();
            assert!(err.to_string().contains("integer"), "{err}");
        }
        // Absent and explicit null both fall back to the unbounded default.
        assert!(tool.execute(&json!({"command": "true"})).is_ok());
        assert!(
            tool.execute(&json!({"command": "true", "timeout_seconds": null}))
                .is_ok()
        );
    }

    #[test]
    fn times_out_hanging_commands() {
        let tool = BashTool::new(Duration::from_millis(300), 20_000);
        let err = tool.execute(&json!({"command": "sleep 30"})).unwrap_err();
        assert!(err.to_string().contains("timeout"), "{err}");
    }

    #[test]
    fn timeout_kills_grandchildren_holding_the_pipes() {
        // The wedge this regression test guards: the direct `bash -c`
        // child exits, but its grandchildren inherit the stdout pipe and
        // keep it open. Killing only the direct child used to leave them
        // alive, wedging the reader join forever (a real `cargo test |
        // tail` run hung for hours). The process-group kill must reap the
        // whole tree, so the call returns the timeout error promptly.
        let tool = BashTool::new(Duration::from_millis(500), 20_000);
        let started = Instant::now();
        // The direct child (`tail`) blocks on a pipe whose writer is a
        // grandchild; other grandchildren hold the stdout/stderr pipes.
        // All of them must die with the group when the deadline hits.
        let err = tool
            .execute(&json!({
                "command": "sleep 30 > /dev/null & sleep 30 1>&2 & sleep 30 | tail -1"
            }))
            .unwrap_err();
        assert!(err.to_string().contains("timeout"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timed out promptly, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn timeout_returns_even_when_a_daemon_escapes_the_group() {
        // Second wedge, same class: the direct child stays alive until
        // the deadline, but one grandchild daemonized out of the process
        // group (`setsid`) and holds the stdout pipe. The group kill
        // cannot reach it, so the reader join would block forever
        // without the deadline-aware join.
        let tool = BashTool::new(Duration::from_millis(500), 20_000);
        let started = Instant::now();
        let err = tool
            .execute(&json!({
                "command": "setsid sh -c 'sleep 300' & sleep 30"
            }))
            .unwrap_err();
        assert!(err.to_string().contains("timeout"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "returned promptly, took {:?}",
            started.elapsed()
        );
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
    fn short_call_arguments_pass_through_untouched() {
        let args = r#"{"command":"ls -la","timeout_seconds":30}"#;
        assert_eq!(truncate_call_arguments(args, 600), args);
    }

    #[test]
    fn long_command_is_cut_and_keeps_its_timeout() {
        let long = "x".repeat(2_000);
        let args = json!({"command": long, "timeout_seconds": 90}).to_string();
        let shown = truncate_call_arguments(&args, CALL_PREVIEW_CHARS);
        assert!(shown.contains("… (truncated)"), "{shown}");
        assert!(shown.contains(r#""timeout_seconds":90}"#), "{shown}");
        // Budget is honored: prefix + kept command + suffix, with the
        // note on top only when the command itself overflowed.
        assert!(shown.chars().count() <= CALL_PREVIEW_CHARS + 20, "{shown}");
        assert!(
            shown.chars().count() < args.chars().count(),
            "actually shorter than the input"
        );
    }

    #[test]
    fn command_only_arguments_regain_a_closing_brace() {
        let long = "y".repeat(2_000);
        let args = json!({"command": long}).to_string();
        let shown = truncate_call_arguments(&args, CALL_PREVIEW_CHARS);
        assert!(shown.contains("… (truncated)"), "{shown}");
        assert!(shown.ends_with('}'), "{shown}");
    }

    #[test]
    fn non_object_arguments_fall_back_to_a_plain_cut() {
        let args = "not json at all".repeat(100);
        let shown = truncate_call_arguments(&args, 40);
        assert!(shown.chars().count() <= 40 + 25, "{shown}");
        assert!(shown.contains("arguments truncated"), "{shown}");
    }

    #[test]
    fn unknown_fields_are_dropped_with_the_note() {
        let args = json!({"command": "echo hi", "extra": "payload".repeat(300)}).to_string();
        let shown = truncate_call_arguments(&args, CALL_PREVIEW_CHARS);
        assert!(shown.contains("echo hi"), "{shown}");
        assert!(!shown.contains("payload"), "{shown}");
        assert!(shown.contains("… (truncated)"), "{shown}");
        assert!(shown.ends_with('}'), "{shown}");
    }

    #[test]
    fn truncate_chars_notes_cut() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        let cut = truncate_chars("hello world", 5);
        assert!(cut.starts_with("hello"));
        assert!(cut.contains("truncated"));
    }
}
