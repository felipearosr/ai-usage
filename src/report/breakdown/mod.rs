//! Deep-dive breakdown commands (`aiu <source> models`, `aiu <source> machines`).
//!
//! `models` renders a machine × exact-model matrix per window: rows are
//! exact models (row totals equal model share), columns are machines (column
//! totals equal machine share), and cells sum to the window's whole. `machines`
//! renders the same machine shares plus a per-machine exact-model list. Both
//! filter to the exact window shown, hide zero-use rows/columns, and render
//! only participating machines.

pub mod json;
pub mod text;

use std::collections::BTreeMap;

use rusqlite::params;

use crate::report::detail::{window_span_secs, VendorQuota};
use crate::report::{has_usage, latest_window_quotas};
use crate::store::Store;
use crate::utc;

#[derive(Debug, PartialEq)]
pub struct SourceBreakdown {
    pub source: String,
    pub generated_at_epoch: u64,
    pub windows: Vec<WindowBreakdown>,
    /// True when usage events exist for this source even though no vendor
    /// window is known yet; renders as an explicit gap, never as silence.
    pub has_usage: bool,
}

#[derive(Debug, PartialEq)]
pub struct WindowBreakdown {
    pub window: String,
    /// Latest vendor observation for this window; None is rendered as an
    /// explicit gap, never as zero percent.
    pub vendor: Option<VendorQuota>,
    pub matrix: Matrix,
}

/// A machine × exact-model grid of output tokens for one window. Rows are
/// exact models, columns are machines: `cells[model][machine]` holds the
/// tokens that model produced on that machine. Zero-use models and machines
/// are absent by construction, so every row and column total is positive.
#[derive(Debug, PartialEq, Default)]
pub struct Matrix {
    /// Row labels (exact models), ordered by descending model total.
    pub models: Vec<String>,
    /// Column labels (machines), ordered by descending machine total.
    pub machines: Vec<String>,
    /// Device ids parallel to [`Matrix::machines`]. Machines are keyed by
    /// device id; the friendly name is display only, so two machines sharing
    /// a name stay distinguishable downstream.
    pub machine_ids: Vec<String>,
    /// Staleness flags parallel to [`Matrix::machines`].
    pub machine_stale: Vec<bool>,
    /// Last successful sync timestamps parallel to [`Matrix::machines`].
    pub machine_last_sync_at_utc: Vec<Option<String>>,
    /// `cells[model][machine]` output tokens within this window.
    pub cells: Vec<Vec<i64>>,
}

impl Matrix {
    /// Row total: the tokens `models[model]` produced across all machines.
    pub fn model_total(&self, model: usize) -> i64 {
        self.cells[model].iter().sum()
    }

    /// Column total: the tokens `machines[machine]` produced across all models.
    pub fn machine_total(&self, machine: usize) -> i64 {
        self.cells.iter().map(|row| row[machine]).sum()
    }

    /// The whole window: the sum of every cell.
    pub fn grand_total(&self) -> i64 {
        self.cells.iter().flat_map(|row| row.iter()).sum()
    }

    /// Row totals, indexed like [`Matrix::models`].
    pub fn model_totals(&self) -> Vec<i64> {
        (0..self.models.len())
            .map(|i| self.model_total(i))
            .collect()
    }

    /// Column totals, indexed like [`Matrix::machines`].
    pub fn machine_totals(&self) -> Vec<i64> {
        (0..self.machines.len())
            .map(|j| self.machine_total(j))
            .collect()
    }

    /// Row totals as shares of the whole window, indexed like `models`.
    pub fn model_share_percents(&self) -> Vec<f64> {
        let grand = self.grand_total();
        (0..self.models.len())
            .map(|i| crate::report::detail::share_percent(self.model_total(i), grand))
            .collect()
    }

    /// Column totals as shares of the whole window, indexed like `machines`.
    pub fn machine_share_percents(&self) -> Vec<f64> {
        let grand = self.grand_total();
        (0..self.machines.len())
            .map(|j| crate::report::detail::share_percent(self.machine_total(j), grand))
            .collect()
    }

    /// True when the window has no usage at all (no positive cells).
    pub fn is_empty(&self) -> bool {
        self.grand_total() == 0
    }

    /// Machine shares of the whole window, in display order.
    pub fn machine_shares(&self) -> Vec<crate::report::detail::Share> {
        let grand = self.grand_total();
        self.machines
            .iter()
            .enumerate()
            .map(|(j, name)| crate::report::detail::Share {
                name: name.clone(),
                output_tokens: self.machine_total(j),
                share_percent: crate::report::detail::share_percent(self.machine_total(j), grand),
                stale: self.machine_stale[j],
                last_sync_at_utc: self.machine_last_sync_at_utc[j].clone(),
            })
            .collect()
    }

    /// Exact models used on `machines[machine]`, with shares of that
    /// machine's own total (not the window's).
    pub fn models_for_machine(&self, machine: usize) -> Vec<crate::report::detail::Share> {
        let total = self.machine_total(machine);
        (0..self.models.len())
            .filter(|&i| self.cells[i][machine] > 0)
            .map(|i| crate::report::detail::Share {
                name: self.models[i].clone(),
                output_tokens: self.cells[i][machine],
                share_percent: crate::report::detail::share_percent(self.cells[i][machine], total),
                stale: false,
                last_sync_at_utc: None,
            })
            .collect()
    }
}

