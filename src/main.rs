use aiu::cli::{self, BreakdownKind, Command};
use aiu::paths;
use aiu::report::{self, breakdown, detail};
use aiu::store::{SourceMode, Store};
use std::io::Write;
use std::time::Duration;

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
        Ok(Command::Machines { json }) => {
            if let Err(e) = run_machines(json) {
                die(1, e);
            }
        }
        Ok(Command::MachineRename { device, name }) => {
            if let Err(e) = run_machine_rename(&device, &name) {
                die(1, e);
            }
        }
        Ok(Command::MachineRemove { device }) => {
            if let Err(e) = run_machine_remove(&device) {
                die(1, e);
            }
        }
        Ok(Command::Sync) => {
            if let Err(e) = run_sync() {
                die(1, e);
            }
        }
        Ok(Command::Init) => {
            if let Err(e) = run_init() {
                die(1, e);
            }
        }
        Ok(Command::Join { code }) => {
            if let Err(e) = run_join(&code) {
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

fn run_machines(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let store = refreshed_store()?;
    let report = report::fleet::build(&store, aiu::utc::now_epoch())?;
    if json {
        println!("{}", report::fleet::render_json(&report));
    } else {
        print!("{}", report::fleet::render_text(&report));
    }
    Ok(())
}

fn run_sync() -> Result<(), Box<dyn std::error::Error>> {
    let store = open_store()?;
    quick_collect(&store)?;
    let config = aiu::setup::load_sync_config(&store, 500)?;
    let mut relay = aiu::relay::HttpRelayClient::from_env()?;
    let summary = aiu::sync::sync_once(&store, &mut relay, &config)?;
    println!(
        "Synced: {} uploaded, {} downloaded, {} duplicates ignored.",
        summary.uploaded, summary.downloaded, summary.duplicates_ignored
    );
    Ok(())
}

fn run_machine_rename(device: &str, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_store()?;
    let config = aiu::setup::load_sync_config(&store, 500)?;
    let device_id = aiu::fleet::rename_machine(&store, &config.workspace_id, device, name)?;
    let mut relay = aiu::relay::HttpRelayClient::from_env()?;
    aiu::sync::sync_once(&store, &mut relay, &config)?;
    println!("Renamed {device_id} to {}.", name.trim());
    Ok(())
}

fn run_machine_remove(device: &str) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_store()?;
    let config = aiu::setup::load_sync_config(&store, 500)?;
    let mut relay = aiu::relay::HttpRelayClient::from_env()?;
    let device_id = aiu::fleet::remove_machine(&store, &mut relay, &config, device)?;
    aiu::sync::sync_once(&store, &mut relay, &config)?;
    println!("Removed {device_id}. Historical usage was kept.");
    Ok(())
}

fn prompt_machine_name() -> Result<String, Box<dyn std::error::Error>> {
    print!("Machine name: ");
    std::io::stdout().flush()?;
    let mut name = String::new();
    std::io::stdin().read_line(&mut name)?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("machine name cannot be empty".into());
    }
    Ok(name)
}

fn setup_home() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    paths::home_dir().ok_or_else(|| aiu::error::AiuError::NoDataDir.into())
}

fn run_init() -> Result<(), Box<dyn std::error::Error>> {
    let store = open_store()?;
    let mut relay = aiu::relay::HttpRelayClient::from_env()?;
    if aiu::setup::is_initialized(&store)? {
        let pairing = aiu::setup::begin_pairing(&store, &mut relay, aiu::utc::now_epoch())?;
        println!("Workspace already initialized.");
        println!("Pair another machine within 10 minutes:");
        println!("  aiu join {}", pairing.code());
        return wait_for_join(&store, &mut relay, &pairing);
    }

    let name = prompt_machine_name()?;
    let home = setup_home()?;
    println!("Detecting sources and importing local history...");
    let outcome = aiu::setup::init_workspace_with_progress(
        &store,
        &mut relay,
        &name,
        &home,
        aiu::utc::now_epoch(),
        &mut print_import_progress,
    )?;
    print!("{}", aiu::setup::render_init(&outcome));
    println!("Workspace setup is complete. Waiting for the joining machine...");
    println!("Press Ctrl-C if you are not pairing now; run `aiu init` later for a fresh code.");
    wait_for_join(&store, &mut relay, &outcome.pairing)
}

fn wait_for_join(
    store: &Store,
    relay: &mut aiu::relay::HttpRelayClient,
    pairing: &aiu::setup::HostPairing,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        match aiu::setup::complete_host_pairing(store, relay, pairing, aiu::utc::now_epoch()) {
            Ok(true) => break,
            Ok(false) => std::thread::sleep(Duration::from_millis(500)),
            Err(error) => return Err(error.into()),
        }
    }
    println!("Machine paired.");
    sync_after_setup(store, relay);
    Ok(())
}

fn run_join(code: &str) -> Result<(), Box<dyn std::error::Error>> {
    let name = prompt_machine_name()?;
    let home = setup_home()?;
    let store = open_store()?;
    let mut relay = aiu::relay::HttpRelayClient::from_env()?;
    let attempt = aiu::setup::start_join(&mut relay, code, &name, aiu::utc::now_epoch())?;
    println!("Pairing accepted. Waiting for the first machine...");
    let outcome = loop {
        match aiu::setup::finish_join_with_progress(
            &store,
            &mut relay,
            &attempt,
            &home,
            aiu::utc::now_epoch(),
            &mut print_import_progress,
        ) {
            Ok(outcome) => break outcome,
            Err(aiu::setup::SetupError::Relay(aiu::setup::PairingRelayError::NotFound)) => {
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(error) => return Err(error.into()),
        }
    };
    print!("{}", aiu::setup::render_join(&outcome));
    sync_after_setup(&store, &mut relay);
    Ok(())
}

fn print_import_progress(progress: aiu::collect::CollectProgress) {
    println!(
        "Importing {}: file {}/{}, {} records read",
        progress.source, progress.file_index, progress.file_count, progress.records_seen
    );
}

fn sync_after_setup(store: &Store, relay: &mut aiu::relay::HttpRelayClient) {
    match aiu::setup::load_sync_config(store, 500) {
        Ok(config) => {
            if let Err(error) = aiu::sync::sync_once(store, relay, &config) {
                eprintln!("aiu: setup completed; initial sync will retry later: {error}");
            }
        }
        Err(error) => {
            eprintln!("aiu: setup completed; initial sync will retry later: {error}");
        }
    }
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
    let store = refreshed_store()?;
    let report = report::build(&store, aiu::utc::now_epoch())?;
    if json_format {
        print!("{}", report::json::render(&report));
    } else {
        print!("{}", report::text::render(&report));
    }
    Ok(())
}

fn run_detail(source: &str, json_format: bool) -> Result<(), Box<dyn std::error::Error>> {
    let store = refreshed_store()?;
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
    let store = refreshed_store()?;
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

fn refreshed_store() -> Result<Store, Box<dyn std::error::Error>> {
    let store = open_store()?;
    quick_collect(&store)?;
    sync_before_report(&store);
    Ok(store)
}

fn sync_before_report(store: &Store) {
    match aiu::setup::is_initialized(store) {
        Ok(false) => return,
        Err(error) => {
            eprintln!("aiu: sync status unavailable; showing local data: {error}");
            return;
        }
        Ok(true) => {}
    }
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let config = aiu::setup::load_sync_config(store, 500)?;
        let mut relay = aiu::relay::HttpRelayClient::from_env()?;
        aiu::sync::sync_once(store, &mut relay, &config)?;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("aiu: sync failed; showing local data: {error}");
    }
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
