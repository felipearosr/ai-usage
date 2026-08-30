//! Workspace init/join acceptance tests (issue 09).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use aiu::setup::{
    complete_host_pairing, finish_join, init_workspace, load_sync_config, render_init, render_join,
    start_join, EncryptedGrant, JoinRequest, PairingOffer, PairingRelay, PairingRelayError,
    SetupError, PAIRING_LIFETIME_SECS,
};
use aiu::store::Store;
use aiu::sync::{
    decrypt_record, encrypt_record, sync_once, DownloadBatch, EncryptedRecord, RelayClient,
    RelayError, SyncRecord,
};

#[derive(Clone)]
struct OfferState {
    offer: PairingOffer,
    host_credential: String,
    request: Option<JoinRequest>,
    grant: Option<(String, EncryptedGrant)>,
    used: bool,
}

#[derive(Default)]
struct FakeRelay {
    workspace_credentials: HashMap<String, HashSet<String>>,
    offers: HashMap<String, OfferState>,
    records: Vec<EncryptedRecord>,
    record_ids: HashSet<(String, String)>,
    fail_publish: bool,
}

impl FakeRelay {
    fn authorized(&self, workspace_id: &str, credential: &str) -> bool {
        self.workspace_credentials
            .get(workspace_id)
            .is_some_and(|credentials| credentials.contains(credential))
    }

    fn offer_mut(
        &mut self,
        locator: &str,
        now_epoch: u64,
    ) -> Result<&mut OfferState, PairingRelayError> {
        let state = self
            .offers
            .get_mut(locator)
            .ok_or(PairingRelayError::NotFound)?;
        if state.used {
            return Err(PairingRelayError::Used);
        }
        if state.offer.expires_at_epoch < now_epoch {
            return Err(PairingRelayError::Expired);
        }
        Ok(state)
    }
}

impl PairingRelay for FakeRelay {
    fn register_workspace(
        &mut self,
        workspace_id: &str,
        device_credential: &str,
    ) -> Result<(), PairingRelayError> {
        self.workspace_credentials
            .entry(workspace_id.to_string())
            .or_default()
            .insert(device_credential.to_string());
        Ok(())
    }

    fn publish_offer(
        &mut self,
        device_credential: &str,
        offer: PairingOffer,
    ) -> Result<(), PairingRelayError> {
        if self.fail_publish {
            return Err(PairingRelayError::Unavailable);
        }
        if !self.authorized(&offer.workspace_id, device_credential) {
            return Err(PairingRelayError::Unauthorized);
        }
        self.offers.insert(
            offer.locator.clone(),
            OfferState {
                offer,
                host_credential: device_credential.to_string(),
                request: None,
                grant: None,
                used: false,
            },
        );
        Ok(())
    }

    fn request_join(
        &mut self,
        locator: &str,
        request: JoinRequest,
        now_epoch: u64,
    ) -> Result<PairingOffer, PairingRelayError> {
        let state = self.offer_mut(locator, now_epoch)?;
        if state.request.is_some() {
            return Err(PairingRelayError::Used);
        }
        state.request = Some(request);
        Ok(state.offer.clone())
    }

    fn pending_join(
        &mut self,
        device_credential: &str,
        locator: &str,
        now_epoch: u64,
    ) -> Result<Option<JoinRequest>, PairingRelayError> {
        let state = self.offer_mut(locator, now_epoch)?;
        if state.host_credential != device_credential {
            return Err(PairingRelayError::Unauthorized);
        }
        Ok(state.request.clone())
    }

    fn complete_join(
        &mut self,
        device_credential: &str,
        locator: &str,
        request_id: &str,
        workspace_id: &str,
        joined_device_credential: &str,
        grant: EncryptedGrant,
        now_epoch: u64,
    ) -> Result<(), PairingRelayError> {
        let state = self.offer_mut(locator, now_epoch)?;
        if state.host_credential != device_credential || state.offer.workspace_id != workspace_id {
            return Err(PairingRelayError::Unauthorized);
        }
        if state
            .request
            .as_ref()
            .map(|request| request.request_id.as_str())
            != Some(request_id)
        {
            return Err(PairingRelayError::NotFound);
        }
        state.grant = Some((request_id.to_string(), grant));
        self.workspace_credentials
            .entry(workspace_id.to_string())
            .or_default()
            .insert(joined_device_credential.to_string());
        Ok(())
    }

