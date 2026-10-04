//! Command-line argument parsing (`clap`).

use std::collections::BTreeMap;

use clap::Parser;

use crate::settings::SamplingParams;

/// An AI coding agent for the terminal.
#[derive(Debug, Parser)]
#[command(name = "clanky", version, about)]
pub struct Cli {
    /// Print mode: run one turn non-interactively and print the response.
    #[arg(short = 'p', long = "print")]
    pub print: bool,

    /// Provider to use (overrides settings).
    #[arg(long)]
    pub provider: Option<String>,

    /// Model to use (overrides settings).
    #[arg(long)]
    pub model: Option<String>,

    /// Thinking budget or level (overrides settings).
    #[arg(long)]
    pub thinking: Option<String>,

    /// Sampling parameters as comma-separated key=value pairs,
    /// e.g. --sampling "temperature=0.7,top_p=0.9" (overrides settings).
    #[arg(long, value_parser = parse_sampling)]
    pub sampling: Option<SamplingParams>,

    /// Resume a saved session (interactive mode only). With a NAME, load
    /// it directly; without, show a picker of saved sessions.
    #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
    pub resume: Option<String>,

    /// Prompt text. Multiple words are joined with spaces;
    /// piped stdin is picked up in M1.
    #[arg(trailing_var_arg = true)]
    pub prompt: Vec<String>,
}

impl Cli {
    /// CLI flags expressed as a `Settings` overlay (unset flags are `None`
    /// and therefore do not clobber file-based settings).
    pub fn overrides(&self) -> super::settings::Settings {
        let prompt = if self.prompt.is_empty() {
            None
        } else {
            Some(self.prompt.join(" "))
        };
        super::settings::Settings {
            provider: self.provider.clone(),
            model: self.model.clone(),
            thinking: self.thinking.clone(),
            sampling: self.sampling.clone(),
            prompt,
        }
    }
}

/// Parse `key=value,key=value` into an ordered map of strings.
fn parse_sampling(raw: &str) -> Result<SamplingParams, String> {
    let mut params = BTreeMap::new();
    for pair in raw.split(',') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("invalid sampling parameter `{pair}`: expected `key=value`"))?;
        if key.is_empty() {
            return Err(format!("invalid sampling parameter `{pair}`: empty key"));
        }
        params.insert(key.to_string(), value.to_string());
    }
    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sampling(pairs: &[(&str, &str)]) -> SamplingParams {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn sampling_parses_pairs() {
        let parsed = parse_sampling("temperature=0.7,top_p=0.9,max_tokens=512").unwrap();
        assert_eq!(
            parsed,
            sampling(&[
                ("temperature", "0.7"),
                ("top_p", "0.9"),
                ("max_tokens", "512"),
            ])
        );
    }

    #[test]
    fn sampling_rejects_missing_equals() {
        assert!(parse_sampling("temperature").is_err());
    }

    #[test]
    fn sampling_rejects_empty_key() {
        assert!(parse_sampling("=0.7").is_err());
    }

    #[test]
    fn sampling_allows_empty_value_and_empty_input() {
        assert_eq!(parse_sampling("").unwrap().len(), 0);
        assert_eq!(parse_sampling("reasoning=").unwrap()["reasoning"], "");
    }

    fn cli_with(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("clanky").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn prompt_args_are_joined() {
        let cli = cli_with(&["-p", "say", "hi", "there"]);
        assert!(cli.print);
        assert_eq!(cli.overrides().prompt.as_deref(), Some("say hi there"));
    }

    #[test]
    fn overrides_are_none_when_flags_absent() {
        let cli = cli_with(&["-p", "hi"]);
        let over = cli.overrides();
        assert!(over.provider.is_none());
        assert!(over.model.is_none());
        assert!(over.thinking.is_none());
        assert!(over.sampling.is_none());
        assert!(over.prompt.is_some());
    }

    #[test]
    fn help_works() {
        // `--help` must parse and short-circuit with exit success.
        let result = Cli::try_parse_from(["clanky", "--help"]);
        assert!(result.is_err()); // clap signals help via a DisplayHelp "error"
        assert_eq!(result.unwrap_err().exit_code(), 0, "--help must exit 0");
    }
}
