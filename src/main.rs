use aiu::cli::{self, BreakdownKind, Command};
use aiu::paths;
use aiu::report::{self, breakdown, detail};
use aiu::store::{SourceMode, Store};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(args) {
        Ok(Command::Help) => print!("{}", cli::usage()),
        Ok(Command::Version) => println!("aiu {}", env!("CARGO_PKG_VERSION")),
        Ok(Command::Report { json }) => {
            if let Err(e) = run_report(json) {
                die(1, e);
            }
        }
        Ok(Command::Detail { source, json }) => {
            if let Err(e) = run_detail(&source, json) {
                die(1, e);
            }
        }
        Ok(Command::Breakdown { source, kind, json }) => {
            if let Err(e) = run_breakdown(&source, kind, json) {
                die(1, e);
            }
        }
        Ok(Command::Sources { json }) => {
            if let Err(e) = run_sources(json) {
                die(1, e);
            }
        }
        Ok(Command::SourcesDetect { json }) => {
            if let Err(e) = run_sources_detect(json) {
                die(1, e);
            }
        }
        Ok(Command::SourcesSet { mode, source, json }) => {
            if let Err(e) = run_sources_set(mode, &source, json) {
                die(1, e);
            }
        }
        Err(e) => die(2, e),
    }
}

fn die(code: i32, err: impl std::fmt::Display) -> ! {
    eprintln!("aiu: {err}");
    std::process::exit(code)
}

fn open_store() -> Result<Store, Box<dyn std::error::Error>> {
    let db_path = paths::db_path().ok_or(aiu::error::AiuError::NoDataDir)?;
    Ok(Store::open(&db_path)?)
}

/// A quick local refresh before rendering: mint the machine identity once,
/// cheaply detect which sources are present, apply overrides, and stream the
/// surviving sources' deltas into the store. A broken source is contained —
/// the report still renders from whatever is already (and newly) persisted.
fn quick_collect(store: &Store) -> Result<(), Box<dyn std::error::Error>> {
    let identity = aiu::identity::ensure_local_identity(store)?;
    let Some(home) = paths::home_dir() else {
        return Ok(());
    };
    let now = aiu::utc::now_epoch();
    let ctx = aiu::adapters::IngestContext {
        device_id: identity.device_id,
        workspace_id: identity.workspace_id,
        now_epoch: now,
    };

    aiu::collect::collect_detected(store, &home, &ctx)?;
    Ok(())
}

fn run_report(json_format: bool) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_store()?;
    quick_collect(&store)?;
    let report = report::build(&store, aiu::utc::now_epoch())?;
    if json_format {
        print!("{}", report::json::render(&report));
    } else {
        print!("{}", report::text::render(&report));
    }
    Ok(())
}

fn run_detail(source: &str, json_format: bool) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_store()?;
    let detail = detail::build(&store, source, aiu::utc::now_epoch())?;
    if json_format {
        print!("{}", detail::json::render(&detail));
    } else {
        print!("{}", detail::text::render(&detail));
    }
    Ok(())
}

fn run_breakdown(
    source: &str,
    kind: BreakdownKind,
    json_format: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_store()?;
    let breakdown = breakdown::build(&store, source, aiu::utc::now_epoch())?;
    match (kind, json_format) {
        (BreakdownKind::Models, true) => print!("{}", breakdown::json::render_matrix(&breakdown)),
        (BreakdownKind::Models, false) => print!("{}", breakdown::text::render_models(&breakdown)),
        (BreakdownKind::Machines, true) => {
            print!("{}", breakdown::json::render_machines(&breakdown))
        }
        (BreakdownKind::Machines, false) => {
            print!("{}", breakdown::text::render_machines(&breakdown))
        }
    }
    Ok(())
}

fn run_sources(json_format: bool) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_store()?;
    let Some(home) = paths::home_dir() else {
        return Err(Box::new(aiu::error::AiuError::NoDataDir));
    };
    let statuses = aiu::sources::statuses(&store, &home)?;
    if json_format {
        print!("{}", aiu::sources::render_statuses_json(&statuses));
    } else {
        print!("{}", aiu::sources::render_statuses(&statuses));
    }
    Ok(())
}

fn run_sources_detect(json_format: bool) -> Result<(), Box<dyn std::error::Error>> {
    let Some(home) = paths::home_dir() else {
        return Err(Box::new(aiu::error::AiuError::NoDataDir));
    };
    let detections = aiu::sources::detect(&home);
    if json_format {
        print!("{}", aiu::sources::render_detections_json(&detections));
    } else {
        print!("{}", aiu::sources::render_detections(&detections));
    }
    Ok(())
}

fn run_sources_set(
    mode: SourceMode,
    source: &str,
    json_format: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_store()?;
    store.set_source_mode(source, mode)?;
    if json_format {
        print!("{}", aiu::sources::render_set_json(source, mode));
    } else {
        print!("{}", aiu::sources::render_set(source, mode));
    }
    Ok(())
}
