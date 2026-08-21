//! JSON rendering of a [`Report`] (`--json` on report commands).

use serde_json::json;

use crate::report::Report;

pub fn render(report: &Report) -> String {
    let sources = report
        .sources
        .iter()
        .map(|source| {
            json!({
                "source": source.source,
                "windows": source.windows.iter().map(|w| {
                    json!({
                        "window": w.window,
                        "used_percent": w.used_percent,
                        "resets_at": w.resets_at_utc,
                        "resets_in_secs": w.resets_in_secs(report.generated_at_epoch),
                    })
                }).collect::<Vec<_>>(),
                "top_model": attribution_json(&source.top_model),
                "top_machine": attribution_json(&source.top_machine),
            })
        })
        .collect::<Vec<_>>();

    let devices = report
        .devices
        .iter()
        .map(|device| {
            json!({
                "name": device.name,
                "last_sync_at": device.last_sync_at_utc,
                "stale": device.is_stale(report.generated_at_epoch),
            })
        })
        .collect::<Vec<_>>();

    let doc = json!({
        "generated_at": crate::utc::format_epoch(report.generated_at_epoch),
        "sources": sources,
        "devices": devices,
    });

    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
}

fn attribution_json(attribution: &Option<crate::report::Attribution>) -> serde_json::Value {
    match attribution {
        None => serde_json::Value::Null,
        Some(a) => json!({"name": a.name, "output_tokens": a.output_tokens}),
    }
}
