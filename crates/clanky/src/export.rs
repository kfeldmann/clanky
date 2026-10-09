//! Markdown export (`/md`): turn the current session into a standalone
//! Markdown document.
//!
//! The transcript comes from the same [`Record`] stream the session file
//! holds, so an export is a faithful rendering of what happened: user
//! prompts, assistant answers, thinking blocks, tool calls with their
//! results and errors. A header carries the session metadata (provider,
//! model, creation time, selected contents) and a footer carries the
//! session's total token usage.
//!
//! Record types are opt-in through [`ExportOptions`]: by default only the
//! user and visible assistant text is written, and `--thinking`/`--tools`
//! add the rest (or `--all`, which is both). Authored text — user prompts
//! and assistant answers — is written verbatim: it is Markdown and must
//! keep rendering as itself, code blocks included. Thinking and tool
//! output — raw, machine-produced text — are wrapped in fenced code
//! blocks, so nothing in them can leak into the surrounding document.

use clanky_protocol::Usage;

use crate::session::{self, Header, Record};

/// Which record types an export includes.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ExportOptions {
    /// Include thinking blocks.
    pub thinking: bool,
    /// Include tool calls, their arguments and their results.
    pub tools: bool,
}

impl ExportOptions {
    /// `--all`: everything the session file holds.
    pub const ALL: Self = Self {
        thinking: true,
        tools: true,
    };

    /// Merge another set of flags into these (command-line flags combine).
    pub fn or(self, other: Self) -> Self {
        Self {
            thinking: self.thinking || other.thinking,
            tools: self.tools || other.tools,
        }
    }

    /// Whether these options select only user and assistant text.
    pub fn is_default(&self) -> bool {
        !self.thinking && !self.tools
    }
}

/// Render `records` as a Markdown document. `name` is the session's
/// display name (the file stem), when it has one.
pub fn render(
    name: Option<&str>,
    header: &Header,
    records: &[Record],
    options: ExportOptions,
) -> String {
    let mut out = String::new();
    out.push_str("# Clanky session");
    if let Some(name) = name {
        out.push_str(&format!(": {name}"));
    }
    out.push_str("\n\n");

    out.push_str("| | |\n|---|---|\n");
    let mut meta = |label: &str, value: String| {
        out.push_str(&format!("| {label} | {value} |\n"));
    };
    if let Some(provider) = &header.provider {
        meta("Provider", provider.clone());
    }
    if let Some(model) = &header.model {
        meta("Model", model.clone());
    }
    meta("Created", session::format_datetime(header.created / 1000));
    meta("Contents", format_options(options));
    out.push('\n');

    let mut saw_usage = false;
    let mut wrote_anything = false;
    for record in records {
        match record {
            Record::User { text } => {
                out.push_str("## User\n\n");
                out.push_str(&verbatim(text));
                out.push('\n');
                wrote_anything = true;
            }
            Record::Assistant { text, calls } => {
                if !text.trim().is_empty() {
                    out.push_str("## Assistant\n\n");
                    out.push_str(&verbatim(text));
                    out.push('\n');
                    wrote_anything = true;
                }
                if options.tools {
                    for call in calls {
                        out.push_str(&format!("### Tool call: `{}`\n\n", call.name));
                        let arguments = serde_json::to_string_pretty(&call.arguments)
                            .unwrap_or_else(|_| call.arguments.to_string());
                        out.push_str(&fence(&arguments, "json"));
                        out.push('\n');
                        wrote_anything = true;
                    }
                }
            }
            Record::Thinking { text } => {
                if options.thinking && !text.trim().is_empty() {
                    out.push_str("## Thinking\n\n");
                    out.push_str(&fence(text, ""));
                    out.push('\n');
                    wrote_anything = true;
                }
            }
            Record::ToolResult { name, output } => {
                if options.tools {
                    out.push_str(&format!("### Tool result: `{name}`\n\n"));
                    out.push_str(&fence(output, ""));
                    out.push('\n');
                    wrote_anything = true;
                }
            }
            Record::Error { message } => {
                out.push_str("## Error\n\n");
                out.push_str(&fence(message, ""));
                out.push('\n');
                wrote_anything = true;
            }
            Record::Usage { .. } => {
                // Usage is summarized once, after the transcript.
                saw_usage = true;
            }
        }
    }

    if !wrote_anything {
        out.push_str("_Nothing to export with the selected options._\n");
    }
    if saw_usage {
        out.push_str(&format!(
            "\n---\n\n_{}_\n",
            describe_usage(total_usage(records))
        ));
    }
    out
}

