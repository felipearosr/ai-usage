//! JSON rendering of a [`SourceBreakdown`]: `aiu <source> models --json`
//! emits the full matrix structurally; `aiu <source> machines --json` emits
//! machine shares with a per-machine model list.

use serde_json::json;

use crate::report::breakdown::{SourceBreakdown, WindowBreakdown};
use crate::report::detail::json::vendor_json;
use crate::report::detail::share_percent;

pub fn render_matrix(breakdown: &SourceBreakdown) -> String {
    let windows = breakdown
        .windows
        .iter()
        .map(|w| matrix_window_json(w, breakdown.generated_at_epoch))
        .collect::<Vec<_>>();
    let doc = json!({
        "source": breakdown.source,
        "generated_at": crate::utc::format_epoch(breakdown.generated_at_epoch),
        "has_usage": breakdown.has_usage,
        "windows": windows,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
}

pub fn render_machines(breakdown: &SourceBreakdown) -> String {
    let windows = breakdown
        .windows
        .iter()
        .map(|w| machines_window_json(w, breakdown.generated_at_epoch))
        .collect::<Vec<_>>();
    let doc = json!({
        "source": breakdown.source,
        "generated_at": crate::utc::format_epoch(breakdown.generated_at_epoch),
        "has_usage": breakdown.has_usage,
        "windows": windows,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
}

fn matrix_window_json(window: &WindowBreakdown, now: u64) -> serde_json::Value {
    let matrix = &window.matrix;
    json!({
        "window": window.window,
        "vendor": vendor_json(&window.vendor, now),
        "matrix": {
            "models": matrix.models,
            "machines": matrix.machines,
            "machine_ids": matrix.machine_ids,
            "machine_stale": matrix.machine_stale,
            "cells": matrix.cells,
            "model_totals": matrix.model_totals(),
            "machine_totals": matrix.machine_totals(),
            "model_shares": matrix.model_share_percents(),
            "machine_shares": matrix.machine_share_percents(),
            "grand_total": matrix.grand_total(),
        },
    })
}

fn machines_window_json(window: &WindowBreakdown, now: u64) -> serde_json::Value {
    let matrix = &window.matrix;
    let grand = matrix.grand_total();
    let machines = matrix
        .machines
        .iter()
        .enumerate()
        .map(|(j, name)| {
            let total = matrix.machine_total(j);
            json!({
                "device_id": matrix.machine_ids[j],
                "name": name,
                "output_tokens": total,
                "share_percent": share_percent(total, grand),
                "stale": matrix.machine_stale[j],
                "models": matrix.models_for_machine(j)
                    .iter()
                    .map(|m| {
                        json!({
                            "name": m.name,
                            "output_tokens": m.output_tokens,
                            "share_percent": m.share_percent,
                        })
                    })
                    .collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "window": window.window,
        "vendor": vendor_json(&window.vendor, now),
        "machines": machines,
    })
}
