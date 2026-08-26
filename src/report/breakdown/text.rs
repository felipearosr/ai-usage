//! Plain-text rendering of a [`SourceBreakdown`]: the `models` matrix and the
//! `machines` per-machine model list.

use crate::report::breakdown::{SourceBreakdown, WindowBreakdown};
use crate::report::detail::VendorQuota;
use crate::report::text::humanize_tokens;
use crate::utc;

/// Width reserved for percentage cells ("100.0%").
const PCT_WIDTH: usize = 6;

pub fn render_models(breakdown: &SourceBreakdown) -> String {
    let mut out = String::new();
    out.push_str(&breakdown.source);
    out.push('\n');

    if breakdown.windows.is_empty() {
        out.push('\n');
        out.push_str(&empty_state(breakdown.has_usage));
        return out;
    }

    for window in &breakdown.windows {
        render_matrix_window(&mut out, window, breakdown.generated_at_epoch);
        out.push('\n');
    }
    out
}

pub fn render_machines(breakdown: &SourceBreakdown) -> String {
    let mut out = String::new();
    out.push_str(&breakdown.source);
    out.push('\n');

    if breakdown.windows.is_empty() {
        out.push('\n');
        out.push_str(&empty_state(breakdown.has_usage));
        return out;
    }

    for window in &breakdown.windows {
        render_machines_window(&mut out, window, breakdown.generated_at_epoch);
        out.push('\n');
    }
    out
}

fn empty_state(has_usage: bool) -> String {
    if has_usage {
        // Usage exists but the vendor has not shown us any quota window.
        "no vendor snapshot yet — aiu has recorded usage, but no quota window is known\n"
            .to_string()
    } else {
        "no usage recorded yet — run `aiu init` to import history\n".to_string()
    }
}

fn render_matrix_window(out: &mut String, window: &WindowBreakdown, now: u64) {
    out.push_str(&format!("[{}]\n", window.window));
    out.push_str(&vendor_line(&window.vendor, now));
    out.push('\n');

    if window.matrix.is_empty() {
        out.push_str("aiu attribution: no usage recorded in this window\n");
        return;
    }

    out.push_str("machine × model matrix (share of window output tokens):\n");
    render_matrix_table(out, &window.matrix);
}

fn render_machines_window(out: &mut String, window: &WindowBreakdown, now: u64) {
    out.push_str(&format!("[{}]\n", window.window));
    out.push_str(&vendor_line(&window.vendor, now));
    out.push('\n');

    if window.matrix.is_empty() {
        out.push_str("aiu attribution: no usage recorded in this window\n");
        return;
    }

    out.push_str(&format!(
        "aiu attribution (last {}, output tokens — aiu's local observation, distinct from the vendor number)\n",
        window.window
    ));
    for (j, share) in window.matrix.machine_shares().iter().enumerate() {
        out.push_str(&format!(
            "  {:<18} {:>8}  {:>5.1}%\n",
            share.name,
            humanize_tokens(share.output_tokens),
            share.share_percent
        ));
        for model in window.matrix.models_for_machine(j) {
            out.push_str(&format!(
                "    {:<16} {:>8}  {:>5.1}%\n",
                model.name,
                humanize_tokens(model.output_tokens),
                model.share_percent
            ));
        }
    }
}

fn render_matrix_table(out: &mut String, matrix: &crate::report::breakdown::Matrix) {
    let grand = matrix.grand_total();
    let label_width = matrix
        .models
        .iter()
        .map(|s| s.len())
        .max()
        .unwrap_or(0)
        .max(2);
    let col_widths: Vec<usize> = matrix
        .machines
        .iter()
        .map(|m| m.len().max(PCT_WIDTH))
        .collect();
    let total_width = "total".len().max(PCT_WIDTH);

    // Header: machine names across the top, "total" column on the right.
    out.push_str(&format!("  {:<label_width$}  ", ""));
    for (j, machine) in matrix.machines.iter().enumerate() {
        out.push_str(&format!("{:>width$}  ", machine, width = col_widths[j]));
    }
    out.push_str(&format!("{:>total_width$}\n", "total"));

    // Model rows: each cell is the share of the whole window.
    for (i, model) in matrix.models.iter().enumerate() {
        out.push_str(&format!("  {:<label_width$}  ", model));
        for (j, _machine) in matrix.machines.iter().enumerate() {
            let pct = crate::report::detail::share_percent(matrix.cells[i][j], grand);
            out.push_str(&format!(
                "{:>width$}  ",
                format!("{pct:.1}%"),
                width = col_widths[j]
            ));
        }
        let row_total = crate::report::detail::share_percent(matrix.model_total(i), grand);
        out.push_str(&format!("{:>total_width$}\n", format!("{row_total:.1}%")));
    }

    // Machine totals row: column totals are machine shares.
    out.push_str(&format!("  {:<label_width$}  ", "total"));
    for (j, width) in col_widths.iter().enumerate() {
        let pct = crate::report::detail::share_percent(matrix.machine_total(j), grand);
        out.push_str(&format!(
            "{:>width$}  ",
            format!("{pct:.1}%"),
            width = width
        ));
    }
    out.push_str(&format!("{:>total_width$}\n", "100.0%"));
}

fn vendor_line(vendor: &Option<VendorQuota>, now: u64) -> String {
    match vendor {
        Some(vendor) => {
            let mut line = format!("vendor quota: {:.1}% used", vendor.used_percent);
            if let Some(secs) = vendor.resets_in_secs(now) {
                line.push_str(&format!(
                    " · resets in {}",
                    utc::humanize_duration_secs(secs)
                ));
            }
            line
        }
        // Missing vendor data is an explicit gap, never zero.
        None => "vendor quota: no vendor snapshot yet".to_string(),
    }
}
