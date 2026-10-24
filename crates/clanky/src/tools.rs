//! Internal tool interface (M2) and the built-in `bash` tool.
//!
//! The agentic loop consumes tools behind the [`Tool`] trait: name, JSON
//! Schema parameters, and execution. MCP servers (M9) join the same trait as
//! additional implementations; the loop never knows the difference.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};

use clanky_protocol::MediaType;
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

/// What a tool execution produced: the text result fed back to the model
/// as the `tool` message's content, plus any inline images that ride
/// beside it (a `view-image` attachment; empty for every other tool).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub text: String,
    pub images: Vec<clanky_protocol::ImagePart>,
}

impl ToolOutput {
    /// A plain text result (no images) — what most tools return.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self::text(text)
    }
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
    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError>;

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

/// The default tool set: `bash`, `edit`, and `view-image`.
/// `timeout_seconds` is the configured default tool-call deadline (huge
/// default when unset, see below); a per-call `timeout_seconds` argument
/// always wins over it (it applies to `bash` only — the other tools are
/// plain buffered IO that never hangs).
pub fn default_tools(timeout_seconds: Option<u64>) -> ToolSet {
    vec![
        Box::new(BashTool::new(
            Duration::from_secs(timeout_seconds.unwrap_or(UNSET_DEADLINE)),
            20_000,
        )),
        Box::new(EditTool),
        Box::new(ViewImageTool),
    ]
}

/// Sentinel default deadline used when no configured deadline is set: far
/// beyond any command, and effectively unbounded (the config value is what
/// the user sees; what the tool sees is a deadline it will never hit, so
/// the timeout machinery can run unconditionally).
const UNSET_DEADLINE: u64 = u64::MAX / 2;

/// The built-in `bash` tool: run a shell command, return stdout and stderr.
pub struct BashTool {
    /// Default deadline for commands; huge (unset) runs them unbounded.
    /// A voluntary `timeout_seconds` on the call is the only other source
    /// of a deadline.
    timeout: Duration,
    max_output_chars: usize,
}

impl Default for BashTool {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(UNSET_DEADLINE),
            max_output_chars: 20_000,
        }
    }
}

impl BashTool {
    /// A `bash` tool with an explicit default deadline (used by tests);
    /// [`BashTool::default`] is unbounded. A deadline past `Instant`'s
    /// range (`UNSET_DEADLINE`) never fires, so an unset configured
    /// deadline is simply passed through and behaves as no deadline.
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
                    "description": "Optional per-call timeout override in seconds (wins over the configured default; without it the configured default applies, unbounded when none is set)",
                    "minimum": 1
                }
            },
            "required": ["command"]
        })
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let Some(command) = arguments.get("command").and_then(Value::as_str) else {
            return Err(ToolError::InvalidArguments {
                tool: self.name().into(),
                message: "expected a string field `command`".into(),
            });
        };
        let timeout = match arguments.get("timeout_seconds") {
            None | Some(Value::Null) => Some(self.timeout),
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
        self.run_with_timeout(command, timeout.unwrap_or(self.timeout))
    }
}

