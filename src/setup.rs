//! No-account workspace setup and authenticated device pairing.
//!
//! The relay stores an expiring rendezvous record, public ephemeral keys, and
//! an encrypted grant. The visible code contains only a random locator and a
//! truncated fingerprint of the inviter's ephemeral public key. X25519 and
//! HKDF derive a one-time wrapping key; XChaCha20-Poly1305 protects the grant.
//! The permanent workspace key only exists inside that authenticated
//! ciphertext and on participating devices.

use std::fmt;
use std::path::Path;

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

use crate::collect::{CollectProgress, SourceCollect};
use crate::store::{NewDevice, Store};
use crate::sync::{SyncConfig, WorkspaceKey};

pub const PAIRING_LIFETIME_SECS: u64 = 10 * 60;
const WORKSPACE_ID: &str = "workspace_id";
const DEVICE_ID: &str = "device_id";
const DEVICE_CREDENTIAL: &str = "device_credential";
const WORKSPACE_KEY: &str = "workspace_key";
const SETUP_COMPLETE: &str = "setup_complete";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingRelayError {
    Unavailable,
    Unauthorized,
    NotFound,
    Expired,
    Used,
}

impl fmt::Display for PairingRelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Unavailable => "pairing relay unavailable",
            Self::Unauthorized => "device credential rejected",
            Self::NotFound => "pairing code not found",
            Self::Expired => "pairing code expired",
            Self::Used => "pairing code has already been used",
        };
        write!(f, "{message}")
    }
}

impl std::error::Error for PairingRelayError {}

#[derive(Debug)]
pub enum SetupError {
    Store(crate::error::AiuError),
    Relay(PairingRelayError),
    InvalidCode,
    InvalidFriendlyName,
    HostAuthentication,
    GrantAuthentication,
    Expired,
    AlreadyInitialized,
    NotInitialized,
    Serialization(serde_json::Error),
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(f, "setup storage failed: {error}"),
            Self::Relay(error) => write!(f, "{error}"),
            Self::InvalidCode => write!(f, "invalid pairing code"),
            Self::InvalidFriendlyName => write!(f, "machine name cannot be empty"),
            Self::HostAuthentication => write!(f, "pairing host authentication failed"),
            Self::GrantAuthentication => write!(f, "pairing grant authentication failed"),
            Self::Expired => write!(f, "pairing code expired"),
            Self::AlreadyInitialized => write!(f, "this machine is already in a workspace"),
            Self::NotInitialized => write!(f, "run `aiu init` or `aiu join <code>` first"),
            Self::Serialization(error) => write!(f, "pairing data is invalid: {error}"),
        }
    }
}

impl std::error::Error for SetupError {}

impl From<crate::error::AiuError> for SetupError {
    fn from(error: crate::error::AiuError) -> Self {
        Self::Store(error)
    }
}

impl From<PairingRelayError> for SetupError {
    fn from(error: PairingRelayError) -> Self {
        Self::Relay(error)
    }
}

impl From<serde_json::Error> for SetupError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization(error)
    }
}

