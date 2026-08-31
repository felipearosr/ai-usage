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
        Ok(Command::Collect) => {
            if let Err(e) = run_collect() {
                die(1, e);
            }
        }
        Ok(Command::Status { json }) => {
            if let Err(e) = run_status(json) {
                die(1, e);
            }
        }
        Ok(Command::Schedule) => {
            if let Err(e) = run_schedule_show() {
                die(1, e);
            }
        }
        Ok(Command::ScheduleInstall) => {
            if let Err(e) = run_schedule_install() {
                die(1, e);
            }
        }
        Ok(Command::ScheduleRemove) => {
            if let Err(e) = run_schedule_remove() {
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

/// The scheduled pass: one full pipeline invocation, then exit. This is what
/// the systemd timer and launchd agent invoke, so it must never wait for
/// input, never stay resident, and never fail because the relay is down.
fn run_collect() -> Result<(), Box<dyn std::error::Error>> {
    let store = open_store()?;
    let identity = aiu::identity::ensure_local_identity(&store)?;
    let home = setup_home()?;
    let ctx = aiu::adapters::IngestContext {
        device_id: identity.device_id,
        workspace_id: identity.workspace_id,
        now_epoch: aiu::utc::now_epoch(),
    };

    // A machine that has not paired collects and prunes locally; only a
    // configured relay is contacted.
    let mut relay = None;
    let mut config = None;
    if aiu::setup::is_initialized(&store)? {
        match (
            aiu::setup::load_sync_config(&store, 500),
            aiu::relay::HttpRelayClient::from_env(),
        ) {
            (Ok(loaded), Ok(client)) => {
                config = Some(loaded);
                relay = Some(client);
            }
            (Err(error), _) => report_sync_unavailable(&error),
            (_, Err(error)) => report_sync_unavailable(&error),
        }
    }

    let run = aiu::pipeline::run(
        &store,
        &home,
        &ctx,
        relay.as_mut().map(|r| r as &mut dyn aiu::sync::RelayClient),
        config.as_ref(),
        aiu::utc::now_epoch(),
    )?;
    print!("{}", aiu::pipeline::render(&run));
    Ok(())
}

/// Local diagnostics. Every probe is best-effort: an unreachable relay or an
/// unreadable schedule is a line in the report, never a failed command — the
/// whole point is to run when something is already wrong.
fn run_status(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_store()?;
    let home = setup_home()?;
    let report = aiu::report::status::build(
        &store,
        probe_schedule(&home),
        probe_relay(&store),
        aiu::utc::now_epoch(),
    )?;
    if json {
        print!("{}", aiu::report::status::render_json(&report));
    } else {
        print!("{}", aiu::report::status::render_text(&report));
    }
    Ok(())
}

fn probe_schedule(home: &std::path::Path) -> aiu::report::status::ScheduleStatus {
    use aiu::report::status::ScheduleStatus;
    let Some(platform) = aiu::scheduler::current_platform() else {
        return ScheduleStatus::Unsupported;
    };
    let xdg = aiu::scheduler::config_home_from_env();
    let unit_paths = aiu::scheduler::unit_paths(platform, home, xdg.as_deref());

    let Some(installed) = aiu::scheduler::read_installed(platform, home, xdg.as_deref()) else {
        // Units present but unreadable is not the same as no units: something
        // is still scheduled, and what it will do is what cannot be read.
        return if aiu::scheduler::is_installed(platform, home, xdg.as_deref()) {
            ScheduleStatus::Unreadable { unit_paths }
        } else {
            ScheduleStatus::NotInstalled
        };
    };

    // With no spec to compare against there is no basis for saying the
    // schedule is current, so the absence is reported rather than read as
    // agreement.
    let drift =
        aiu::scheduler::current_spec().map(|current| aiu::scheduler::drift(&installed, &current));

    ScheduleStatus::Installed {
        platform,
        interval_minutes: installed.spec.interval_minutes,
        activated: aiu::scheduler::is_activated(platform, &mut aiu::scheduler::ProcessRunner),
        unit_paths: installed.unit_paths,
        drift,
    }
}

/// Asks the relay for a single record. A machine that has not paired has no
/// relay to reach, which is reported as such rather than as a failure.
fn probe_relay(store: &Store) -> aiu::report::status::RelayStatus {
    use aiu::report::status::RelayStatus;
    match aiu::setup::is_initialized(store) {
        Ok(false) => return RelayStatus::NotConfigured,
        Err(error) => return RelayStatus::Unreachable(error.to_string()),
        Ok(true) => {}
    }
    // Addressing the relay and reaching it are different failures with
    // different fixes, and a revoked device — the likeliest machine to be
    // running this command — must not be told the network is down.
    let config = match aiu::setup::load_sync_config(store, 1) {
        Ok(config) => config,
        Err(error) => return RelayStatus::Misconfigured(error.to_string()),
    };
    let mut relay = match aiu::relay::HttpRelayClient::from_env() {
        Ok(relay) => relay,
        Err(error) => return RelayStatus::Misconfigured(error.to_string()),
    };

    match aiu::sync::RelayClient::download(
        &mut relay,
        &config.device_credential,
        &config.workspace_id,
        None,
        1,
    ) {
        Ok(_) => RelayStatus::Reachable,
        Err(aiu::sync::RelayError::Revoked) => RelayStatus::Revoked,
        Err(error) => RelayStatus::Unreachable(error.to_string()),
    }
}

fn run_schedule_show() -> Result<(), Box<dyn std::error::Error>> {
    let home = setup_home()?;
    let schedule = probe_schedule(&home);
    print!("{}", aiu::report::status::render_schedule(&schedule));
    if let aiu::report::status::ScheduleStatus::Installed { unit_paths, .. }
    | aiu::report::status::ScheduleStatus::Unreadable { unit_paths } = &schedule
    {
        for path in unit_paths {
            println!("  {}", path.display());
        }
    }
    Ok(())
}

fn run_schedule_install() -> Result<(), Box<dyn std::error::Error>> {
    let home = setup_home()?;
    match aiu::scheduler::install_default(&home, &mut aiu::scheduler::ProcessRunner) {
        Some(installed) => {
            print!("{}", aiu::scheduler::describe(Some(&installed)));
            println!();
            for path in &installed.unit_paths {
                println!("  {}", path.display());
            }
            Ok(())
        }
        None => Err("no supported scheduler on this OS; run `aiu collect` manually".into()),
    }
}

fn run_schedule_remove() -> Result<(), Box<dyn std::error::Error>> {
    let home = setup_home()?;
    let Some(platform) = aiu::scheduler::current_platform() else {
        println!("No supported scheduler on this OS; nothing to remove.");
        return Ok(());
    };
    let xdg = aiu::scheduler::config_home_from_env();
    let removed = aiu::scheduler::uninstall(
        platform,
        &home,
        xdg.as_deref(),
        &mut aiu::scheduler::ProcessRunner,
    )?;
    if removed {
        println!("Collection schedule removed. Local data and identity are untouched.");
    } else {
        println!("No collection schedule was installed.");
    }
    Ok(())
}

fn report_sync_unavailable(error: &dyn std::fmt::Display) {
    eprintln!("aiu: sync unavailable; collecting locally: {error}");
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
    let mut outcome = outcome;
    outcome.scheduler = aiu::scheduler::install_default(&home, &mut aiu::scheduler::ProcessRunner);
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
    let mut outcome = outcome;
    outcome.scheduler = aiu::scheduler::install_default(&home, &mut aiu::scheduler::ProcessRunner);
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