/// Total token usage across `records` (prompt and completion summed over
/// every per-round usage report), for the transcript footer and `/md`
/// summaries.
pub fn total_usage(records: &[Record]) -> Usage {
    let mut prompt = 0u64;
    let mut completion = 0u64;
    for record in records {
        if let Record::Usage {
            prompt_tokens,
            completion_tokens,
        } = record
        {
            prompt += prompt_tokens.unwrap_or(0);
            completion += completion_tokens.unwrap_or(0);
        }
    }
    Usage {
        prompt_tokens: Some(prompt),
        completion_tokens: Some(completion),
        cached_tokens: None,
    }
}

fn describe_usage(usage: Usage) -> String {
    format!(
        "session total: {} prompt tokens · {} completion tokens",
        usage.prompt_tokens.unwrap_or(0),
        usage.completion_tokens.unwrap_or(0)
    )
}

/// A human description of the selected options, for the header table.
fn format_options(options: ExportOptions) -> String {
    if options.is_default() {
        return "user and assistant messages".into();
    }
    let mut parts = vec!["user and assistant messages".to_string()];
    if options.thinking {
        parts.push("thinking".into());
    }
    if options.tools {
        parts.push("tool calls and results".into());
    }
    parts.join(", ")
}

/// An authored text body: written as-is. User prompts and assistant
/// answers are Markdown — code blocks, headings, lists included — and a
/// fenced block inside them is balanced by its own closing fence, so it
/// needs no wrapping to render in place.
fn verbatim(text: &str) -> String {
    format!("{}\n", text.trim_end())
}