pub type Result<T> = std::result::Result<T, SetupError>;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PairingOffer {
    pub locator: String,
    pub workspace_id: String,
    pub host_public_key: [u8; 32],
    pub expires_at_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JoinRequest {
    pub request_id: String,
    pub joiner_public_key: [u8; 32],
    pub device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EncryptedGrant {
    pub nonce: [u8; 24],
    pub ciphertext: Vec<u8>,
}

/// Pairing-only relay operations. Implementations see no workspace key,
/// friendly machine name, source name, model, or usage value.
pub trait PairingRelay {
    fn register_workspace(
        &mut self,
        workspace_id: &str,
        device_id: &str,
        device_credential: &str,
    ) -> std::result::Result<(), PairingRelayError>;

    fn publish_offer(
        &mut self,
        device_credential: &str,
        offer: PairingOffer,
    ) -> std::result::Result<(), PairingRelayError>;

    fn request_join(
        &mut self,
        locator: &str,
        request: JoinRequest,
        now_epoch: u64,
    ) -> std::result::Result<PairingOffer, PairingRelayError>;

    fn pending_join(
        &mut self,
        device_credential: &str,
        locator: &str,
        now_epoch: u64,
    ) -> std::result::Result<Option<JoinRequest>, PairingRelayError>;

    #[allow(clippy::too_many_arguments)]
    fn complete_join(
        &mut self,
        device_credential: &str,
        locator: &str,
        request_id: &str,
        workspace_id: &str,
        joined_device_credential: &str,
        grant: EncryptedGrant,
        now_epoch: u64,
    ) -> std::result::Result<(), PairingRelayError>;

    fn take_grant(
        &mut self,
        locator: &str,
        request_id: &str,
        now_epoch: u64,
    ) -> std::result::Result<EncryptedGrant, PairingRelayError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode {
    locator: [u8; 8],
    host_fingerprint: [u8; 10],
}

impl PairingCode {
    pub fn parse(value: &str) -> Result<Self> {
        let compact = value.replace('-', "");
        if compact.len() != 36 {
            return Err(SetupError::InvalidCode);
        }
        let bytes = decode_hex(&compact).ok_or(SetupError::InvalidCode)?;
        let mut locator = [0; 8];
        let mut host_fingerprint = [0; 10];
        locator.copy_from_slice(&bytes[..8]);
        host_fingerprint.copy_from_slice(&bytes[8..]);
        Ok(Self {
            locator,
            host_fingerprint,
        })
    }

    pub fn locator(&self) -> String {
        hex(&self.locator)
    }
}

impl fmt::Display for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let compact = format!("{}{}", hex(&self.locator), hex(&self.host_fingerprint));
        for (index, chunk) in compact.as_bytes().chunks(6).enumerate() {
            if index > 0 {
                write!(f, "-")?;
            }
            write!(f, "{}", std::str::from_utf8(chunk).expect("hex is UTF-8"))?;
        }
        Ok(())
    }
}

pub struct HostPairing {
    code: PairingCode,
    secret: StaticSecret,
    public_key: [u8; 32],
    expires_at_epoch: u64,
}

impl fmt::Debug for HostPairing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostPairing")
            .field("code", &self.code)
            .field("expires_at_epoch", &self.expires_at_epoch)
            .finish_non_exhaustive()
    }
}

impl HostPairing {
    pub fn code(&self) -> &PairingCode {
        &self.code
    }
}

pub struct JoinAttempt {
    code: PairingCode,
    secret: StaticSecret,
    host_public_key: [u8; 32],
    request_id: String,
    device_id: String,
    friendly_name: String,
    expires_at_epoch: u64,
}

#[derive(Debug)]
pub struct InitOutcome {
    pub workspace_id: String,
    pub device_id: String,
    pub pairing_code: String,
    pub detected_sources: Vec<&'static str>,
    pub imports: Vec<SourceCollect>,
    pub pairing: HostPairing,
}