impl BashTool {
    fn run_with_timeout(&self, command: &str, timeout: Duration) -> Result<ToolOutput, ToolError> {
        let mut shell = Command::new("bash");
        shell
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Every command runs in its own process group so the timeout (when
        // one exists, configured or per-call) can kill the whole tree: the
        // direct `bash -c` child only — never its grandchildren (`cargo
        // test` and friends) — would otherwise survive, hold the output
        // pipes, and wedge the reader joins below forever. With no deadline
        // the group is never killed and is harmless; ctrl+c still reaches
        // the command because SIGINT from the interactive terminal goes to
        // the foreground group, which an unbounded child joins only via its
        // own group being ignored by the terminal — Clanky propagates it by
        // exiting and the plugin protocol's cancellation (spec §8) tears the
        // call down.
        #[cfg(unix)]
        {
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

        // A deadline past `Instant`'s range (the unset sentinel) runs the
        // command unbounded: only a voluntary timeout ever kills it.
        let deadline = Instant::now().checked_add(timeout);
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
                            timeout.as_secs()
                        ));
                    }
                }
                Err(e) => break Err(format!("waiting for bash failed: {e}")),
            }
        };

        // Killing the group closes the pipes, so the readers finish even
        // on timeout. The joins still carry their own grace: a backgrounded
        // grandchild can outlive the direct child holding a pipe (the
        // direct child exited, `kill -KILL` missed because the child was
        // not in our group, ...) — whatever slipped through must not
        // wedge the call past its deadline.
        // That hold-up is not a timeout phenomenon, though: with *no*
        // deadline (`sleep 5 & echo ok` is legal and unbounded) the same
        // backgrounded writer would hang a plain `join` forever, so the
        // group-kill fallback runs on the reader loop, not the wait loop.
        let (stdout, stderr) = join_with_deadline([stdout_reader, stderr_reader], child.id());
        let status = status.map_err(|message| ToolError::Failed {
            tool: "bash".into(),
            message,
        })?;

        Ok(ToolOutput::text(self.format(status, &stdout, &stderr)))
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

/// The built-in `edit` tool: replace a literal substring in a file.
///
/// The model supplies `path`, `old_string`, `new_string`, and an optional
/// `replace_all` flag. Matching is byte-for-byte literal — no patterns, no
/// wildcards — so the text in the tool call is exactly the text in the file.
///
/// There is no read-before-edit requirement and no permission boundary: bash
/// already writes anywhere. The tool's value is intent and failure messages —
/// the match-count errors below are data the model can recover from, instead
/// of a silent `sed` that hit the wrong line.
pub struct EditTool;

impl Default for EditTool {
    fn default() -> Self {
        Self
    }
}

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Replace a literal substring in a file (byte-for-byte, no patterns). Fails on zero matches; fails on multiple matches unless `replace_all` is set."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The file to edit (relative paths resolve against the working directory)"
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact text to replace (must match the file byte-for-byte)"
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement text"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence instead of failing on multiple matches (default: false)"
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        // Argument extraction: every field the schema requires must be a
        // string, and each missing/mistyped field gets its own message so
        // the model can fix just that field.
        let string_arg = |field: &str| -> Result<&str, ToolError> {
            arguments.get(field).and_then(Value::as_str).ok_or_else(|| {
                ToolError::InvalidArguments {
                    tool: self.name().into(),
                    message: format!("expected a string field `{field}`"),
                }
            })
        };
        let path = string_arg("path")?;
        let old_string = string_arg("old_string")?;
        let new_string = string_arg("new_string")?;
        let replace_all = match arguments.get("replace_all") {
            None | Some(Value::Null) => false,
            Some(value) => value.as_bool().ok_or_else(|| ToolError::InvalidArguments {
                tool: self.name().into(),
                message: "expected a boolean field `replace_all`".into(),
            })?,
        };
        if old_string.is_empty() {
            return Err(ToolError::InvalidArguments {
                tool: self.name().into(),
                message:
                    "`old_string` must not be empty (an empty match would replace the whole file)"
                        .into(),
            });
        }

        // Read the file as bytes and match on bytes: the needle and
        // haystack are matched byte-for-byte, and a file with invalid
        // UTF-8 beside the edited region must not block an edit to valid
        // text in it.
        let bytes = std::fs::read(path).map_err(|err| ToolError::Failed {
            tool: self.name().into(),
            message: format!("failed to read {path}: {err}"),
        })?;
        let count = count_subsequence(&bytes, old_string.as_bytes());

        // Failures are data, not turn errors: each error names exactly what
        // went wrong so the model can re-read the file and retry.
        if count == 0 {
            return Err(ToolError::Failed {
                tool: self.name().into(),
                message: format!(
                    "not found: `old_string` occurs 0 times in {path} (check the exact text, including whitespace)"
                ),
            });
        }
        if count > 1 && !replace_all {
            return Err(ToolError::Failed {
                tool: self.name().into(),
                message: format!(
                    "`old_string` occurs {count} times in {path}; provide more surrounding context to make it unique, or pass `replace_all: true`"
                ),
            });
        }

        let (replaced, replacements) =
            replace_subsequence(&bytes, old_string.as_bytes(), new_string.as_bytes());
        debug_assert_eq!(
            replacements, count,
            "replacement count must match the match count"
        );
        std::fs::write(path, &replaced).map_err(|err| ToolError::Failed {
            tool: self.name().into(),
            message: format!("failed to write {path}: {err}"),
        })?;

        // A short confirmation: the count is how the model verifies it did
        // what it intended. No file contents, no diff — the tool call in
        // context already carries `old_string`/`new_string`.
        Ok(ToolOutput::text(format!(
            "Replaced {count} occurrence(s) in {path}"
        )))
    }
}

