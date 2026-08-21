//! Argument parsing without external dependencies.
//!
//! Surface for V0 skeleton: `aiu` renders the overview report;
//! `--json` switches the report format. Everything else prints usage.

use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Report { json: bool },
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
    aiu [--json]

OPTIONS:
    --json      Render the report as JSON
    -h, --help  Print help
    -V, --version
";

pub fn parse<I>(args: I) -> Result<Command, ArgsError>
where
    I: IntoIterator<Item = String>,
{
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            other => return Err(ArgsError(format!("unknown argument: {other}\n\n{USAGE}"))),
        }
    }
    Ok(Command::Report { json })
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
    fn help_and_version_win_over_other_flags() {
        assert_eq!(parse(args(&["--json", "--help"])).unwrap(), Command::Help);
        assert_eq!(parse(args(&["-V"])).unwrap(), Command::Version);
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        assert!(parse(args(&["frobnicate"])).is_err());
    }
}