#[derive(Debug)]
pub struct JoinOutcome {
    pub workspace_id: String,
    pub device_id: String,
    pub detected_sources: Vec<&'static str>,
    pub imports: Vec<SourceCollect>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct GrantPayload {
    workspace_id: String,
    workspace_key: [u8; 32],
    device_credential: String,
}

pub fn init_workspace(
    store: &Store,
    relay: &mut dyn PairingRelay,
    friendly_name: &str,
    home: &Path,
    now_epoch: u64,
) -> Result<InitOutcome> {
    init_workspace_with_progress(store, relay, friendly_name, home, now_epoch, &mut |_| {})
}

pub fn init_workspace_with_progress(
    store: &Store,
    relay: &mut dyn PairingRelay,
    friendly_name: &str,
    home: &Path,
    now_epoch: u64,
    progress: &mut dyn FnMut(CollectProgress),
) -> Result<InitOutcome> {
    validate_fresh(store)?;
    validate_friendly_name(friendly_name)?;
    // Plain report commands may have already minted storage-level ids while
    // collecting local history. Adopt those ids so init never strands queued
    // records under an abandoned workspace.
    let existing = crate::identity::ensure_local_identity(store)?;
    let workspace_id = existing.workspace_id;
    let device_id = existing.device_id;
    let device_credential = match store.get_metadata(DEVICE_CREDENTIAL)? {
        Some(value) => value,
        None => {
            let value = random_hex(32);
            store.set_metadata(DEVICE_CREDENTIAL, &value)?;
            value
        }
    };
    let workspace_key = match store
        .get_metadata(WORKSPACE_KEY)?
        .and_then(|value| decode_array::<32>(&value))
    {
        Some(value) => value,
        None => {
            let value = random_array::<32>();
            store.set_metadata(WORKSPACE_KEY, &hex(&value))?;
            value
        }
    };

    relay.register_workspace(&workspace_id, &device_id, &device_credential)?;
    let pairing = publish_pairing(relay, &workspace_id, &device_credential, now_epoch)?;
    persist_identity(
        store,
        &workspace_id,
        &device_id,
        friendly_name,
        &device_credential,
        &workspace_key,
    )?;
    let (detected_sources, imports) =
        import_history(store, home, &workspace_id, &device_id, now_epoch, progress)?;
    let pairing_code = pairing.code().to_string();

    Ok(InitOutcome {
        workspace_id,
        device_id,
        pairing_code,
        detected_sources,
        imports,
        pairing,
    })
}

pub fn begin_pairing(
    store: &Store,
    relay: &mut dyn PairingRelay,
    now_epoch: u64,
) -> Result<HostPairing> {
    let secrets = load_local_secrets(store)?;
    publish_pairing(
        relay,
        &secrets.workspace_id,
        &secrets.device_credential,
        now_epoch,
    )
}

fn publish_pairing(
    relay: &mut dyn PairingRelay,
    workspace_id: &str,
    device_credential: &str,
    now_epoch: u64,
) -> Result<HostPairing> {
    let secret = StaticSecret::random_from_rng(OsRng);
    let public_key = PublicKey::from(&secret).to_bytes();
    let locator = random_array::<8>();
    let fingerprint = fingerprint(&public_key);
    let code = PairingCode {
        locator,
        host_fingerprint: fingerprint,
    };
    let expires_at_epoch = now_epoch.saturating_add(PAIRING_LIFETIME_SECS);
    relay.publish_offer(
        device_credential,
        PairingOffer {
            locator: code.locator(),
            workspace_id: workspace_id.to_string(),
            host_public_key: public_key,
            expires_at_epoch,
        },
    )?;
    Ok(HostPairing {
        code,
        secret,
        public_key,
        expires_at_epoch,
    })
}

pub fn start_join(
    relay: &mut dyn PairingRelay,
    code: &str,
    friendly_name: &str,
    now_epoch: u64,
) -> Result<JoinAttempt> {
    validate_friendly_name(friendly_name)?;
    let code = PairingCode::parse(code)?;
    let secret = StaticSecret::random_from_rng(OsRng);
    let joiner_public_key = PublicKey::from(&secret).to_bytes();
    let request_id = random_hex(16);
    let device_id = random_hex(16);
    let offer = relay.request_join(
        &code.locator(),
        JoinRequest {
            request_id: request_id.clone(),
            joiner_public_key,
            device_id: device_id.clone(),
        },
        now_epoch,
    )?;
    if offer.expires_at_epoch < now_epoch {
        return Err(SetupError::Expired);
    }
    if fingerprint(&offer.host_public_key) != code.host_fingerprint {
        return Err(SetupError::HostAuthentication);
    }
    Ok(JoinAttempt {
        code,
        secret,
        host_public_key: offer.host_public_key,
        request_id,
        device_id,
        friendly_name: friendly_name.to_string(),
        expires_at_epoch: offer.expires_at_epoch,
    })
}

pub fn complete_host_pairing(
    store: &Store,
    relay: &mut dyn PairingRelay,
    pairing: &HostPairing,
    now_epoch: u64,
) -> Result<bool> {
    if pairing.expires_at_epoch < now_epoch {
        return Err(SetupError::Expired);
    }
    let secrets = load_local_secrets(store)?;
    let Some(request) = relay.pending_join(
        &secrets.device_credential,
        &pairing.code.locator(),
        now_epoch,
    )?
    else {
        return Ok(false);
    };
    let joined_device_credential = random_hex(32);
    let mut payload = GrantPayload {
        workspace_id: secrets.workspace_id.clone(),
        workspace_key: *secrets.workspace_key.as_bytes(),
        device_credential: joined_device_credential.clone(),
    };
    let grant = encrypt_grant(pairing, &request, &pairing.code, &payload)?;
    payload.workspace_key.zeroize();
    relay.complete_join(
        &secrets.device_credential,
        &pairing.code.locator(),
        &request.request_id,
        &secrets.workspace_id,
        &joined_device_credential,
        grant,
        now_epoch,
    )?;
    Ok(true)
}

pub fn finish_join(
    store: &Store,
    relay: &mut dyn PairingRelay,
    attempt: &JoinAttempt,
    home: &Path,
    now_epoch: u64,
) -> Result<JoinOutcome> {
    finish_join_with_progress(store, relay, attempt, home, now_epoch, &mut |_| {})
}

pub fn finish_join_with_progress(
    store: &Store,
    relay: &mut dyn PairingRelay,
    attempt: &JoinAttempt,
    home: &Path,
    now_epoch: u64,
    progress: &mut dyn FnMut(CollectProgress),
) -> Result<JoinOutcome> {
    validate_fresh(store)?;
    if attempt.expires_at_epoch < now_epoch {
        return Err(SetupError::Expired);
    }
    let grant = relay.take_grant(&attempt.code.locator(), &attempt.request_id, now_epoch)?;
    let mut payload = decrypt_grant(attempt, &grant)?;
    persist_identity(
        store,
        &payload.workspace_id,
        &attempt.device_id,
        &attempt.friendly_name,
        &payload.device_credential,
        &payload.workspace_key,
    )?;
    let workspace_id = payload.workspace_id.clone();
    payload.workspace_key.zeroize();
    let (detected_sources, imports) = import_history(
        store,
        home,
        &workspace_id,
        &attempt.device_id,
        now_epoch,
        progress,
    )?;
    Ok(JoinOutcome {
        workspace_id,
        device_id: attempt.device_id.clone(),
        detected_sources,
        imports,
    })
}

pub fn load_sync_config(store: &Store, download_limit: usize) -> Result<SyncConfig> {
    let secrets = load_local_secrets(store)?;
    let device_id = store
        .get_metadata(DEVICE_ID)?
        .ok_or(SetupError::NotInitialized)?;
    Ok(SyncConfig {
        workspace_id: secrets.workspace_id,
        device_id,
        device_credential: secrets.device_credential,
        key: secrets.workspace_key,
        download_limit,
    })
}

pub fn is_initialized(store: &Store) -> Result<bool> {
    Ok(store.get_metadata(SETUP_COMPLETE)?.as_deref() == Some("1"))
}

pub fn render_init(outcome: &InitOutcome) -> String {
    let sources = if outcome.detected_sources.is_empty() {
        "none".to_string()
    } else {
        outcome.detected_sources.join(", ")
    };
    let imported: u64 = outcome
        .imports
        .iter()
        .map(|item| item.events_imported)
        .sum();
    format!(
        "Workspace created\nMachine: {}\nDetected sources: {sources}\nImported: {imported} usage records\nScheduler: automatic collection is not installed yet\nPair another machine within 10 minutes:\n  aiu join {}\n",
        outcome.device_id, outcome.pairing_code
    )
}

pub fn render_join(outcome: &JoinOutcome) -> String {
    let sources = if outcome.detected_sources.is_empty() {
        "none".to_string()
    } else {
        outcome.detected_sources.join(", ")
    };
    let imported: u64 = outcome
        .imports
        .iter()
        .map(|item| item.events_imported)
        .sum();
    format!(
        "Joined workspace\nMachine: {}\nDetected sources: {sources}\nImported: {imported} usage records\n",
        outcome.device_id
    )
}

struct LocalSecrets {
    workspace_id: String,
    device_credential: String,
    workspace_key: WorkspaceKey,
}

fn load_local_secrets(store: &Store) -> Result<LocalSecrets> {
    if store.get_metadata(SETUP_COMPLETE)?.as_deref() != Some("1") {
        return Err(SetupError::NotInitialized);
    }
    let workspace_id = store
        .get_metadata(WORKSPACE_ID)?
        .ok_or(SetupError::NotInitialized)?;
    let device_credential = store
        .get_metadata(DEVICE_CREDENTIAL)?
        .ok_or(SetupError::NotInitialized)?;
    let key = store
        .get_metadata(WORKSPACE_KEY)?
        .and_then(|value| decode_array::<32>(&value))
        .ok_or(SetupError::NotInitialized)?;
    Ok(LocalSecrets {
        workspace_id,
        device_credential,
        workspace_key: WorkspaceKey::from_bytes(key),
    })
}

fn validate_fresh(store: &Store) -> Result<()> {
    if store.get_metadata(SETUP_COMPLETE)?.is_some() {
        Err(SetupError::AlreadyInitialized)
    } else {
        Ok(())
    }
}

fn validate_friendly_name(value: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(SetupError::InvalidFriendlyName)
    } else {
        Ok(())
    }
}

