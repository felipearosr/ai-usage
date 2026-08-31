//! Plain-text rendering of a [`SourceDetail`].

use crate::report::detail::{SourceDetail, VendorQuota, WindowDetail};
use crate::report::text::humanize_tokens;
use crate::utc;

pub fn render(detail: &SourceDetail) -> String {
    let mut out = String::new();
    out.push_str(&detail.source);
    out.push('\n');

    if detail.windows.is_empty() {
        out.push('\n');
        out.push_str(&empty_state(detail.has_usage));
        return out;
    }

    for window in &detail.windows {
        render_window(&mut out, window, detail.generated_at_epoch);
        out.push('\n');
    }
    out
}

/// The message rendered when no vendor window is known: an explicit gap when
/// usage exists, an init hint when nothing has been recorded. Shared with the
/// breakdown renderers so both output formats stay identical.
pub(crate) fn empty_state(has_usage: bool) -> String {
    if has_usage {
        // Usage exists but the vendor has not shown us any quota window:
        // an explicit gap, never zero and never silence.
        "no vendor snapshot yet — aiu has recorded usage, but no quota window is known\n"
            .to_string()
    } else {
        "no usage recorded yet — run `aiu init` to import history\n".to_string()
    }
}

/// The vendor-quota line ("vendor quota: 42.5% used · resets in 1h 14m"),
/// shared with the breakdown renderers. Missing data is an explicit gap.
pub(crate) fn vendor_line(vendor: &Option<VendorQuota>, now: u64) -> String {
    match vendor {
        Some(vendor) => {
            let mut line = format!("vendor quota: {:.1}% used", vendor.used_percent);
            if let Some(secs) = vendor.resets_in_secs(now) {
                line.push_str(&format!(
                    " · resets in {}",
                    utc::humanize_duration_secs(secs)
                ));
            }
            if vendor.is_stale(now) {
                line.push_str(" · STALE");
            }
            let observed = vendor
                .observation_age_secs(now)
                .map(crate::report::humanize_sync_age)
                .unwrap_or_else(|| "at an invalid time".to_string());
            line.push_str(&format!(
                " · observed {observed} by {}",
                vendor.observing_device_name
            ));
            if vendor.observer_last_sync_at_utc.is_none() {
                line.push_str(" (device never synced)");
            }
            line
        }
        None => "vendor quota: no vendor snapshot yet".to_string(),
    }
}

fn render_window(out: &mut String, window: &WindowDetail, now: u64) {
    out.push_str(&format!("[{}]\n", window.window));
    out.push_str(&vendor_line(&window.vendor, now));
    out.push('\n');

    if window.machines.is_empty() && window.models.is_empty() {
        out.push_str("aiu attribution: no usage recorded in this window\n");
        return;
    }

    out.push_str(&format!(
        "aiu attribution (last {}, output tokens — aiu's local observation, distinct from the vendor number)\n",
        window.window
    ));
    if !window.machines.is_empty() {
        out.push_str("  by machine\n");
        for share in &window.machines {
            let name = crate::report::machine_freshness_label(
                &share.name,
                share.last_sync_at_utc.as_deref(),
                now,
            );
            out.push_str(&format!(
                "    {:<18} {:>8}  {:>5.1}%\n",
                name,
                humanize_tokens(share.output_tokens),
                share.share_percent
            ));
        }
    }
    if !window.models.is_empty() {
        out.push_str("  by model\n");
        for share in &window.models {
            out.push_str(&format!(
                "    {:<18} {:>8}  {:>5.1}%\n",
                share.name,
                humanize_tokens(share.output_tokens),
                share.share_percent
            ));
        }
    }
}
