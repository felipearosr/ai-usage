//! Argument parsing without external dependencies.
//!
//! Surface: `aiu` renders the compact overview report, `aiu <source>` renders
//! a per-source detail view, `aiu <source> models` the machine × exact-model
//! matrix, `aiu <source> machines` machine shares plus per-machine models;
//! `aiu sources` lists detection + overrides, with `detect` and
//! `enable|disable|auto <source>` subcommands. `--json` switches any of them
//! to JSON.

use std::fmt;

use crate::store::SourceMode;

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
    Sources {
        json: bool,
    },
    SourcesDetect {
        json: bool,
    },
    SourcesSet {
        mode: SourceMode,
        source: String,
        json: bool,
    },
    Machines {
        json: bool,
    },
    MachineRename {
        device: String,
        name: String,
    },
    MachineRemove {
        device: String,
    },
    Init,
    Join {
        code: String,
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
    aiu sources [--json]            list detection + per-source overrides
    aiu sources detect [--json]     re-run source detection
    aiu sources <mode> <source>     enable | disable | auto a source
    aiu machines [--json]           global machine health and participation
    aiu machine rename <device> <name>
                                    rename a machine across the workspace
    aiu machine remove <device>     revoke a machine without deleting history
    aiu init                        create a workspace and pairing code
    aiu join <code>                 join an existing workspace

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

fn mode_arg(word: &str) -> Option<SourceMode> {
    match word {
        "auto" => Some(SourceMode::Auto),
        "enable" => Some(SourceMode::Enabled),
        "disable" => Some(SourceMode::Disabled),
        _ => None,
    }
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
        [command] if command == "init" && !json => Ok(Command::Init),
        [command, code] if command == "join" && !json => Ok(Command::Join { code: code.clone() }),
        [command, ..] if command == "init" || command == "join" => Err(ArgsError(format!(
            "invalid setup command\nusage: aiu init | aiu join <code>\n\n{USAGE}"
        ))),
        [s] if s == "sources" => Ok(Command::Sources { json }),
        [s, sub] if s == "sources" && sub == "detect" => Ok(Command::SourcesDetect { json }),
        [s, mode, source] if s == "sources" && is_source(source) => {
            match mode_arg(mode) {
                Some(mode) => Ok(Command::SourcesSet {
                    mode,
                    source: source.clone(),
                    json,
                }),
                None => Err(ArgsError(format!(
                    "unknown sources command: {mode}\navailable: detect, enable, disable, auto\n\n{USAGE}"
                ))),
            }
        }
        [s, ..] if s == "sources" => Err(ArgsError(format!(
            "unknown sources command\navailable: detect, enable <source>, disable <source>, auto <source>\n\n{USAGE}"
        ))),
        [command] if command == "machines" => Ok(Command::Machines { json }),
        [command, action, device, name]
            if command == "machine" && action == "rename" && !json =>
        {
            Ok(Command::MachineRename {
                device: device.clone(),
                name: name.clone(),
            })
        }
        [command, action, device]
            if command == "machine" && action == "remove" && !json =>
        {
            Ok(Command::MachineRemove {
                device: device.clone(),
            })
        }
        [command, ..] if command == "machine" || command == "machines" => Err(ArgsError(
            format!(
                "invalid machine command\nusage: aiu machines [--json] | aiu machine rename <device> <name> | aiu machine remove <device>\n\n{USAGE}"
            ),
        )),
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
    fn fleet_commands_are_recognized() {
        assert_eq!(
            parse(args(&["machines", "--json"])).unwrap(),
            Command::Machines { json: true }
        );
        assert_eq!(
            parse(args(&["machine", "rename", "device-a", "studio"])).unwrap(),
            Command::MachineRename {
                device: "device-a".into(),
                name: "studio".into(),
            }
        );
        assert_eq!(
            parse(args(&["machine", "remove", "device-a"])).unwrap(),
            Command::MachineRemove {
                device: "device-a".into(),
            }
        );
        assert!(parse(args(&["machine", "remove", "device-a", "--json"])).is_err());
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

    #[test]
    fn sources_commands_parse() {
        assert_eq!(
            parse(args(&["sources"])).unwrap(),
            Command::Sources { json: false }
        );
        assert_eq!(
            parse(args(&["sources", "--json"])).unwrap(),
            Command::Sources { json: true }
        );
        assert_eq!(
            parse(args(&["sources", "detect"])).unwrap(),
            Command::SourcesDetect { json: false }
        );
        for (word, mode) in [
            ("enable", SourceMode::Enabled),
            ("disable", SourceMode::Disabled),
            ("auto", SourceMode::Auto),
        ] {
            for source in SOURCES {
                assert_eq!(
                    parse(args(&["sources", word, source])).unwrap(),
                    Command::SourcesSet {
                        mode,
                        source: source.to_string(),
                        json: false,
                    }
                );
            }
        }
    }

    #[test]
    fn init_and_join_commands_parse() {
        assert_eq!(parse(args(&["init"])).unwrap(), Command::Init);
        assert_eq!(
            parse(args(&["join", "abc-def"])).unwrap(),
            Command::Join {
                code: "abc-def".to_string()
            }
        );
        assert!(parse(args(&["join"])).is_err());
        assert!(parse(args(&["init", "extra"])).is_err());
        assert!(parse(args(&["init", "--json"])).is_err());
    }

    #[test]
    fn sources_rejects_bad_arguments() {
        assert!(parse(args(&["sources", "frobnicate"])).is_err());
        assert!(
            parse(args(&["sources", "enable"])).is_err(),
            "missing source"
        );
        assert!(
            parse(args(&["sources", "enable", "wat"])).is_err(),
            "unknown source"
        );
        assert!(parse(args(&["sources", "detect", "extra"])).is_err());
    }
}