    fn take_grant(
        &mut self,
        locator: &str,
        request_id: &str,
        now_epoch: u64,
    ) -> Result<EncryptedGrant, PairingRelayError> {
        let state = self.offer_mut(locator, now_epoch)?;
        let (stored_request_id, grant) = state.grant.take().ok_or(PairingRelayError::NotFound)?;
        if stored_request_id != request_id {
            state.grant = Some((stored_request_id, grant));
            return Err(PairingRelayError::NotFound);
        }
        state.used = true;
        state.request = None;
        Ok(grant)
    }
}

impl RelayClient for FakeRelay {
    fn upload(&mut self, credential: &str, records: &[EncryptedRecord]) -> Result<(), RelayError> {
        for record in records {
            if !self.authorized(&record.workspace_id, credential) {
                return Err(RelayError::Revoked);
            }
            let identity = (record.workspace_id.clone(), record.record_id.clone());
            if self.record_ids.insert(identity) {
                self.records.push(record.clone());
            }
        }
        Ok(())
    }

    fn download(
        &mut self,
        credential: &str,
        workspace_id: &str,
        after_cursor: Option<&str>,
        limit: usize,
    ) -> Result<DownloadBatch, RelayError> {
        if !self.authorized(workspace_id, credential) {
            return Err(RelayError::Revoked);
        }
        let start = after_cursor
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let records = self
            .records
            .iter()
            .filter(|record| record.workspace_id == workspace_id)
            .skip(start)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        Ok(DownloadBatch {
            cursor: (start + records.len()).to_string(),
            records,
        })
    }
}

