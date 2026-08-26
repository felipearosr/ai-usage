//! JSON rendering of a [`SourceDetail`] (`aiu claude --json`).

use serde_json::json;

use crate::report::detail::{SourceDetail, VendorQuota, WindowDetail};

pub fn render(detail: &SourceDetail) -> String {
    let windows = detail
        .windows
        .iter()
        .map(|w| window_json(w, detail.generated_at_epoch))
        .collect::<Vec<_>>();

    let doc = json!({
        "source": detail.source,
        "generated_at": crate::utc::format_epoch(detail.generated_at_epoch),
        "has_usage": detail.has_usage,
        "windows": windows,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
}

/// Vendor block for one window, shared with the breakdown renderers. Absent
/// vendor data is null, never 0%.
pub(crate) fn vendor_json(vendor: &Option<VendorQuota>, now: u64) -> serde_json::Value {
    match vendor {
        Some(vendor) => json!({
            "used_percent": vendor.used_percent,
            "resets_at": vendor.resets_at_utc,
            "resets_in_secs": vendor.resets_in_secs(now),
        }),
        None => serde_json::Value::Null,
    }
}

fn window_json(window: &WindowDetail, now: u64) -> serde_json::Value {
    let vendor = vendor_json(&window.vendor, now);

    let total: i64 = window.machines.iter().map(|m| m.output_tokens).sum();
    json!({
        "window": window.window,
        "vendor": vendor,
        "attribution": {
            "total_output_tokens": total,
            "machines": shares_json(&window.machines),
            "models": shares_json(&window.models),
        },
    })
}

fn shares_json(shares: &[crate::report::detail::Share]) -> Vec<serde_json::Value> {
    shares
        .iter()
        .map(|share| {
            json!({
                "name": share.name,
                "output_tokens": share.output_tokens,
                "share_percent": share.share_percent,
            })
        })
        .collect()
}
