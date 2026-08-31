//! Reading and writing the scheduler's unit files.
//!
//! Split from the scheduler proper because this half changes when a *file
//! format* changes, while the rest changes when *scheduling* does. Every
//! escaper here has a matching parser, and the pair is covered by round-trip
//! tests: a lossy read would report drift that is not real.

use std::path::PathBuf;

use super::{is_capturable, ScheduleSpec, LAUNCHD_LABEL, SYSTEMD_SERVICE};

/// The systemd unit that performs one collection and exits.
pub fn render_systemd_service(spec: &ScheduleSpec) -> String {
    let environment: String = spec
        .environment
        .iter()
        .filter(|(_, value)| is_capturable(value))
        .map(|(key, value)| format!("Environment=\"{key}={}\"\n", escape_systemd(value)))
        .collect();
    format!(
        "[Unit]\n\
         Description=aiu — collect AI coding usage\n\
         Documentation=https://github.com/felipearosr/ai-usage\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         {environment}\
         ExecStart=\"{}\" collect\n",
        escape_systemd(&spec.exe.display().to_string())
    )
}

/// Escapes a value for a double-quoted systemd `Environment=` assignment.
///
/// `%` starts a specifier that systemd expands; an unresolvable one makes it
/// drop the whole assignment, so a literal percent must be doubled.
fn escape_systemd(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
}

/// Escapes text for an XML character-data position in the plist.
fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The timer that starts the service on the scheduled interval.
pub fn render_systemd_timer(spec: &ScheduleSpec) -> String {
    format!(
        "[Unit]\n\
         Description=aiu — collect AI coding usage every {minutes} minutes\n\
         \n\
         [Timer]\n\
         OnCalendar=*:0/{minutes}\n\
         AccuracySec=1min\n\
         Persistent=true\n\
         Unit={service}\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n",
        minutes = spec.interval_minutes,
        service = SYSTEMD_SERVICE,
    )
}

/// The launchd agent. `StartInterval` re-runs a program that has exited, so
/// there is no `KeepAlive` and nothing stays resident.
pub fn render_launchd_plist(spec: &ScheduleSpec) -> String {
    let environment = if !spec.environment.iter().any(|(_, v)| is_capturable(v)) {
        String::new()
    } else {
        let entries: String = spec
            .environment
            .iter()
            .filter(|(_, value)| is_capturable(value))
            .map(|(key, value)| {
                format!(
                    "        <key>{}</key>\n        <string>{}</string>\n",
                    escape_xml(key),
                    escape_xml(value)
                )
            })
            .collect();
        format!("    <key>EnvironmentVariables</key>\n    <dict>\n{entries}    </dict>\n")
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n    \
             <key>Label</key>\n    \
             <string>{label}</string>\n    \
             <key>ProgramArguments</key>\n    \
             <array>\n        \
                 <string>{exe}</string>\n        \
                 <string>collect</string>\n    \
             </array>\n    \
             <key>StartInterval</key>\n    \
             <integer>{seconds}</integer>\n    \
             <key>RunAtLoad</key>\n    \
             <false/>\n\
         {environment}\
         </dict>\n\
         </plist>\n",
        label = LAUNCHD_LABEL,
        exe = escape_xml(&spec.exe.display().to_string()),
        seconds = spec.interval_seconds(),
    )
}

/// The parts of a schedule that live inside the unit files, as opposed to
/// the platform and paths the caller already knows.
pub(super) struct ParsedUnit {
    pub(super) exe: PathBuf,
    pub(super) interval_minutes: u64,
    pub(super) environment: Vec<(String, String)>,
}

pub(super) fn parse_systemd(service: &str, timer: &str) -> Option<ParsedUnit> {
    let exec = service
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart="))?;
    // Rendered as `"<escaped path>" collect`.
    let quoted = exec.strip_prefix('"')?;
    let end = find_closing_quote(quoted)?;
    let exe = PathBuf::from(unescape_systemd(&quoted[..end]));

    let interval_minutes = timer
        .lines()
        .find_map(|line| line.strip_prefix("OnCalendar=*:0/"))?
        .trim()
        .parse()
        .ok()?;

    let environment = service
        .lines()
        .filter_map(|line| line.strip_prefix("Environment=\""))
        .filter_map(|rest| {
            let end = find_closing_quote(rest)?;
            let assignment = unescape_systemd(&rest[..end]);
            let (key, value) = assignment.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect();

    Some(ParsedUnit {
        exe,
        interval_minutes,
        environment,
    })
}

/// The offset of the quote that closes a systemd double-quoted value,
/// skipping any that is backslash-escaped.
fn find_closing_quote(value: &str) -> Option<usize> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Some(index),
            _ => index += 1,
        }
    }
    None
}

/// Inverse of [`escape_systemd`]. `%%` collapses first so that the backslash
/// pass cannot be fed percent-escapes it should not see.
fn unescape_systemd(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '%' if chars.peek() == Some(&'%') => {
                chars.next();
                out.push('%');
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            other => out.push(other),
        }
    }
    out
}

pub(super) fn parse_launchd(plist: &str) -> Option<ParsedUnit> {
    let program_arguments = between(plist, "<key>ProgramArguments</key>", "</array>")?;
    let exe = PathBuf::from(unescape_xml(&first_string(program_arguments)?));

    let interval_seconds: u64 = between(plist, "<key>StartInterval</key>", "</integer>")?
        .split("<integer>")
        .nth(1)?
        .trim()
        .parse()
        .ok()?;
    // aiu only ever writes whole minutes. Anything else is hand-edited into a
    // schedule this cannot describe, and truncating it to 0 minutes would
    // report a drift that misstates what is actually installed.
    if interval_seconds == 0 || !interval_seconds.is_multiple_of(60) {
        return None;
    }

    let environment = match between(plist, "<key>EnvironmentVariables</key>", "</dict>") {
        Some(block) => parse_plist_dict(block),
        None => Vec::new(),
    };

    Some(ParsedUnit {
        exe,
        interval_minutes: interval_seconds / 60,
        environment,
    })
}

/// The text between the first occurrence of `start` and the next `end`.
fn between<'a>(haystack: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let rest = &haystack[haystack.find(start)? + start.len()..];
    Some(&rest[..rest.find(end)?])
}

fn first_string(block: &str) -> Option<String> {
    Some(between(block, "<string>", "</string>")?.to_string())
}

/// Reads `<key>`/`<string>` pairs in document order.
fn parse_plist_dict(block: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut rest = block;
    while let Some(key_start) = rest.find("<key>") {
        rest = &rest[key_start + "<key>".len()..];
        let Some(key_end) = rest.find("</key>") else {
            break;
        };
        let key = unescape_xml(&rest[..key_end]);
        rest = &rest[key_end..];
        let Some(value) = between(rest, "<string>", "</string>") else {
            break;
        };
        let value = unescape_xml(value);
        rest = &rest[rest.find("</string>").unwrap_or(rest.len())..];
        pairs.push((key, value));
    }
    pairs
}

/// Inverse of [`escape_xml`]. `&amp;` is expanded last so an escaped
/// ampersand cannot be re-read as the start of another entity.
fn unescape_xml(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}