fn temp_home(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "aiu-setup-{label}-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::remove_dir_all(&path).ok();
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn claude_fixture(home: &Path) {
    write(
        &home.join(".claude/projects/private/session.jsonl"),
        "{\"type\":\"assistant\",\"sessionId\":\"s\",\"timestamp\":\"2026-08-29T12:00:00Z\",\"message\":{\"id\":\"host-event\",\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    );
}

fn codex_fixture(home: &Path) {
    write(
        &home.join(".codex/sessions/2026/08/rollout-join.jsonl"),
        concat!(
            "{\"timestamp\":\"2026-08-29T12:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"session_id\":\"s\",\"cli_version\":\"0.200.0\"}}\n",
            "{\"timestamp\":\"2026-08-29T12:00:01Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.6-codex\",\"turn_id\":\"t\"}}\n",
            "{\"timestamp\":\"2026-08-29T12:00:02Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":3,\"cached_input_tokens\":0,\"output_tokens\":4,\"reasoning_output_tokens\":0,\"total_tokens\":7}}}}\n"
        ),
    );
}

fn event_count(store: &Store) -> i64 {
    store
        .conn()
        .query_row("SELECT COUNT(*) FROM usage_events", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn init_and_join_import_fixture_history_and_sync_immediately() {
    let host_home = temp_home("host-e2e");
    let join_home = temp_home("join-e2e");
    claude_fixture(&host_home);
    codex_fixture(&join_home);
    let host_store = Store::open_in_memory().unwrap();
    let join_store = Store::open_in_memory().unwrap();
    let mut relay = FakeRelay::default();
    let now = 1_777_464_000;

    let initialized =
        init_workspace(&host_store, &mut relay, "workstation", &host_home, now).unwrap();
    assert_eq!(initialized.detected_sources, vec!["claude"]);
    assert_eq!(initialized.imports[0].events_imported, 1);
    assert!(render_init(&initialized).contains("aiu join"));

    let attempt = start_join(&mut relay, &initialized.pairing_code, "laptop", now + 1).unwrap();
    assert!(complete_host_pairing(&host_store, &mut relay, &initialized.pairing, now + 2).unwrap());
    let joined = finish_join(&join_store, &mut relay, &attempt, &join_home, now + 3).unwrap();
    assert_eq!(joined.workspace_id, initialized.workspace_id);
    assert_eq!(joined.detected_sources, vec!["codex"]);
    assert_eq!(joined.imports[0].events_imported, 1);
    assert!(render_join(&joined).contains("Joined workspace"));

    let host_config = load_sync_config(&host_store, 100).unwrap();
    let join_config = load_sync_config(&join_store, 100).unwrap();
    sync_once(&host_store, &mut relay, &host_config).unwrap();
    sync_once(&join_store, &mut relay, &join_config).unwrap();
    sync_once(&host_store, &mut relay, &host_config).unwrap();
    assert_eq!(event_count(&host_store), 2);
    assert_eq!(event_count(&join_store), 2);

    let probe = SyncRecord::UsageEvent(Box::new(aiu::store::NewEvent {
        workspace_id: initialized.workspace_id.clone(),
        ..aiu::store::NewEvent::default()
    }));
    let encrypted = encrypt_record(&initialized.workspace_id, &host_config.key, &probe).unwrap();
    assert_eq!(decrypt_record(&join_config.key, &encrypted).unwrap(), probe);

    let host_name: String = host_store
        .conn()
        .query_row(
            "SELECT friendly_name FROM devices WHERE device_id = ?1",
            [&initialized.device_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(host_name, "workstation");

    std::fs::remove_dir_all(host_home).ok();
    std::fs::remove_dir_all(join_home).ok();
}

#[test]
fn codes_expire_are_single_use_and_do_not_contain_the_workspace_key() {
    let home = temp_home("security");
    let host_store = Store::open_in_memory().unwrap();
    let join_store = Store::open_in_memory().unwrap();
    let mut relay = FakeRelay::default();
    let initialized = init_workspace(&host_store, &mut relay, "host", &home, 100).unwrap();
    let workspace_key = host_store.get_metadata("workspace_key").unwrap().unwrap();
    let compact_code = initialized.pairing_code.replace('-', "");
    assert!(!workspace_key.contains(&compact_code));
    assert!(!compact_code.contains(&workspace_key));

    let attempt = start_join(&mut relay, &initialized.pairing_code, "join", 101).unwrap();
    assert!(matches!(
        start_join(&mut relay, &initialized.pairing_code, "replay", 102),
        Err(SetupError::Relay(PairingRelayError::Used))
    ));
    complete_host_pairing(&host_store, &mut relay, &initialized.pairing, 103).unwrap();
    finish_join(&join_store, &mut relay, &attempt, &home, 104).unwrap();
    assert!(matches!(
        start_join(&mut relay, &initialized.pairing_code, "stolen", 105),
        Err(SetupError::Relay(PairingRelayError::Used))
    ));

    let second_store = Store::open_in_memory().unwrap();
    let second = init_workspace(&second_store, &mut relay, "other", &home, 1_000).unwrap();
    assert!(matches!(
        start_join(
            &mut relay,
            &second.pairing_code,
            "late",
            1_000 + PAIRING_LIFETIME_SECS + 1
        ),
        Err(SetupError::Relay(PairingRelayError::Expired))
    ));
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn relay_cannot_substitute_the_host_ephemeral_key() {
    let home = temp_home("host-auth");
    let store = Store::open_in_memory().unwrap();
    let mut relay = FakeRelay::default();
    let initialized = init_workspace(&store, &mut relay, "host", &home, 10).unwrap();
    let locator = initialized.pairing.code().locator();
    relay
        .offers
        .get_mut(&locator)
        .unwrap()
        .offer
        .host_public_key = [0x55; 32];

    assert!(matches!(
        start_join(&mut relay, &initialized.pairing_code, "join", 11),
        Err(SetupError::HostAuthentication)
    ));
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn setup_requires_a_machine_name_and_adopts_preexisting_local_ids() {
    let home = temp_home("identity");
    let store = Store::open_in_memory().unwrap();
    let existing = aiu::identity::ensure_local_identity(&store).unwrap();
    let mut relay = FakeRelay::default();

    assert!(matches!(
        init_workspace(&store, &mut relay, "  ", &home, 10),
        Err(SetupError::InvalidFriendlyName)
    ));
    let initialized = init_workspace(&store, &mut relay, "desktop", &home, 10).unwrap();
    assert_eq!(initialized.workspace_id, existing.workspace_id);
    assert_eq!(initialized.device_id, existing.device_id);
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn failed_offer_publication_does_not_poison_local_setup() {
    let home = temp_home("retry");
    let store = Store::open_in_memory().unwrap();
    let mut relay = FakeRelay {
        fail_publish: true,
        ..FakeRelay::default()
    };

    assert!(matches!(
        init_workspace(&store, &mut relay, "desktop", &home, 10),
        Err(SetupError::Relay(PairingRelayError::Unavailable))
    ));
    relay.fail_publish = false;
    let initialized = init_workspace(&store, &mut relay, "desktop", &home, 11).unwrap();
    assert_eq!(
        relay.workspace_credentials[&initialized.workspace_id].len(),
        1,
        "a retry must reuse the pending device credential"
    );
    std::fs::remove_dir_all(home).ok();
}