fn persist_identity(
    store: &Store,
    workspace_id: &str,
    device_id: &str,
    friendly_name: &str,
    device_credential: &str,
    workspace_key: &[u8; 32],
) -> Result<()> {
    store.set_metadata(WORKSPACE_ID, workspace_id)?;
    store.set_metadata(DEVICE_ID, device_id)?;
    store.set_metadata(DEVICE_CREDENTIAL, device_credential)?;
    store.set_metadata(WORKSPACE_KEY, &hex(workspace_key))?;
    store.upsert_local_device(&NewDevice {
        device_id: device_id.to_string(),
        friendly_name: friendly_name.trim().to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        last_sync_at_utc: None,
    })?;
    store.set_metadata(SETUP_COMPLETE, "1")?;
    Ok(())
}

fn import_history(
    store: &Store,
    home: &Path,
    workspace_id: &str,
    device_id: &str,
    now_epoch: u64,
    progress: &mut dyn FnMut(CollectProgress),
) -> Result<(Vec<&'static str>, Vec<SourceCollect>)> {
    let detections = crate::sources::detect(home);
    let detected_sources = detections
        .iter()
        .filter(|item| item.detected)
        .map(|item| item.source)
        .collect();
    let context = crate::adapters::IngestContext {
        device_id: device_id.to_string(),
        workspace_id: workspace_id.to_string(),
        now_epoch,
    };
    let imports = crate::collect::collect_detected_with_progress(store, home, &context, progress)?;
    Ok((detected_sources, imports))
}