/// The built-in `view-image` tool: read an image file from disk and put
/// it into the model's context, for multimodal models (M20).
///
/// The tool result's *text* is a small size note; the image itself rides
/// beside the text in the result's images list, which the turn loop
/// places on the `tool` message and the provider adapters translate into
/// their backend's multimodal shape. A text-only model 400s when the
/// image reaches it — the backend's error is surfaced unchanged (project
/// invariant: no local capability whitelist).
pub struct ViewImageTool;

impl Default for ViewImageTool {
    fn default() -> Self {
        Self
    }
}

impl Tool for ViewImageTool {
    fn name(&self) -> &str {
        "view-image"
    }

    fn description(&self) -> &str {
        "Read an image file from disk and attach it so you can see it."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The image file to view (relative paths resolve against the working directory)"
                }
            },
            "required": ["path"]
        })
    }

    fn execute(&self, arguments: &Value) -> Result<ToolOutput, ToolError> {
        let Some(path) = arguments.get("path").and_then(Value::as_str) else {
            return Err(ToolError::InvalidArguments {
                tool: self.name().into(),
                message: "expected a string field `path`".into(),
            });
        };
        let bytes = std::fs::read(path).map_err(|err| ToolError::Failed {
            tool: self.name().into(),
            message: format!("failed to read {path}: {err}"),
        })?;
        let media_type = media_type_for(path, &bytes).map_err(|message| ToolError::Failed {
            tool: self.name().into(),
            message,
        })?;
        let data_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(ToolOutput {
            text: size_note(bytes.len()),
            images: vec![clanky_protocol::ImagePart {
                data_base64,
                media_type,
            }],
        })
    }
}

/// Human-readable size note, in SI units: `623 bytes` under a kilobyte,
/// `256 kb` below a megabyte, `1.2 mb` above. Matches the milestone's
/// `256 kb` example.
fn size_note(bytes: usize) -> String {
    if bytes < 1000 {
        format!("{bytes} bytes")
    } else if bytes < 1_000_000 {
        format!("{} kb", bytes / 1000)
    } else {
        format!("{:.1} mb", bytes as f64 / 1_000_000.0)
    }
}

/// Resolve an image's media type: the file's magic bytes when the format
/// carries one (JPEG, PNG, GIF, WEBP — the formats in practice), the
/// filename extension otherwise (the container formats: HEIC/HEIF/AVIF
/// and any misnamed file). An unknown extension is the error the model
/// can recover from.
fn media_type_for(path: &str, bytes: &[u8]) -> std::result::Result<MediaType, String> {
    if let Some(detected) = MediaType::sniff(bytes) {
        return Ok(detected);
    }
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    MediaType::canonical_lookup(extension).ok_or_else(|| {
        "unknown image media type: the file has no recognizable image signature and its extension is not a known image format".to_string()
    })
}

/// Count non-overlapping occurrences of `needle` in `haystack`.
fn count_subsequence(haystack: &[u8], needle: &[u8]) -> usize {
    replace_subsequence(haystack, needle, needle).1
}

