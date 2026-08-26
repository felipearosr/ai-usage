//! Argument parsing without external dependencies.
//!
//! Surface: `aiu` renders the compact overview report, `aiu <source>` renders
//! a per-source detail view, `aiu <source> models` the machine × exact-model
//! matrix, `aiu <source> machines` machine shares plus per-machine models;
//! `--json` switches any of them to JSON.

use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Report {
        json: bool,
    },
    Detail {
        source: String,
        json: bool,
    },
    Breakdown {
        source: String,
        kind: BreakdownKind,
        json: bool,
    },
    Help,
    Version,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum BreakdownKind {
    Models,
    Machines,
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
    aiu [--json]                    compact overview across sources
    aiu <source> [--json]           per-source detail view (quota + attribution)
    aiu <source> models [--json]    machine × exact-model matrix per window
    aiu <source> machines [--json]  machine shares + per-machine model list

SOURCES:
    claude    Claude Code
    codex     Codex
    go        OpenCode Go

OPTIONS:
    --json      Render as JSON
    -h, --help  Print help
    -V, --version
";

const SOURCES: [&str; 3] = ["claude", "codex", "go"];

fn is_source(name: &str) -> bool {
    SOURCES.contains(&name)
}

pub fn parse<I>(args: I) -> Result<Command, ArgsError>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    let mut json = false;
    let mut positionals: Vec<String> = Vec::new();
    for arg in &args {
        match arg.as_str() {
            "--json" => json = true,
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            other if other.starts_with('-') => {
                return Err(ArgsError(format!("unknown argument: {other}\n\n{USAGE}")))
            }
            _ => positionals.push(arg.clone()),
        }
    }

    match positionals.as_slice() {
        [] => Ok(Command::Report { json }),
        [source] if is_source(source) => Ok(Command::Detail {
            source: source.clone(),
            json,
        }),
        [source] => Err(ArgsError(format!(
            "unknown command: {source}\navailable detail views: claude, codex, go\n\n{USAGE}"
        ))),
        [source, sub] if is_source(source) => {
            let kind = match sub.as_str() {
                "models" => BreakdownKind::Models,
                "machines" => BreakdownKind::Machines,
                other => {
                    return Err(ArgsError(format!(
                    "unknown command: {other}\navailable breakdowns: models, machines\n\n{USAGE}"
                )))
                }
            };
            Ok(Command::Breakdown {
                source: source.clone(),
                kind,
                json,
            })
        }
        [source, ..] => Err(ArgsError(format!(
            "unknown command: {source}\navailable detail views: claude, codex, go\n\n{USAGE}"
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
    fn codex_detail_is_recognized() {
        assert_eq!(
            parse(args(&["codex"])).unwrap(),
            Command::Detail {
                source: "codex".to_string(),
                json: false
            }
        );
        assert_eq!(
            parse(args(&["codex", "--json"])).unwrap(),
            Command::Detail {
                source: "codex".to_string(),
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

    #[test]
    fn go_source_is_recognized_as_a_detail_view() {
        assert_eq!(
            parse(args(&["go"])).unwrap(),
            Command::Detail {
                source: "go".to_string(),
                json: false
            }
        );
    }

    #[test]
    fn models_and_machines_breakdowns_parse_for_every_source() {
        for source in ["claude", "codex", "go"] {
            assert_eq!(
                parse(args(&[source, "models"])).unwrap(),
                Command::Breakdown {
                    source: source.to_string(),
                    kind: BreakdownKind::Models,
                    json: false,
                }
            );
            assert_eq!(
                parse(args(&[source, "machines", "--json"])).unwrap(),
                Command::Breakdown {
                    source: source.to_string(),
                    kind: BreakdownKind::Machines,
                    json: true,
                }
            );
        }
    }

    #[test]
    fn breakdown_rejects_unknown_subcommand() {
        assert!(parse(args(&["claude", "frobnicate"])).is_err());
        assert!(parse(args(&["claude", "models", "extra"])).is_err());
    }
}
