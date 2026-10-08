//! Session persistence (M4).
//!
//! A session is one JSONL file in `./.clanky/sessions/`, one record per
//! line. The first line is a versioned header; every later line is one
//! [`Record`] describing an event of the conversation. The format is
//! append-only (autosave writes each record as it happens, so killing the
//! TUI loses at most the event in flight) and forward-compatible (unknown
//! record types are skipped on load, mirroring the provider protocol's
//! versioning policy).
//!
//! Resuming restores two things: the transcript entries the TUI renders
//! and the full [`ChatMessage`] history, so the next turn continues with
//! the complete context. [`history_from_records`] rebuilds the history;
//! tool calls that were recorded but never got a result (an interrupted
//! turn) are answered with a synthetic error result so the message
//! sequence stays valid for the backend.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use clanky_protocol::{ChatMessage, ToolCall, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};

/// Current session file format version (§ format header).
pub const FORMAT_VERSION: u32 = 1;

/// Where sessions of the current project live.
pub fn sessions_dir() -> PathBuf {
    crate::config::project_dir().join(crate::config::SESSIONS_DIR)
}

/// Current wall-clock time as epoch milliseconds.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Session file header: the first line of every session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Header {
    pub version: u32,
    /// Creation time, epoch milliseconds.
    pub created: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// One conversation event. Serialized with a `"type"` tag (camelCase
/// field names), one JSON object per line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Record {
    /// The user's prompt.
    User { text: String },
    /// The complete thinking text of one assistant round.
    Thinking { text: String },
    /// The complete visible text of one assistant round plus its parsed
    /// tool calls (empty when the round made none).
    Assistant { text: String, calls: Vec<ToolCall> },
    /// A tool result fed back to the model.
    ToolResult { name: String, output: String },
    /// Token usage of one turn.
    Usage {
        #[serde(default)]
        prompt_tokens: Option<u64>,
        #[serde(default)]
        completion_tokens: Option<u64>,
    },
    /// A turn or write error, kept so the transcript survives resume.
    Error { message: String },
}

/// One parsed session file.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionData {
    pub header: Header,
    pub records: Vec<Record>,
}

/// Summary of one saved session, for the `/resume` picker.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub path: PathBuf,
    /// Display name (the file stem).
    pub name: String,
    /// Last modification time, epoch seconds.
    pub modified: u64,
    /// Number of user prompts in the session.
    pub turns: usize,
}

/// Append-only session file writer. The file is created lazily on the
/// first record, so a session that never sends a prompt leaves no file.
pub struct SessionWriter {
    path: PathBuf,
    header: Header,
    mode: Mode,
    file: Option<File>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    /// Create a fresh file (with header) on the first record.
    Create,
    /// Continue an existing file (no new header); create it with a
    /// header only if it has vanished.
    Reopen,
}

impl SessionWriter {
    /// A writer for a new session file (not created until the first
    /// record is appended).
    pub fn new(path: PathBuf, header: Header) -> Self {
        Self {
            path,
            header,
            mode: Mode::Create,
            file: None,
        }
    }

    /// A writer that continues an existing session file (no new header).
    pub fn reopen(path: PathBuf) -> Self {
        Self {
            path,
            header: Header {
                version: FORMAT_VERSION,
                created: 0,
                provider: None,
                model: None,
            },
            mode: Mode::Reopen,
            file: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record (the header first, on file creation) and flush,
    /// so a killed process loses at most the record being written.
    pub fn append(&mut self, record: &Record) -> Result<()> {
        if self.file.is_none() {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent).map_err(|source| Error::WriteSession {
                    path: self.path.clone(),
                    source,
                })?;
            }
            self.file = Some(self.open_file()?);
        }
        let file = self.file.as_mut().expect("file opened above");
        let line = serde_json::to_string(record).expect("record serializes") + "\n";
        file.write_all(line.as_bytes())
            .and_then(|_| file.flush())
            .map_err(|source| Error::WriteSession {
                path: self.path.clone(),
                source,
            })
    }

    fn open_file(&mut self) -> Result<File> {
        if self.mode == Mode::Reopen && self.path.exists() {
            return OpenOptions::new()
                .append(true)
                .open(&self.path)
                .map_err(|source| Error::WriteSession {
                    path: self.path.clone(),
                    source,
                });
        }
        // Fresh file: write the header first, tagged with its type. A
        // missing `created` value (reopened writer whose file vanished)
        // is stamped with now.
        if self.header.created == 0 {
            self.header.created = now_millis();
        }
        let mut file = File::create_new(&self.path).map_err(|source| Error::WriteSession {
            path: self.path.clone(),
            source,
        })?;
        let mut header_value = serde_json::to_value(&self.header).expect("header serializes");
        header_value["type"] = "header".into();
        let header_line =
            serde_json::to_string(&header_value).expect("header is valid JSON") + "\n";
        file.write_all(header_line.as_bytes())
            .map_err(|source| Error::WriteSession {
                path: self.path.clone(),
                source,
            })?;
        Ok(file)
    }