/// Replace every occurrence of `needle` with `replacement`, returning the
/// new bytes and the number of replacements.
fn replace_subsequence(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> (Vec<u8>, usize) {
    let mut out = Vec::with_capacity(haystack.len());
    let mut count = 0;
    let mut index = 0;
    while index + needle.len() <= haystack.len() {
        if &haystack[index..index + needle.len()] == needle {
            out.extend_from_slice(replacement);
            index += needle.len();
            count += 1;
        } else {
            out.push(haystack[index]);
            index += 1;
        }
    }
    out.extend_from_slice(&haystack[index..]);
    (out, count)
}

/// How long the reader joins may wait past a missed deadline before
/// giving up on an escaped writer and returning what was captured.
const JOIN_GRACE: Duration = Duration::from_secs(2);

/// After the final group kill, how long the readers may take to observe
/// EOF and hand over what they captured before the call returns.
const KILL_FLUSH: Duration = Duration::from_millis(1_000);

/// Join the output reader threads, but never past [`JOIN_GRACE`]: a writer
/// that outlived the direct child — typically a backgrounded grandchild
/// (`sleep 5 & echo ok`) holding one of the pipes — would otherwise wedge
/// the tool call indefinitely, *deadline or no deadline*: with no deadline
/// configured this used to be a plain `join` and hung forever. On grace
/// expiry the group is signalled once more (TERM, then KILL) and whatever
/// was captured is returned; the stuck reader threads stay detached and
/// die with the process.
fn join_with_deadline(
    readers: [std::thread::JoinHandle<Vec<u8>>; 2],
    pgid: u32,
) -> (Vec<u8>, Vec<u8>) {
    let mut outputs = (Vec::new(), Vec::new());
    let mut pending: Vec<(usize, std::thread::JoinHandle<Vec<u8>>)> =
        readers.into_iter().enumerate().collect();
    let grace_end = Instant::now() + JOIN_GRACE;
    while !pending.is_empty() {
        if Instant::now() >= grace_end {
            // Last resort: the group survived TERM+KILL (or the writer
            // daemonized into another group). Kill it once more, then give
            // the readers a moment to observe EOF and finish: the kill
            // closes the pipes, so the captured output lands in the very
            // last polling pass below instead of being discarded.
            #[cfg(unix)]
            {
                let _ = kill_process_group(pgid, "TERM");
                std::thread::sleep(Duration::from_millis(100));
                let _ = kill_process_group(pgid, "KILL");
            }
            let flush = Instant::now() + KILL_FLUSH;
            while Instant::now() < flush {
                collect_finished(&mut pending, &mut outputs);
                if pending.is_empty() {
                    return outputs;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
        collect_finished(&mut pending, &mut outputs);
    }
    outputs
}

/// Move any finished reader's bytes into `outputs`.
fn collect_finished(
    pending: &mut Vec<(usize, std::thread::JoinHandle<Vec<u8>>)>,
    outputs: &mut (Vec<u8>, Vec<u8>),
) {
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
/// marker as the *final line* on a non-zero status (see `BashTool::format`),
/// and the agentic loop prefixes `ERROR:` when the call itself fails
/// (unknown tool, malformed arguments, timeout, interruption). The marker
/// is matched only as an exact final line so a successful tool whose
/// output merely *mentions* it (a `cat` of a session transcript, a review
/// quoting the marker) is not misclassified as failed.
pub fn is_failed_result(output: &str) -> bool {
    output.starts_with("ERROR:")
        || output
            .lines()
            .next_back()
            .is_some_and(|line| line.starts_with("[exit status: ") && line.ends_with(']'))
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
            .text
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
    fn failure_detection_ignores_a_mentioned_marker() {
        // A successful tool whose output merely echoes the marker (a `cat`
        // of a transcript, a review quoting it) must not read as failed:
        // only the exact final line counts.
        assert!(!is_failed_result(
            "[exit status: 1] was the last error\nall good"
        ));
        assert!(!is_failed_result(
            "review: the result ended in [exit status: 2]"
        ));
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
    fn configured_default_timeout_applies_and_per_call_wins() {
        // A configured default deadline kills a command that does not pass
        // its own `timeout_seconds`.
        let tool = BashTool::new(Duration::from_secs(1), 20_000);
        let err = tool.execute(&json!({"command": "sleep 30"})).unwrap_err();
        assert!(err.to_string().contains("1s timeout"), "{err}");

        // The per-call value wins over the configured default, in both
        // directions.
        let out = tool
            .execute(&json!({"command": "sleep 2; echo done", "timeout_seconds": 10}))
            .unwrap();
        assert_eq!(out.text, "done");
        let err = tool
            .execute(&json!({"command": "sleep 30", "timeout_seconds": 1}))
            .unwrap_err();
        assert!(err.to_string().contains("timeout"), "{err}");
    }

    #[test]
    fn per_call_timeout_extends_or_shortens_deadline() {
        // A per-call override is honored on top of the unbounded default:
        // a short one kills a hanging sleep.
        let tool = BashTool::default();
        let ok = tool
            .execute(&json!({"command": "true", "timeout_seconds": 5}))
            .unwrap();
        assert_eq!(ok.text, "(no output)");

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
            assert_eq!(out.text, "(no output)");
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
    fn no_deadline_still_returns_when_a_grandchild_holds_the_pipe() {
        // Third wedge, and the one that hits the *default* configuration:
        // no timeout configured, the direct child exits, and a backgrounded
        // grandchild holds the stdout pipe (`sleep 5 & echo ok`). The wait
        // loop finishes, but a plain reader join would block until the
        // grandchild exits — indefinitely, since there is no deadline to
        // arm the kill. The reader loop must reap the group itself.
        let tool = BashTool::default(); // no timeout: unbounded
        let started = Instant::now();
        let output = tool
            .execute(&json!({"command": "sleep 30 & echo ok"}))
            .unwrap()
            .text;
        assert_eq!(output, "ok");
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
            .unwrap()
            .text;
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

    // ---- edit tool ----

    fn edit(arguments: Value) -> Result<String, ToolError> {
        EditTool.execute(&arguments).map(|out| out.text)
    }

    /// Write a scratch file and return its (absolute) path.
    fn scratch(name: &str, contents: &str) -> String {
        let dir = std::env::temp_dir().join(format!("clanky-edit-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path.display().to_string()
    }

    /// Best-effort cleanup of every scratch directory this test binary's
    /// processes created (the pid suffix differs per test executable run,
    /// so the glob reaches stale leftovers too).
    #[test]
    fn edit_scratch_dirs_are_cleaned_up() {
        for entry in std::fs::read_dir(std::env::temp_dir()).unwrap().flatten() {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("clanky-edit-test-"))
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    fn read(path: &str) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn edit_replaces_a_unique_occurrence() {
        let path = scratch("unique.txt", "one\ntwo\nthree\n");
        let result = edit(json!({
            "path": path,
            "old_string": "two",
            "new_string": "TWO"
        }))
        .unwrap();
        assert_eq!(result, format!("Replaced 1 occurrence(s) in {path}"));
        assert_eq!(read(&path), "one\nTWO\nthree\n");
    }

    #[test]
    fn edit_fails_on_zero_matches() {
        let path = scratch("nomatch.txt", "one\ntwo\n");
        let err = edit(json!({
            "path": path,
            "old_string": "nope",
            "new_string": "x"
        }))
        .unwrap_err();
        assert!(
            matches!(err, ToolError::Failed { message, .. } if message.contains("occurs 0 times"))
        );
        assert_eq!(read(&path), "one\ntwo\n");
    }

    #[test]
    fn edit_fails_on_multiple_matches_without_replace_all() {
        let path = scratch("multi.txt", "a b a\n");
        let err = edit(json!({
            "path": path,
            "old_string": "a",
            "new_string": "x"
        }))
        .unwrap_err();
        assert!(
            matches!(err, ToolError::Failed { message, .. } if message.contains("occurs 2 times") && message.contains("surrounding context"))
        );
        assert_eq!(read(&path), "a b a\n");
    }

    #[test]
    fn edit_replace_all_replaces_every_occurrence() {
        let path = scratch("all.txt", "a b a\n");
        let result = edit(json!({
            "path": path,
            "old_string": "a",
            "new_string": "x",
            "replace_all": true
        }))
        .unwrap();
        assert_eq!(result, format!("Replaced 2 occurrence(s) in {path}"));
        assert_eq!(read(&path), "x b x\n");
    }

    #[test]
    fn edit_matches_are_literal_not_patterns() {
        // `.` is not a wildcard: `a.c` matches only the literal `a.c`,
        // never `abc`. One match succeeds and replaces exactly that text.
        let path = scratch("literal.txt", "a.c abc\n");
        edit(json!({
            "path": path,
            "old_string": "a.c",
            "new_string": "x"
        }))
        .unwrap();
        assert_eq!(read(&path), "x abc\n");

        // And a regex-only needle that would match `abc` matches nothing.
        let path = scratch("literal2.txt", "abc\n");
        let err = edit(json!({
            "path": path,
            "old_string": "a.c",
            "new_string": "x"
        }))
        .unwrap_err();
        assert!(
            matches!(err, ToolError::Failed { message, .. } if message.contains("occurs 0 times"))
        );
        assert_eq!(read(&path), "abc\n");
    }

    #[test]
    fn edit_preserves_invalid_utf8_beside_the_edit() {
        let dir = std::env::temp_dir().join(format!("clanky-edit-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("binary.bin");
        std::fs::write(&path, b"before \xff after").unwrap();
        let path = path.display().to_string();
        edit(json!({
            "path": path,
            "old_string": "before",
            "new_string": "after"
        }))
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"after \xff after");
    }

    #[test]
    fn edit_reports_missing_file() {
        let dir = std::env::temp_dir().join(format!("clanky-edit-test-{}", std::process::id()));
        let path = dir.join("does-not-exist.txt");
        let err = edit(json!({
            "path": path.display().to_string(),
            "old_string": "a",
            "new_string": "b"
        }))
        .unwrap_err();
        assert!(
            matches!(err, ToolError::Failed { message, .. } if message.contains("failed to read"))
        );
    }

    #[test]
    fn edit_validates_its_arguments() {
        let path = scratch("args.txt", "x\n");
        let err = edit(json!({"path": path, "old_string": "x"})).unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArguments { message, .. } if message.contains("`new_string`"))
        );
        let err = edit(json!({"path": path, "old_string": 5, "new_string": "y"})).unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArguments { message, .. } if message.contains("`old_string`"))
        );
        let err = edit(json!({"old_string": "x", "new_string": "y"})).unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArguments { message, .. } if message.contains("`path`"))
        );
        let err = edit(json!({"path": path, "old_string": "", "new_string": "y"})).unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArguments { message, .. } if message.contains("`old_string` must not be empty"))
        );
        let err = edit(json!({
            "path": path,
            "old_string": "x",
            "new_string": "y",
            "replace_all": "yes"
        }))
        .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArguments { message, .. } if message.contains("`replace_all`"))
        );
    }

    #[test]
    fn edit_counts_multiline_and_adjacent_matches() {
        // Multi-line needle, matched byte-for-byte including the newline.
        let path = scratch("multiline.txt", "start\nmiddle\nend\nstart\nmiddle\nend\n");
        let result = edit(json!({
            "path": path,
            "old_string": "start\nmiddle\nend",
            "new_string": "block"
        }));
        // Two identical blocks: fails without `replace_all`.
        assert!(result.is_err());
        let result = edit(json!({
            "path": path,
            "old_string": "start\nmiddle\nend",
            "new_string": "block",
            "replace_all": true
        }))
        .unwrap();
        assert_eq!(result, format!("Replaced 2 occurrence(s) in {path}"));
        assert_eq!(read(&path), "block\nblock\n");

        // Adjacent occurrences are found: "xx" in "xxxx" is 2 replacements.
        let path = scratch("adjacent.txt", "xxxx\n");
        let result = edit(json!({
            "path": path,
            "old_string": "xx",
            "new_string": "y",
            "replace_all": true
        }))
        .unwrap();
        assert_eq!(result, format!("Replaced 2 occurrence(s) in {path}"));
        assert_eq!(read(&path), "yy\n");
    }

    // ---- view-image tool ----

    fn view(arguments: Value) -> Result<ToolOutput, ToolError> {
        ViewImageTool.execute(&arguments)
    }

    /// Write a scratch image file (bytes verbatim) and return its path.
    fn image(name: &str, contents: &[u8]) -> String {
        let dir = std::env::temp_dir().join(format!("clanky-view-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path.display().to_string()
    }

    #[test]
    fn view_image_attaches_sniffed_media_type() {
        // A real PNG signature (sniffed, regardless of the extension).
        let png = b"\x89PNG\r\n\x1a\nfakepngdata";
        let path = image("shot.png", png);
        let out = view(json!({"path": path})).unwrap();
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].media_type, MediaType::Png);
        assert_eq!(
            out.images[0].data_base64,
            base64::engine::general_purpose::STANDARD.encode(png)
        );
        assert_eq!(out.text, format!("{} bytes", png.len()));
    }

    #[test]
    fn view_image_resolves_extension_when_the_signature_is_unknown() {
        // HEIC shares the ISO-BMFF box structure with video formats, so
        // the byte sniff deliberately declines; the extension answers.
        let path = image("photo.heic", b"\x00\x00\x00\x18ftypheic");
        let out = view(json!({"path": path})).unwrap();
        assert_eq!(out.images[0].media_type, MediaType::Heic);
    }

    #[test]
    fn view_image_extension_is_case_insensitive_and_jpeg_aliases() {
        let path = image("PHOTO.JPG", b"\xFF\xD8\xFF\xE0");
        let out = view(json!({"path": path})).unwrap();
        assert_eq!(out.images[0].media_type, MediaType::Jpeg);
        // Sniffing wins over a wrong extension: a jpeg named .png is a jpeg.
        let path = image("misnamed.png", b"\xFF\xD8\xFF\xE0");
        let out = view(json!({"path": path})).unwrap();
        assert_eq!(out.images[0].media_type, MediaType::Jpeg);
    }

    #[test]
    fn view_image_rejects_unknown_types() {
        let path = image("notes.txt", b"just text");
        let err = view(json!({"path": path})).unwrap_err();
        assert!(
            matches!(err, ToolError::Failed { ref message, .. } if message.contains("unknown image media type")),
            "{err}"
        );
        // A missing extension on an unrecognizable file is the same error.
        let dir = std::env::temp_dir().join(format!("clanky-view-test-{}", std::process::id()));
        let path = dir.join("noext");
        std::fs::write(&path, b"data").unwrap();
        let err = view(json!({"path": path.display().to_string()})).unwrap_err();
        assert!(
            matches!(err, ToolError::Failed { ref message, .. } if message.contains("unknown image media type")),
            "{err}"
        );
    }

    #[test]
    fn view_image_reports_missing_file() {
        let dir = std::env::temp_dir().join(format!("clanky-view-test-{}", std::process::id()));
        let path = dir.join("does-not-exist.png");
        let err = view(json!({"path": path.display().to_string()})).unwrap_err();
        assert!(
            matches!(err, ToolError::Failed { ref message, .. } if message.contains("failed to read")),
            "{err}"
        );
    }

    #[test]
    fn view_image_validates_its_arguments() {
        let err = view(json!({})).unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArguments { message, .. } if message.contains("`path`"))
        );
        let err = view(json!({"path": 42})).unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArguments { message, .. } if message.contains("`path`"))
        );
    }

    #[test]
    fn view_image_tool_metadata_matches_protocol_shape() {
        let tool = ViewImageTool;
        assert_eq!(tool.name(), "view-image");
        assert_eq!(
            tool.description(),
            "Read an image file from disk and attach it so you can see it."
        );
        let wire = tool.wire();
        assert_eq!(wire.name, "view-image");
        assert_eq!(wire.parameters.unwrap()["required"][0], "path");
    }

    #[test]
    fn size_note_uses_si_units() {
        assert_eq!(size_note(623), "623 bytes");
        assert_eq!(size_note(999), "999 bytes");
        assert_eq!(size_note(256_000), "256 kb");
        assert_eq!(size_note(1_234_567), "1.2 mb");
    }
}
