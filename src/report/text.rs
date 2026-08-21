//! Plain-text rendering of a [`Report`].

use crate::report::Report;
use crate::utc;

pub fn render(report: &Report) -> String {
    let mut out = String::new();
    out.push_str("aiu\n\n");

    if report.sources.is_empty() && report.devices.is_empty() {
        out.push_str("No usage recorded yet.\n");
        out.push_str("Run `aiu init` to detect installed tools and import history.\n");
        return out;
    }

    for source in &report.sources {
        render_source(&mut out, source, report.generated_at_epoch);
        out.push('\n');
    }

    if !report.devices.is_empty() {
        out.push_str("devices\n");
        for device in &report.devices {
            let age = device.age_secs(report.generated_at_epoch);
            let detail = match age {
                Some(secs) => format!("synced {} ago", utc::humanize_duration_secs(secs)),
                None => "never synced".to_string(),
            };
            if device.is_stale(report.generated_at_epoch) {
                out.push_str(&format!("  {:<10} STALE · {detail}\n", device.name));
            } else {
                out.push_str(&format!("  {:<10} {detail}\n", device.name));
            }
        }
    }

    out
}

fn render_source(out: &mut String, source: &crate::report::SourceReport, now: u64) {
    out.push_str(&source.source);
    out.push('\n');

    if source.windows.is_empty() && source.top_model.is_none() && source.top_machine.is_none() {
        out.push_str("  no data yet\n");
        return;
    }

    for window in &source.windows {
        let mut line = format!("  {:<8} {:.1}% used", window.window, window.used_percent);
        if let Some(secs) = window.resets_in_secs(now) {
            line.push_str(&format!(
                " · resets in {}",
                utc::humanize_duration_secs(secs)
            ));
        }
        out.push_str(&line);
        out.push('\n');
    }

    if source.windows.is_empty() && (source.top_model.is_some() || source.top_machine.is_some()) {
        // Missing data is rendered as an explicit gap, never as zero.
        out.push_str("  quota     no vendor snapshot yet\n");
    }

    if let Some(model) = &source.top_model {
        out.push_str(&format!(
            "  top model    {} ({} out tok, all-time)\n",
            model.name,
            humanize_tokens(model.output_tokens)
        ));
    }
    if let Some(machine) = &source.top_machine {
        out.push_str(&format!(
            "  top machine  {} ({} out tok, all-time)\n",
            machine.name,
            humanize_tokens(machine.output_tokens)
        ));
    }
}

/// Formats token counts compactly ("940", "12.3k", "4.56M").
pub fn humanize_tokens(tokens: i64) -> String {
    let t = tokens.max(0) as f64;
    if t < 1_000.0 {
        return format!("{}", tokens.max(0));
    }
    if t < 1_000_000.0 {
        return format!("{:.1}k", t / 1_000.0);
    }
    format!("{:.2}M", t / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::STALE_AFTER_SECS;

    #[test]
    fn humanizes_token_counts() {
        assert_eq!(humanize_tokens(940), "940");
        assert_eq!(humanize_tokens(12_300), "12.3k");
        assert_eq!(humanize_tokens(4_560_000), "4.56M");
        assert_eq!(humanize_tokens(0), "0");
    }

    #[test]
    fn empty_report_prints_init_hint() {
        let report = Report {
            generated_at_epoch: 0,
            sources: vec![],
            devices: vec![],
        };
        let text = render(&report);
        assert!(text.contains("No usage recorded yet."));
        assert!(text.contains("aiu init"));
        assert!(!text.contains("STALE"));
    }

    #[test]
    fn stale_threshold_is_thirty_minutes() {
        assert_eq!(STALE_AFTER_SECS, 30 * 60);
    }
}