/// Builds the breakdown through the same queries the CLI renders. `now_epoch`
/// is injected so window filtering and reset math are deterministic in tests.
pub fn build(store: &Store, source: &str, now_epoch: u64) -> crate::error::Result<SourceBreakdown> {
    let conn = store.conn();
    let quotas = latest_window_quotas(conn, source)?;

    let mut windows = Vec::with_capacity(quotas.len());
    for quota in quotas {
        let span = window_span_secs(&quota.window);
        // A matrix only exists when we know how long the window is; otherwise
        // cells would not match the window shown.
        let matrix = match span {
            Some(span) => {
                let cutoff = utc::format_epoch(now_epoch.saturating_sub(span));
                matrix(conn, source, &cutoff, now_epoch)?
            }
            None => Matrix::default(),
        };
        windows.push(WindowBreakdown {
            window: quota.window,
            vendor: Some(VendorQuota {
                used_percent: quota.used_percent,
                resets_at_utc: quota.resets_at_utc,
                observed_at_utc: quota.observed_at_utc,
                observing_device_name: quota.observing_device_name,
                observer_last_sync_at_utc: quota.observer_last_sync_at_utc,
            }),
            matrix,
        });
    }

    let has_usage = has_usage(conn, source)?;

    Ok(SourceBreakdown {
        source: source.to_string(),
        generated_at_epoch: now_epoch,
        windows,
        has_usage,
    })
}

/// One aggregated query for the whole window, then pivot in Rust. Rows and
/// columns with no positive tokens are dropped (zero-use hiding); ordering is
/// deterministic: descending totals, then name, then device id for ties.
///
/// The machine dimension is keyed by `device_id` (with the friendly name as
/// its display label) so two machines sharing a name are kept apart rather
/// than folded together — machine attribution is per device, not per name.
fn matrix(
    conn: &rusqlite::Connection,
    source: &str,
    cutoff_utc: &str,
    now_epoch: u64,
) -> crate::error::Result<Matrix> {
    let mut stmt = conn.prepare(
        "SELECT e.exact_model, e.device_id, d.friendly_name, SUM(e.output_tokens),
                d.last_sync_at_utc
         FROM usage_events e
         JOIN devices d ON d.device_id = e.device_id
         WHERE e.source = ?1 AND e.ts_utc >= ?2
         GROUP BY e.exact_model, e.device_id, d.friendly_name, d.last_sync_at_utc
         HAVING SUM(e.output_tokens) > 0",
    )?;
    let rows = stmt
        .query_map(params![source, cutoff_utc], |row| {
            Ok((
                row.get::<_, String>(0)?,         // exact_model
                row.get::<_, String>(1)?,         // device_id
                row.get::<_, String>(2)?,         // friendly_name
                row.get::<_, i64>(3)?,            // output tokens
                row.get::<_, Option<String>>(4)?, // last sync
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut model_totals: BTreeMap<String, i64> = BTreeMap::new();
    let mut machine_names: BTreeMap<String, String> = BTreeMap::new();
    let mut machine_totals: BTreeMap<String, i64> = BTreeMap::new();
    let mut machine_last_sync: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut cells: BTreeMap<(String, String), i64> = BTreeMap::new();
    for (model, device_id, name, tokens, last_sync) in rows {
        *model_totals.entry(model.clone()).or_default() += tokens;
        machine_names.entry(device_id.clone()).or_insert(name);
        *machine_totals.entry(device_id.clone()).or_default() += tokens;
        machine_last_sync
            .entry(device_id.clone())
            .or_insert(last_sync);
        *cells.entry((model, device_id)).or_default() += tokens;
    }

    let mut models: Vec<String> = model_totals.keys().cloned().collect();
    models.sort_by(|a, b| model_totals[b].cmp(&model_totals[a]).then(a.cmp(b)));

    let mut machine_ids: Vec<String> = machine_totals.keys().cloned().collect();
    machine_ids.sort_by(|a, b| {
        machine_totals[b]
            .cmp(&machine_totals[a])
            .then_with(|| machine_names[a].cmp(&machine_names[b]))
            .then_with(|| a.cmp(b))
    });
    let machines: Vec<String> = machine_ids
        .iter()
        .map(|id| machine_names[id].clone())
        .collect();
    let machine_last_sync_at_utc = machine_ids
        .iter()
        .map(|id| machine_last_sync[id].clone())
        .collect::<Vec<_>>();
    let machine_stale = machine_last_sync_at_utc
        .iter()
        .map(|last_sync| crate::report::is_stale_at(last_sync.as_deref(), now_epoch))
        .collect();

    let cells = models
        .iter()
        .map(|model| {
            machine_ids
                .iter()
                .map(|id| *cells.get(&(model.clone(), id.clone())).unwrap_or(&0))
                .collect()
        })
        .collect();

    Ok(Matrix {
        models,
        machines,
        machine_ids,
        machine_stale,
        machine_last_sync_at_utc,
        cells,
    })
}