/// Wrap `text` in a code fence, lengthening the fence when the content
/// itself contains backtick runs (so nothing can escape the block).
fn fence(text: &str, language: &str) -> String {
    let mut longest = 0usize;
    let mut run = 0usize;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let ticks = "`".repeat(longest.max(2) + 1);
    let text = text.strip_suffix('\n').unwrap_or(text);
    format!("{ticks}{language}\n{text}\n{ticks}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clanky_protocol::ToolCall;
    use serde_json::json;

    fn header() -> Header {
        Header {
            version: session::FORMAT_VERSION,
            created: 1_700_000_000_000,
            provider: Some("deepinfra".into()),
            model: Some("mock/model".into()),
        }
    }

    fn records() -> Vec<Record> {
        vec![
            Record::User {
                text: "count files".into(),
            },
            Record::Thinking {
                text: "I should run ls.".into(),
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

    #[test]
    fn default_export_keeps_only_user_and_assistant() {
        let doc = render(
            Some("session-a"),
            &header(),
            &records(),
            ExportOptions::default(),
        );
        assert!(doc.contains("# Clanky session: session-a"), "{doc}");
        assert!(doc.contains("| Provider | deepinfra |"), "{doc}");
        assert!(doc.contains("| Model | mock/model |"), "{doc}");
        assert!(doc.contains("## User\n\ncount files"), "{doc}");
        assert!(doc.contains("## Assistant\n\nLet me check."), "{doc}");
        assert!(doc.contains("42 files."), "{doc}");
        // Thinking, tools and errors stay out.
        assert!(!doc.contains("I should run ls."), "{doc}");
        assert!(!doc.contains("bash"), "{doc}");
        assert!(!doc.contains("Tool call"), "{doc}");
        // The usage footer is always informative.
        assert!(
            doc.contains("session total: 10 prompt tokens · 4 completion tokens"),
            "{doc}"
        );
    }

    #[test]
    fn thinking_flag_adds_thinking_blocks() {
        let doc = render(
            Some("session-a"),
            &header(),
            &records(),
            ExportOptions {
                thinking: true,
                tools: false,
            },
        );
        assert!(doc.contains("## Thinking"), "{doc}");
        assert!(doc.contains("I should run ls."), "{doc}");
        assert!(!doc.contains("Tool call"), "{doc}");
    }

    #[test]
    fn tools_flag_adds_calls_arguments_and_results() {
        let doc = render(
            Some("session-a"),
            &header(),
            &records(),
            ExportOptions {
                thinking: false,
                tools: true,
            },
        );
        assert!(doc.contains("### Tool call: `bash`"), "{doc}");
        assert!(doc.contains("\"command\": \"ls | wc -l\""), "{doc}");
        assert!(doc.contains("### Tool result: `bash`"), "{doc}");
        assert!(doc.contains("42"), "{doc}");
        assert!(!doc.contains("I should run ls."), "{doc}");
    }

    #[test]
    fn all_includes_everything() {
        let doc = render(Some("session-a"), &header(), &records(), ExportOptions::ALL);
        assert!(doc.contains("## Thinking"), "{doc}");
        assert!(doc.contains("### Tool call: `bash`"), "{doc}");
        assert!(doc.contains("### Tool result: `bash`"), "{doc}");
        assert!(
            doc.contains(
                "| Contents | user and assistant messages, thinking, tool calls and results |"
            ),
            "{doc}"
        );
    }

    #[test]
    fn fenced_content_cannot_break_the_document() {
        let records = vec![Record::Thinking {
            text: "a ``` b\n```\nmore".into(),
        }];
        let doc = render(Some("session-a"), &header(), &records, ExportOptions::ALL);
        assert!(doc.contains("````\na ``` b\n```\nmore\n````"), "{doc}");
    }

    #[test]
    fn assistant_markdown_is_kept_as_markdown() {
        let records = vec![Record::Assistant {
            text: "## Findings\n\n- one\n- two".into(),
            calls: vec![],
        }];
        let doc = render(
            Some("session-a"),
            &header(),
            &records,
            ExportOptions::default(),
        );
        assert!(doc.contains("## Findings\n\n- one\n- two"), "{doc}");
    }

    #[test]
    fn a_body_carrying_a_fence_is_kept_verbatim() {
        let records = vec![Record::Assistant {
            text: "here:\n```sh\nls\n```".into(),
            calls: vec![],
        }];
        let doc = render(
            Some("session-a"),
            &header(),
            &records,
            ExportOptions::default(),
        );
        assert!(doc.contains("```sh\nls\n```"), "{doc}");
        assert!(!doc.contains("````"), "{doc}");
    }

    #[test]
    fn user_text_with_a_code_block_is_verbatim_too() {
        let records = vec![
            Record::User {
                text: "why does this fail?\n```sh\nls\n```".into(),
            },
            Record::Assistant {
                text: "because ls lists files.".into(),
                calls: vec![],
            },
        ];
        let doc = render(
            Some("session-a"),
            &header(),
            &records,
            ExportOptions::default(),
        );
        assert!(doc.contains("```sh\nls\n```"), "{doc}");
        assert!(!doc.contains("````"), "{doc}");
    }

    #[test]
    fn errors_are_exported_even_by_default() {
        let records = vec![Record::Error {
            message: "provider error: 401".into(),
        }];
        let doc = render(
            Some("session-a"),
            &header(),
            &records,
            ExportOptions::default(),
        );
        assert!(doc.contains("## Error"), "{doc}");
        assert!(doc.contains("provider error: 401"), "{doc}");
    }

    #[test]
    fn usage_footer_sums_every_round() {
        let records = vec![
            Record::Usage {
                prompt_tokens: Some(10),
                completion_tokens: Some(4),
            },
            Record::Usage {
                prompt_tokens: Some(20),
                completion_tokens: Some(6),
            },
        ];
        let doc = render(None, &header(), &records, ExportOptions::default());
        assert!(
            doc.contains("session total: 30 prompt tokens · 10 completion tokens"),
            "{doc}"
        );
    }

    #[test]
    fn no_usage_records_means_no_footer() {
        let records = vec![Record::User { text: "hi".into() }];
        let doc = render(None, &header(), &records, ExportOptions::default());
        assert!(!doc.contains("session total"), "{doc}");
    }

    #[test]
    fn empty_selection_says_so() {
        let records = vec![Record::Thinking { text: "hmm".into() }];
        let doc = render(
            Some("session-a"),
            &header(),
            &records,
            ExportOptions::default(),
        );
        assert!(doc.contains("_Nothing to export"), "{doc}");
    }

    #[test]
    fn options_merge_and_describe() {
        let merged = ExportOptions {
            thinking: true,
            tools: false,
        }
        .or(ExportOptions {
            thinking: false,
            tools: true,
        });
        assert_eq!(merged, ExportOptions::ALL);
        assert!(ExportOptions::default().is_default());
        assert!(!ExportOptions::ALL.is_default());
    }
}
