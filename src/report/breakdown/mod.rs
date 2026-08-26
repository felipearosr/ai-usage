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
use crate::report::latest_window_quotas;
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
                matrix(conn, source, &cutoff)?
            }
            None => Matrix::default(),
        };
        windows.push(WindowBreakdown {
            window: quota.window,
            vendor: Some(VendorQuota {
                used_percent: quota.used_percent,
                resets_at_utc: quota.resets_at_utc,
            }),
            matrix,
        });
    }

    let has_usage: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM usage_events WHERE source = ?1)",
            params![source],
            |row| row.get(0),
        )
        .map_err(crate::error::AiuError::from)?;

    Ok(SourceBreakdown {
        source: source.to_string(),
        generated_at_epoch: now_epoch,
        windows,
        has_usage,
    })
}

/// One aggregated query for the whole window, then pivot in Rust. Rows and
/// columns with no positive tokens are dropped (zero-use hiding); ordering is
/// deterministic: descending totals, then name for ties.
fn matrix(
    conn: &rusqlite::Connection,
    source: &str,
    cutoff_utc: &str,
) -> crate::error::Result<Matrix> {
    let mut stmt = conn.prepare(
        "SELECT e.exact_model, d.friendly_name, SUM(e.output_tokens)
         FROM usage_events e
         JOIN devices d ON d.device_id = e.device_id
         WHERE e.source = ?1 AND e.ts_utc >= ?2
         GROUP BY e.exact_model, d.friendly_name
         HAVING SUM(e.output_tokens) > 0",
    )?;
    let rows = stmt
        .query_map(params![source, cutoff_utc], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut model_totals: BTreeMap<String, i64> = BTreeMap::new();
    let mut machine_totals: BTreeMap<String, i64> = BTreeMap::new();
    let mut cells: BTreeMap<(String, String), i64> = BTreeMap::new();
    for (model, machine, tokens) in rows {
        *model_totals.entry(model.clone()).or_default() += tokens;
        *machine_totals.entry(machine.clone()).or_default() += tokens;
        *cells.entry((model, machine)).or_default() += tokens;
    }

    let mut models: Vec<String> = model_totals.keys().cloned().collect();
    models.sort_by(|a, b| model_totals[b].cmp(&model_totals[a]).then(a.cmp(b)));
    let mut machines: Vec<String> = machine_totals.keys().cloned().collect();
    machines.sort_by(|a, b| machine_totals[b].cmp(&machine_totals[a]).then(a.cmp(b)));

    let cells = models
        .iter()
        .map(|model| {
            machines
                .iter()
                .map(|machine| *cells.get(&(model.clone(), machine.clone())).unwrap_or(&0))
                .collect()
        })
        .collect();

    Ok(Matrix {
        models,
        machines,
        cells,
    })
}
