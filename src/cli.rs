//! Argument parsing without external dependencies.
//!
//! Surface: `aiu` renders the compact overview report, `aiu claude` renders
//! the Claude Code detail view; `--json` switches either to JSON.

use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Report { json: bool },
    Detail { source: String, json: bool },
    Help,
    Version,
}

#[derive(Debug)]
pub struct ArgsError(pub String);

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ArgsError {}

const USAGE: &str = "\
aiu — AI coding subscription usage tracker

USAGE:
    aiu [--json]            compact overview across sources
    aiu claude [--json]     Claude Code detail view (per-window quota + attribution)

OPTIONS:
    --json      Render as JSON
    -h, --help  Print help
    -V, --version
";

pub fn parse<I>(args: I) -> Result<Command, ArgsError>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    let mut json = false;
    let mut positional: Option<String> = None;
    for arg in &args {
        match arg.as_str() {
            "--json" => json = true,
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            other if other.starts_with('-') => {
                return Err(ArgsError(format!("unknown argument: {other}\n\n{USAGE}")))
            }
            _ => {
                if positional.is_some() {
                    return Err(ArgsError(format!("unexpected argument: {arg}\n\n{USAGE}")));
                }
                positional = Some(arg.clone());
            }
        }
    }

    match positional.as_deref() {
        None => Ok(Command::Report { json }),
        Some("claude") => Ok(Command::Detail {
            source: "claude".to_string(),
            json,
        }),
        Some(other) => Err(ArgsError(format!(
            "unknown command: {other}\navailable detail views: claude\n\n{USAGE}"
        ))),
    }
}

pub fn usage() -> &'static str {
    USAGE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_args_means_text_report() {
        assert_eq!(parse(args(&[])).unwrap(), Command::Report { json: false });
    }

    #[test]
    fn json_flag_is_recognized() {
        assert_eq!(
            parse(args(&["--json"])).unwrap(),
            Command::Report { json: true }
        );
    }

    #[test]
    fn claude_detail_with_and_without_json() {
        assert_eq!(
            parse(args(&["claude"])).unwrap(),
            Command::Detail {
                source: "claude".to_string(),
                json: false
            }
        );
        assert_eq!(
            parse(args(&["claude", "--json"])).unwrap(),
            Command::Detail {
                source: "claude".to_string(),
                json: true
            }
        );
        assert_eq!(
            parse(args(&["--json", "claude"])).unwrap(),
            Command::Detail {
                source: "claude".to_string(),
                json: true
            }
        );
    }

    #[test]
    fn help_and_version_win_over_other_flags() {
        assert_eq!(parse(args(&["--json", "--help"])).unwrap(), Command::Help);
        assert_eq!(parse(args(&["-V"])).unwrap(), Command::Version);
        assert_eq!(parse(args(&["claude", "-V"])).unwrap(), Command::Version);
    }

    #[test]
    fn unknown_commands_and_arguments_are_rejected() {
        assert!(parse(args(&["frobnicate"])).is_err());
        assert!(parse(args(&["--wat"])).is_err());
        assert!(parse(args(&["claude", "extra"])).is_err());
    }
}