    /// Rename the session file (the display name is the file stem).
    /// Fails if the target already exists or the name is invalid.
    pub fn rename(&mut self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        let target = self
            .path
            .parent()
            .unwrap_or(Path::new("."))
            .join(format!("{name}.jsonl"));
        if target != self.path && target.exists() {
            return Err(Error::SessionExists(name.into()));
        }
        if target != self.path {
            std::fs::rename(&self.path, &target).map_err(|source| Error::WriteSession {
                path: target.clone(),
                source,
            })?;
            self.path = target.clone();
        }
        Ok(target)
    }
}

/// Validate a session display name.
pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidSessionName(name.into()))
    }
}

/// A default file name for unnamed sessions: local-ish timestamp plus a
/// short unique suffix, e.g. `20250101-101530-3fa1.jsonl`.
pub fn default_name() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let stamp = format_timestamp_compact(now.as_secs());
    let mix = (now.subsec_nanos() ^ (process::id() << 13)) & 0xffff;
    format!("{stamp}-{mix:04x}")
}

/// `YYYYMMDD-HHMMSS` from epoch seconds (UTC; sessions are local files).
fn format_timestamp_compact(epoch_secs: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil(epoch_secs);
    format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}")
}

/// Human date/time for pickers and listings: `YYYY-MM-DD HH:MM` (UTC).
pub fn format_datetime(epoch_secs: u64) -> String {
    let (y, m, d, hh, mm, _) = civil(epoch_secs);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

/// Split epoch seconds into UTC calendar fields. Days are converted with
/// the civil-from-days algorithm (Howard Hinnant) — no chrono needed.
fn civil(epoch_secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (epoch_secs / 86_400) as i64;
    let secs_of_day = (epoch_secs % 86_400) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // year of era
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year
    let mp = (5 * doy + 2) / 153; // month index [0, 11] starting March
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    )
}

/// List saved sessions in `dir`, newest first. Files that do not parse
/// as sessions are skipped (they may be mid-write or corrupt).
pub fn list(dir: &Path) -> Vec<SessionInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut infos: Vec<SessionInfo> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Some(modified) = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        else {
            continue;
        };
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let name = name.to_string();
        let turns = count_user_turns(&path);
        infos.push(SessionInfo {
            path,
            name,
            modified: modified.as_secs(),
            turns,
        });
    }
    infos.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.name.cmp(&b.name))
    });
    infos
}

fn count_user_turns(path: &Path) -> usize {
    let Ok(file) = File::open(path) else {
        return 0;
    };
    BufReader::new(file)
        .lines()
        .map_while(std::result::Result::ok)
        .filter(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .filter(|v| v.get("type").and_then(Value::as_str) == Some("user"))
                .is_some()
        })
        .count()
}

/// Read and parse a session file. Tolerates unknown record types and
/// malformed lines (skipped); the header must be present and known.
pub fn load(path: &Path) -> Result<SessionData> {
    let file = File::open(path).map_err(|source| Error::InvalidSession {
        path: path.to_path_buf(),
        message: format!("cannot open ({source})"),
    })?;
    let mut lines = BufReader::new(file).lines();

    let header_line = lines
        .next()
        .transpose()
        .map_err(|source| Error::InvalidSession {
            path: path.to_path_buf(),
            message: format!("cannot read ({source})"),
        })?
        .ok_or_else(|| Error::InvalidSession {
            path: path.to_path_buf(),
            message: "empty file".into(),
        })?;

    let header: Header = parse_header(path, &header_line)?;

    let mut records = Vec::new();
    for line in lines {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue; // malformed line: skip
        };
        let kind = value.get("type").and_then(Value::as_str);
        if !matches!(
            kind,
            Some("user" | "thinking" | "assistant" | "toolResult" | "usage" | "error")
        ) {
            continue; // unknown record type: skip (forward compatibility)
        }
        if let Ok(record) = serde_json::from_value::<Record>(value) {
            records.push(record); // known type, bad payload: skip
        }
    }
    Ok(SessionData { header, records })
}

