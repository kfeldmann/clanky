//! Slash command parsing (M5): pure parsing, no TUI state.
//!
//! Built-in commands take precedence over prompt templates; an unknown
//! command parses to `Err` and is never sent to the model.

use crate::prompts::Template;

/// A parsed slash command.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// `/name [name]`: rename the session file.
    Name(Option<String>),
    /// `/resume`: pick and load a saved session.
    Resume,
    /// `/model [id]`: set the model, or open the picker.
    Model(Option<String>),
    /// `/provider [name]`: switch provider, or open the picker.
    Provider(Option<String>),
    /// `/thinking [value]`: set thinking, or open the picker.
    Thinking(Option<String>),
    /// `/sampling [edits]`.
    Sampling(SamplingEdit),
    /// `/` with no command word: open the command palette (commands and
    /// templates).
    Palette,
    /// `/<template> [extra text]`: put a prompt template into the input
    /// line for editing.
    Template {
        template: Template,
        extra: Option<String>,
    },
}

/// A `/sampling` argument shape.
#[derive(Debug, PartialEq)]
pub enum SamplingEdit {
    /// `/sampling`: show the current parameters.
    Show,
    /// `/sampling clear`: remove all parameters.
    Clear,
    /// `/sampling key=value[,key=value…]`: set parameters.
    Set(Vec<(String, String)>),
    /// `/sampling key[,key…]`: remove parameters.
    Unset(Vec<String>),
}

/// Parse a slash command. `None` = not a command (plain prompt);
/// `Some(Err)` = unknown or malformed command (never sent to the model).
/// `templates` are the discovered prompt templates; built-in command
/// names shadow templates of the same name.
pub fn parse_command(
    input: &str,
    templates: &[Template],
) -> Option<std::result::Result<Command, String>> {
    let rest = input.strip_prefix('/')?;
    let (word, arg) = rest.split_once(' ').unwrap_or((rest, ""));
    let arg = arg.trim();
    let optional_arg = || (!arg.is_empty()).then(|| arg.to_string());

    Some(match word {
        "" => Ok(Command::Palette),
        "name" => Ok(Command::Name(optional_arg())),
        "resume" if arg.is_empty() => Ok(Command::Resume),
        "resume" => Err("`/resume` takes no arguments".into()),
        "model" => Ok(Command::Model(optional_arg())),
        "provider" => Ok(Command::Provider(optional_arg())),
        "thinking" => Ok(Command::Thinking(optional_arg())),
        "sampling" => parse_sampling(arg).map(Command::Sampling),
        _ if let Some(template) = templates.iter().find(|t| t.name == word) => {
            Ok(Command::Template {
                template: template.clone(),
                extra: optional_arg(),
            })
        }
        other => Err(format!(
            "unknown command `/{other}`; type `/` to browse commands, or add a prompt \
             to .clanky/prompts/"
        )),
    })
}

