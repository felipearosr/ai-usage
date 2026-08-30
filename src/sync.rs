//! End-to-end encrypted synchronization through an opaque relay boundary.

use std::fmt;

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::store::{DeviceSyncState, NewEvent, NewSnapshot};

#[derive(Debug)]
pub enum SyncError {
    Crypto,
    WorkspaceMismatch,
    Serialization(serde_json::Error),
    Store(crate::error::AiuError),
    Relay(RelayError),
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::Crypto => write!(f, "sync record authentication failed"),
            SyncError::WorkspaceMismatch => {
                write!(f, "sync record belongs to a different workspace")
            }
            SyncError::Serialization(error) => {
                write!(f, "sync record serialization failed: {error}")
            }
            SyncError::Store(error) => write!(f, "sync storage failed: {error}"),
            SyncError::Relay(error) => write!(f, "sync relay failed: {error}"),
        }
    }
}

impl std::error::Error for SyncError {}

impl From<serde_json::Error> for SyncError {
    fn from(error: serde_json::Error) -> Self {
        SyncError::Serialization(error)
    }
}

impl From<crate::error::AiuError> for SyncError {
    fn from(error: crate::error::AiuError) -> Self {
        SyncError::Store(error)
    }
}

impl From<RelayError> for SyncError {
    fn from(error: RelayError) -> Self {
        SyncError::Relay(error)
    }
}

pub type Result<T> = std::result::Result<T, SyncError>;

#[derive(Clone)]
pub struct WorkspaceKey([u8; 32]);

impl WorkspaceKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Drop for WorkspaceKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

pub struct SyncConfig {
    pub workspace_id: String,
    pub device_id: String,
    pub device_credential: String,
    pub key: WorkspaceKey,
    pub download_limit: usize,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncSummary {
    pub uploaded: usize,
    pub downloaded: usize,
    pub duplicates_ignored: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeviceRevocation {
    pub workspace_id: String,
    pub device_id: String,
    pub revoked_at_utc: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "record")]
pub enum SyncRecord {
    UsageEvent(Box<NewEvent>),
    QuotaSnapshot(Box<NewSnapshot>),
    DeviceState(Box<DeviceSyncState>),
    DeviceRevocation(Box<DeviceRevocation>),
}

impl SyncRecord {
    fn belongs_to(&self, workspace_id: &str) -> bool {
        match self {
            SyncRecord::UsageEvent(event) => event.workspace_id == workspace_id,
            SyncRecord::QuotaSnapshot(_) => true,
            SyncRecord::DeviceState(device) => device.workspace_id == workspace_id,
            SyncRecord::DeviceRevocation(device) => device.workspace_id == workspace_id,
        }
    }
}

/// The relay-visible form. The workspace and record IDs are opaque routing
/// values. All usage metadata remains inside authenticated ciphertext.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EncryptedRecord {
    pub workspace_id: String,
    pub record_id: String,
    pub nonce: [u8; 24],
    pub ciphertext: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayError {
    Unavailable,
    Revoked,
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RelayError::Unavailable => write!(f, "relay unavailable"),
            RelayError::Revoked => write!(f, "device credential revoked"),
        }
    }
}

impl std::error::Error for RelayError {}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DownloadBatch {
    pub records: Vec<EncryptedRecord>,
    pub cursor: String,
}

/// External relay boundary. Implementations may perform network I/O, while
/// seam tests use an in-memory fake. Plaintext records and workspace keys are
/// deliberately absent from this interface.
pub trait RelayClient {
    fn upload(
        &mut self,
        device_credential: &str,
        records: &[EncryptedRecord],
    ) -> std::result::Result<(), RelayError>;

    fn download(
        &mut self,
        device_credential: &str,
        workspace_id: &str,
        after_cursor: Option<&str>,
        limit: usize,
    ) -> std::result::Result<DownloadBatch, RelayError>;

    fn revoke_device(
        &mut self,
        device_credential: &str,
        workspace_id: &str,
        device_id: &str,
    ) -> std::result::Result<(), RelayError>;
}

pub fn encrypt_record(
    workspace_id: &str,
    key: &WorkspaceKey,
    record: &SyncRecord,
) -> Result<EncryptedRecord> {
    let cipher = XChaCha20Poly1305::new((&key.0).into());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let plaintext = serde_json::to_vec(record)?;
    let record_id = relay_record_id(key, &plaintext);
    let associated_data = associated_data(workspace_id, &record_id);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &plaintext,
                aad: associated_data.as_bytes(),
            },
        )
        .map_err(|_| SyncError::Crypto)?;
    Ok(EncryptedRecord {
        workspace_id: workspace_id.to_string(),
        record_id,
        nonce: nonce.into(),
        ciphertext,
    })
}

pub fn decrypt_record(key: &WorkspaceKey, encrypted: &EncryptedRecord) -> Result<SyncRecord> {
    let cipher = XChaCha20Poly1305::new((&key.0).into());
    let associated_data = associated_data(&encrypted.workspace_id, &encrypted.record_id);
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(&encrypted.nonce),
            Payload {
                msg: &encrypted.ciphertext,
                aad: associated_data.as_bytes(),
            },
        )
        .map_err(|_| SyncError::Crypto)?;
    if relay_record_id(key, &plaintext) != encrypted.record_id {
        return Err(SyncError::Crypto);
    }
    let record: SyncRecord = serde_json::from_slice(&plaintext)?;
    Ok(record)
}