fn parse_header(path: &Path, line: &str) -> Result<Header> {
    let value: Value = serde_json::from_str(line).map_err(|err| Error::InvalidSession {
        path: path.to_path_buf(),
        message: format!("bad header ({err})"),
    })?;
    if value.get("type").and_then(Value::as_str) != Some("header") {
        return Err(Error::InvalidSession {
            path: path.to_path_buf(),
            message: "first line is not a session header".into(),
        });
    }
    let header: Header = serde_json::from_value(value).map_err(|err| Error::InvalidSession {
        path: path.to_path_buf(),
        message: format!("bad header ({err})"),
    })?;
    if header.version > FORMAT_VERSION {
        return Err(Error::UnsupportedSessionVersion {
            path: path.to_path_buf(),
            found: header.version,
            supported: FORMAT_VERSION,
        });
    }
    Ok(header)
}

/// Rebuild the full conversation history (system context excluded —
/// callers re-assemble it) from session records. Tool results are matched
/// to their calls by order; calls without a result (interrupted turn)
/// get a synthetic error result so the sequence stays backend-valid.
pub fn history_from_records(records: &[Record]) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    let mut awaiting: VecDeque<String> = VecDeque::new();
    for record in records {
        match record {
            Record::User { text } => messages.push(ChatMessage::user(text)),
            Record::Thinking { .. } | Record::Usage { .. } | Record::Error { .. } => {}
            Record::Assistant { text, calls } => {
                for call in calls {
                    awaiting.push_back(call.id.clone());
                }
                messages.push(ChatMessage::Assistant {
                    content: text.clone(),
                    tool_calls: (!calls.is_empty()).then(|| calls.clone()),
                });
            }
            Record::ToolResult { output, .. } => {
                if let Some(id) = awaiting.pop_front() {
                    messages.push(ChatMessage::tool(id, output));
                }
            }
        }
    }
    for id in awaiting.drain(..) {
        messages.push(ChatMessage::tool(id, "ERROR: turn was interrupted"));
    }
    messages
}