/// Parse the `/sampling` argument into an edit.
fn parse_sampling(arg: &str) -> std::result::Result<SamplingEdit, String> {
    if arg.is_empty() {
        return Ok(SamplingEdit::Show);
    }
    if arg == "clear" {
        return Ok(SamplingEdit::Clear);
    }
    let pairs: Vec<&str> = arg
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let sets: Vec<Option<(&str, &str)>> = pairs
        .iter()
        .map(|pair| pair.split_once('=').map(|(k, v)| (k.trim(), v)))
        .collect();
    if sets.iter().all(Option::is_some) {
        let pairs = sets
            .into_iter()
            .map(|pair| pair.expect("all sets have a key").to_owned())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Ok(SamplingEdit::Set(pairs))
    } else if sets.iter().all(Option::is_none) {
        Ok(SamplingEdit::Unset(
            pairs.into_iter().map(str::to_string).collect(),
        ))
    } else {
        Err("cannot mix `key=value` and `key` in one /sampling command".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn templates(names: &[&str]) -> Vec<Template> {
        names
            .iter()
            .map(|name| Template {
                name: (*name).into(),
                content: format!("body of {name}"),
            })
            .collect()
    }

    #[test]
    fn plain_prompt_is_not_a_command() {
        assert_eq!(parse_command("hello", &[]), None);
    }

    #[test]
    fn built_ins_parse() {
        let none = &[];
        assert_eq!(parse_command("/resume", none), Some(Ok(Command::Resume)));
        assert_eq!(
            parse_command("/name my-session", none),
            Some(Ok(Command::Name(Some("my-session".into()))))
        );
        assert_eq!(parse_command("/name", none), Some(Ok(Command::Name(None))));
        assert_eq!(
            parse_command("/name   ", none),
            Some(Ok(Command::Name(None)))
        );
        assert_eq!(
            parse_command("/model gpt-x", none),
            Some(Ok(Command::Model(Some("gpt-x".into()))))
        );
        assert_eq!(
            parse_command("/model", none),
            Some(Ok(Command::Model(None)))
        );
        assert_eq!(
            parse_command("/thinking off", none),
            Some(Ok(Command::Thinking(Some("off".into()))))
        );
        assert_eq!(
            parse_command("/provider", none),
            Some(Ok(Command::Provider(None)))
        );
    }

    #[test]
    fn malformed_built_ins_error() {
        assert_eq!(
            parse_command("/resume later", &[]),
            Some(Err("`/resume` takes no arguments".into()))
        );
        assert_eq!(parse_command("/", &[]), Some(Ok(Command::Palette)));
    }

    #[test]
    fn sampling_parses_all_shapes() {
        let none = &[];
        assert_eq!(
            parse_command("/sampling", none),
            Some(Ok(Command::Sampling(SamplingEdit::Show)))
        );
        assert_eq!(
            parse_command("/sampling clear", none),
            Some(Ok(Command::Sampling(SamplingEdit::Clear)))
        );
        assert_eq!(
            parse_command("/sampling temperature=0.7", none),
            Some(Ok(Command::Sampling(SamplingEdit::Set(vec![(
                "temperature".into(),
                "0.7".into()
            )]))))
        );
        assert_eq!(
            parse_command("/sampling temperature=0.7, top_p=0.9", none),
            Some(Ok(Command::Sampling(SamplingEdit::Set(vec![
                ("temperature".into(), "0.7".into()),
                ("top_p".into(), "0.9".into())
            ]))))
        );
        assert_eq!(
            parse_command("/sampling temperature,top_p", none),
            Some(Ok(Command::Sampling(SamplingEdit::Unset(vec![
                "temperature".into(),
                "top_p".into()
            ]))))
        );
        assert!(matches!(
            parse_command("/sampling temperature=0.7,top_p", none),
            Some(Err(message)) if message.contains("cannot mix")
        ));
    }

    #[test]
    fn templates_resolve_by_name() {
        let templates = templates(&["review"]);
        match parse_command("/review", &templates) {
            Some(Ok(Command::Template { template, extra })) => {
                assert_eq!(template.name, "review");
                assert_eq!(template.content, "body of review");
                assert_eq!(extra, None);
            }
            other => panic!("unexpected parse: {other:?}"),
        }
        match parse_command("/review fix this", &templates) {
            Some(Ok(Command::Template { extra, .. })) => {
                assert_eq!(extra.as_deref(), Some("fix this"));
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    #[test]
    fn built_ins_shadow_templates_of_the_same_name() {
        let templates = templates(&["resume"]);
        assert_eq!(
            parse_command("/resume", &templates),
            Some(Ok(Command::Resume))
        );
    }

    #[test]
    fn unknown_commands_error_cleanly() {
        assert!(matches!(
            parse_command("/foo", &[]),
            Some(Err(message)) if message.contains("unknown command `/foo`")
        ));
        assert!(matches!(
            parse_command("/review", &[]),
            Some(Err(message)) if message.contains("unknown command `/review`")
        ));
    }
}