fn encrypt_grant(
    pairing: &HostPairing,
    request: &JoinRequest,
    code: &PairingCode,
    payload: &GrantPayload,
) -> Result<EncryptedGrant> {
    let peer = PublicKey::from(request.joiner_public_key);
    let shared = pairing.secret.diffie_hellman(&peer);
    let mut key = derive_pairing_key(
        shared.as_bytes(),
        code,
        &pairing.public_key,
        &request.joiner_public_key,
    )?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let aad = pairing_aad(
        code,
        &request.request_id,
        &pairing.public_key,
        &request.joiner_public_key,
    );
    let mut plaintext = serde_json::to_vec(payload)?;
    let encrypted = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| SetupError::GrantAuthentication);
    key.zeroize();
    plaintext.zeroize();
    let ciphertext = encrypted?;
    Ok(EncryptedGrant {
        nonce: nonce.into(),
        ciphertext,
    })
}

fn decrypt_grant(attempt: &JoinAttempt, grant: &EncryptedGrant) -> Result<GrantPayload> {
    let host = PublicKey::from(attempt.host_public_key);
    let shared = attempt.secret.diffie_hellman(&host);
    let mut key = derive_pairing_key(
        shared.as_bytes(),
        &attempt.code,
        &attempt.host_public_key,
        PublicKey::from(&attempt.secret).as_bytes(),
    )?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let aad = pairing_aad(
        &attempt.code,
        &attempt.request_id,
        &attempt.host_public_key,
        PublicKey::from(&attempt.secret).as_bytes(),
    );
    let decrypted = cipher
        .decrypt(
            XNonce::from_slice(&grant.nonce),
            Payload {
                msg: &grant.ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| SetupError::GrantAuthentication);
    key.zeroize();
    let mut plaintext = decrypted?;
    let parsed = serde_json::from_slice(&plaintext);
    plaintext.zeroize();
    Ok(parsed?)
}

fn derive_pairing_key(
    shared: &[u8; 32],
    code: &PairingCode,
    host_public: &[u8; 32],
    joiner_public: &[u8; 32],
) -> Result<[u8; 32]> {
    let mut salt = Vec::with_capacity(18);
    salt.extend_from_slice(&code.locator);
    salt.extend_from_slice(&code.host_fingerprint);
    let hkdf = Hkdf::<Sha256>::new(Some(&salt), shared);
    let mut info = b"aiu-pairing-grant-v1\0".to_vec();
    info.extend_from_slice(host_public);
    info.extend_from_slice(joiner_public);
    let mut output = [0; 32];
    hkdf.expand(&info, &mut output)
        .map_err(|_| SetupError::GrantAuthentication)?;
    Ok(output)
}

fn pairing_aad(
    code: &PairingCode,
    request_id: &str,
    host_public: &[u8; 32],
    joiner_public: &[u8; 32],
) -> Vec<u8> {
    let mut aad = b"aiu-pairing-aad-v1\0".to_vec();
    aad.extend_from_slice(&code.locator);
    aad.extend_from_slice(request_id.as_bytes());
    aad.extend_from_slice(host_public);
    aad.extend_from_slice(joiner_public);
    aad
}

fn fingerprint(public_key: &[u8; 32]) -> [u8; 10] {
    let digest = Sha256::digest(public_key);
    let mut output = [0; 10];
    output.copy_from_slice(&digest[..10]);
    output
}

fn random_array<const N: usize>() -> [u8; N] {
    use chacha20poly1305::aead::rand_core::RngCore;
    let mut bytes = [0; N];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

fn random_hex(bytes: usize) -> String {
    use chacha20poly1305::aead::rand_core::RngCore;
    let mut value = vec![0; bytes];
    OsRng.fill_bytes(&mut value);
    hex(&value)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let pair = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(pair, 16).ok()
        })
        .collect()
}

fn decode_array<const N: usize>(value: &str) -> Option<[u8; N]> {
    let bytes = decode_hex(value)?;
    bytes.try_into().ok()
}
