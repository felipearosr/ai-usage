//! Plain-text rendering of a [`SourceDetail`].

use crate::report::detail::{SourceDetail, WindowDetail};
use crate::report::text::humanize_tokens;
use crate::utc;

pub fn render(detail: &SourceDetail) -> String {
    let mut out = String::new();
    out.push_str(&detail.source);
    out.push('\n');

    if detail.windows.is_empty() {
        out.push('\n');
        if detail.has_usage {
            // Usage exists but the vendor has not shown us any quota window:
            // an explicit gap, never zero and never silence.
            out.push_str(
                "no vendor snapshot yet — aiu has recorded usage, but no quota window is known\n",
            );
        } else {
            out.push_str("no usage recorded yet — run `aiu init` to import history\n");
        }
        return out;
    }

    for window in &detail.windows {
        render_window(&mut out, window, detail.generated_at_epoch);
        out.push('\n');
    }
    out
}

fn render_window(out: &mut String, window: &WindowDetail, now: u64) {
    out.push_str(&format!("[{}]\n", window.window));

    match &window.vendor {
        Some(vendor) => {
            let mut line = format!("vendor quota: {:.1}% used", vendor.used_percent);
            if let Some(secs) = vendor.resets_in_secs(now) {
                line.push_str(&format!(
                    " · resets in {}",
                    utc::humanize_duration_secs(secs)
                ));
            }
            out.push_str(&line);
            out.push('\n');
        }
        None => {
            // Missing vendor data is an explicit gap, never zero.
            out.push_str("vendor quota: no vendor snapshot yet\n");
        }
    }

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
            out.push_str(&format!(
                "    {:<18} {:>8}  {:>5.1}%\n",
                share.name,
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