type HmacSha256 = Hmac<Sha256>;

fn relay_record_id(key: &WorkspaceKey, plaintext: &[u8]) -> String {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(&key.0)
        .expect("HMAC-SHA-256 accepts a 32-byte workspace key");
    mac.update(b"aiu:relay-record-id:v1\0");
    mac.update(plaintext);
    hex(mac.finalize().into_bytes().as_slice())
}

fn local_record_id(payload: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"aiu:local-outbox-id:v1\0");
    hasher.update(payload);
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn associated_data(workspace_id: &str, record_id: &str) -> String {
    format!("aiu-sync-v1\0{workspace_id}\0{record_id}")
}

/// Adds one immutable plaintext record to the local durable outbox. Encryption
/// happens immediately before relay upload, and a stable record ID makes both
/// local enqueue and relay delivery idempotent.
pub fn enqueue_record(
    store: &crate::store::Store,
    record: &SyncRecord,
) -> crate::error::Result<bool> {
    let kind = match record {
        SyncRecord::UsageEvent(_) => "usage_event",
        SyncRecord::QuotaSnapshot(_) => "quota_snapshot",
        SyncRecord::DeviceState(_) => "device_state",
        SyncRecord::DeviceRevocation(_) => "device_revocation",
    };
    let payload = serde_json::to_vec(record)?;
    store.enqueue_sync_record(&local_record_id(&payload), kind, &payload)
}

/// Uploads the durable outbox, then downloads and applies one cursor page.
/// Outbox rows are marked sent only after the relay accepts the whole batch.
pub fn sync_once(
    store: &crate::store::Store,
    relay: &mut dyn RelayClient,
    config: &SyncConfig,
) -> Result<SyncSummary> {
    let sync_at = crate::utc::now_rfc3339();
    let device =
        store.device_sync_state(&config.workspace_id, &config.device_id, Some(&sync_at))?;
    enqueue_record(store, &SyncRecord::DeviceState(Box::new(device)))?;
    let pending = store.pending_sync_records()?;
    let encrypted = pending
        .iter()
        .map(|item| {
            let record: SyncRecord = serde_json::from_slice(&item.payload)?;
            if !record.belongs_to(&config.workspace_id) {
                return Err(SyncError::WorkspaceMismatch);
            }
            encrypt_record(&config.workspace_id, &config.key, &record)
        })
        .collect::<Result<Vec<_>>>()?;

    if !encrypted.is_empty() {
        relay.upload(&config.device_credential, &encrypted)?;
        let ids = pending
            .iter()
            .map(|item| item.outbox_id)
            .collect::<Vec<_>>();
        store.mark_sync_records_sent(&ids)?;
        store.touch_device_sync(&config.device_id, &sync_at)?;
    }

    let cursor = store.sync_cursor()?;
    let batch = relay.download(
        &config.device_credential,
        &config.workspace_id,
        cursor.as_deref(),
        config.download_limit.max(1),
    )?;
    let mut summary = SyncSummary {
        uploaded: encrypted.len(),
        ..SyncSummary::default()
    };
    for encrypted_record in &batch.records {
        if encrypted_record.workspace_id != config.workspace_id {
            return Err(SyncError::WorkspaceMismatch);
        }
        if store.sync_record_applied(&encrypted_record.record_id)? {
            summary.duplicates_ignored += 1;
            continue;
        }
        let record = decrypt_record(&config.key, encrypted_record)?;
        if !record.belongs_to(&config.workspace_id) {
            return Err(SyncError::WorkspaceMismatch);
        }
        apply_record(store, &record)?;
        store.mark_sync_record_applied(&encrypted_record.record_id)?;
        summary.downloaded += 1;
    }
    store.set_sync_cursor(&batch.cursor)?;
    Ok(summary)
}

fn apply_record(store: &crate::store::Store, record: &SyncRecord) -> Result<()> {
    match record {
        SyncRecord::UsageEvent(event) => {
            ensure_device(store, &event.device_id)?;
            store.record_event(event)?;
        }
        SyncRecord::QuotaSnapshot(snapshot) => {
            ensure_device(store, &snapshot.observing_device_id)?;
            store.record_snapshot_if_changed(snapshot)?;
        }
        SyncRecord::DeviceState(device) => store.apply_device_sync_state(device)?,
        SyncRecord::DeviceRevocation(device) => {
            store.mark_device_revoked(&device.device_id, &device.revoked_at_utc)?
        }
    }
    Ok(())
}

fn ensure_device(store: &crate::store::Store, device_id: &str) -> Result<()> {
    store.ensure_device(&crate::store::NewDevice {
        device_id: device_id.to_string(),
        friendly_name: device_id.to_string(),
        os: String::new(),
        arch: String::new(),
        last_sync_at_utc: None,
    })?;
    Ok(())
}
