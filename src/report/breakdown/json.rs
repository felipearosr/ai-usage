//! JSON rendering of a [`SourceBreakdown`]: `aiu <source> models --json`
//! emits the full matrix structurally; `aiu <source> machines --json` emits
//! machine shares with a per-machine model list.

use serde_json::json;

use crate::report::breakdown::{Matrix, SourceBreakdown, WindowBreakdown};
use crate::report::detail::{share_percent, VendorQuota};

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
            "cells": matrix.cells,
            "model_totals": row_totals(matrix),
            "machine_totals": column_totals(matrix),
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
                "name": name,
                "output_tokens": total,
                "share_percent": share_percent(total, grand),
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

fn row_totals(matrix: &Matrix) -> Vec<i64> {
    (0..matrix.models.len())
        .map(|i| matrix.model_total(i))
        .collect()
}

fn column_totals(matrix: &Matrix) -> Vec<i64> {
    (0..matrix.machines.len())
        .map(|j| matrix.machine_total(j))
        .collect()
}

fn vendor_json(vendor: &Option<VendorQuota>, now: u64) -> serde_json::Value {
    match vendor {
        Some(vendor) => json!({
            "used_percent": vendor.used_percent,
            "resets_at": vendor.resets_at_utc,
            "resets_in_secs": vendor.resets_in_secs(now),
        }),
        // Explicit gap in JSON too: absent vendor data is null, not 0%.
        None => serde_json::Value::Null,
    }
}