/// Convenience: token usage as a record (clamps `None`s for the file).
pub fn usage_record(usage: Usage) -> Record {
    Record::Usage {
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clanky-session-{name}"));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_records() -> Vec<Record> {
        vec![
            Record::User {
                text: "count files".into(),
            },
            Record::Assistant {
                text: "Let me check.".into(),
                calls: vec![ToolCall {
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
                prompt_tokens: Some(10),
                completion_tokens: Some(4),
            },
        ]
    }

    fn write_session(dir: &Path, name: &str, records: &[Record]) -> PathBuf {
        let path = dir.join(format!("{name}.jsonl"));
        let mut writer = SessionWriter::new(
            path.clone(),
            Header {
                version: FORMAT_VERSION,
                created: 1_700_000_000_000,
                provider: Some("deepinfra".into()),
                model: Some("test/model".into()),
            },
        );
        for record in records {
            writer.append(record).unwrap();
        }
        path
    }

    #[test]
    fn roundtrip_write_and_load() {
        let dir = temp_dir("roundtrip");
        let records = sample_records();
        let path = write_session(&dir, "test", &records);
        let data = load(&path).unwrap();
        assert_eq!(data.header.version, FORMAT_VERSION);
        assert_eq!(data.header.provider.as_deref(), Some("deepinfra"));
        assert_eq!(data.records, records);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn lazy_writer_creates_file_on_first_record() {
        let dir = temp_dir("lazy");
        let path = dir.join("lazy.jsonl");
        SessionWriter::new(
            path.clone(),
            Header {
                version: FORMAT_VERSION,
                created: 0,
                provider: None,
                model: None,
            },
        );
        assert!(!path.exists(), "no file before the first record");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rename_moves_the_file_and_later_appends_follow() {
        let dir = temp_dir("rename");
        let records = sample_records();
        let path = write_session(&dir, "before", &records);
        let mut writer = SessionWriter::reopen(path);
        let renamed = writer.rename("after").unwrap();
        assert!(renamed.exists());
        assert!(!dir.join("before.jsonl").exists());
        writer
            .append(&Record::User {
                text: "next".into(),
            })
            .unwrap();
        let data = load(&renamed).unwrap();
        assert_eq!(data.records.len(), records.len() + 1);
        assert_eq!(
            data.records.last(),
            Some(&Record::User {
                text: "next".into()
            })
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rename_rejects_invalid_and_existing_names() {
        let dir = temp_dir("rename-invalid");
        let path = write_session(&dir, "a", &[Record::User { text: "x".into() }]);
        let mut writer = SessionWriter::reopen(path.clone());
        assert!(matches!(
            writer.rename("with/slash"),
            Err(Error::InvalidSessionName(_))
        ));
        assert!(matches!(
            writer.rename(""),
            Err(Error::InvalidSessionName(_))
        ));
        assert!(matches!(
            writer.rename(".hidden"),
            Err(Error::InvalidSessionName(_))
        ));
        write_session(&dir, "taken", &[Record::User { text: "x".into() }]);
        assert!(matches!(
            writer.rename("taken"),
            Err(Error::SessionExists(_))
        ));
        assert!(dir.join("a.jsonl").exists(), "failed rename keeps the file");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn load_skips_unknown_and_malformed_lines() {
        let dir = temp_dir("forward-compat");
        let path = dir.join("future.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"header","version":1,"created":1}"#,
                "\n",
                r#"{"type":"quantumFlux","level":11}"#,
                "\n",
                "not json at all\n",
                r#"{"type":"user","text":"hi"}"#,
                "\n",
            ),
        )
        .unwrap();
        let data = load(&path).unwrap();
        assert_eq!(data.records, vec![Record::User { text: "hi".into() }]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn load_rejects_missing_header_and_newer_versions() {
        let dir = temp_dir("bad-header");
        let bad = dir.join("bad.jsonl");
        std::fs::write(&bad, "{\"type\":\"user\",\"text\":\"hi\"}\n").unwrap();
        assert!(
            load(&bad)
                .unwrap_err()
                .to_string()
                .contains("not a session header")
        );

        let newer = dir.join("newer.jsonl");
        std::fs::write(
            &newer,
            "{\"type\":\"header\",\"version\":2,\"created\":1}\n",
        )
        .unwrap();
        assert!(matches!(
            load(&newer),
            Err(Error::UnsupportedSessionVersion { found: 2, .. })
        ));

        let empty = dir.join("empty.jsonl");
        std::fs::write(&empty, "").unwrap();
        assert!(
            load(&empty).unwrap_err().to_string().contains("empty file"),
            "empty file is an error, not an empty session"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn history_rebuilds_conversation_from_records() {
        let records = sample_records();
        let history = history_from_records(&records);
        assert_eq!(history.len(), 4, "user, assistant+call, tool, assistant");
        assert!(matches!(history[0], ChatMessage::User { .. }));
        assert!(matches!(
            &history[1],
            ChatMessage::Assistant {
                tool_calls: Some(_),
                ..
            }
        ));
        assert!(
            matches!(&history[2], ChatMessage::Tool { tool_call_id, .. } if tool_call_id == "call_1")
        );
        assert!(matches!(
            &history[3],
            ChatMessage::Assistant {
                tool_calls: None,
                ..
            }
        ));
    }

    #[test]
    fn interrupted_calls_get_synthetic_results() {
        let records = vec![
            Record::User { text: "go".into() },
            Record::Assistant {
                text: String::new(),
                calls: vec![ToolCall {
                    id: "call_9".into(),
                    name: "bash".into(),
                    arguments: json!({}),
                }],
            },
            // No tool result: the turn was killed mid-flight.
        ];
        let history = history_from_records(&records);
        assert_eq!(history.len(), 3);
        assert!(
            matches!(&history[2], ChatMessage::Tool { content, .. } if content.contains("interrupted"))
        );
    }

    #[test]
    fn list_reports_sessions_newest_first_with_turn_counts() {
        let dir = temp_dir("listing");
        write_session(&dir, "old", &sample_records());
        // Ensure a distinct mtime ordering.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let newer = write_session(&dir, "newer", &[Record::User { text: "x".into() }]);
        // A stray non-session file is ignored.
        std::fs::write(dir.join("notes.txt"), "hello").unwrap();

        let infos = list(&dir);
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].name, "newer");
        assert_eq!(infos[0].path, newer);
        assert_eq!(infos[0].turns, 1);
        assert_eq!(infos[1].name, "old");
        assert_eq!(infos[1].turns, 1, "one user record in sample_records");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn default_name_is_timestamped_and_valid() {
        let name = default_name();
        assert!(validate_name(&name).is_ok(), "{name}");
        assert_eq!(name.len(), 20, "YYYYMMDD-HHMMSS-xxxx");
    }

    #[test]
    fn datetime_formatting() {
        assert_eq!(format_datetime(0), "1970-01-01 00:00");
        assert_eq!(format_datetime(951_825_600), "2000-02-29 12:00");
        assert_eq!(format_datetime(1_700_000_000), "2023-11-14 22:13");
        assert_eq!(format_timestamp_compact(1_700_000_000), "20231114-221320");
    }
}
