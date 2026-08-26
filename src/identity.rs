//! Local machine identity bootstrap.
//!
//! A stable `device_id` and `workspace_id` are minted on first use and
//! persisted in the metadata table, so repeated runs attribute usage to the
//! same machine. This is the storage-level identity only — the full
//! workspace lifecycle (credentials, encryption keys, friendly name, pairing)
//! is `aiu init`/`aiu join` (issue 09), which builds on these ids.
//!
//! Ids are 128-bit random values from the OS CSPRNG; they are opaque
//! identifiers, never derived from paths, hostnames, or anything identifying.

use std::io::Read;

use crate::error::Result;
use crate::store::Store;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalIdentity {
    pub device_id: String,
    pub workspace_id: String,
}

/// Returns the persisted identity, minting it once when absent. Idempotent:
/// a second call returns the exact same ids and creates no duplicates.
pub fn ensure_local_identity(store: &Store) -> Result<LocalIdentity> {
    let device_id = get_or_mint(store, "device_id")?;
    let workspace_id = get_or_mint(store, "workspace_id")?;
    Ok(LocalIdentity {
        device_id,
        workspace_id,
    })
}

/// Reads a metadata key, minting and persisting a random value when absent.
fn get_or_mint(store: &Store, key: &str) -> Result<String> {
    if let Some(existing) = store.get_metadata(key)? {
        return Ok(existing);
    }
    let fresh = random_hex(16)?;
    store.set_metadata(key, &fresh)?;
    Ok(fresh)
}

/// Reads `bytes` random bytes from the OS CSPRNG and hex-encodes them.
fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_across_calls_and_persists() {
        let store = Store::open_in_memory().unwrap();
        let first = ensure_local_identity(&store).unwrap();
        let second = ensure_local_identity(&store).unwrap();

        assert_eq!(first, second, "ids are minted once, never regenerated");
        assert_eq!(first.device_id.len(), 32);
        assert_eq!(first.workspace_id.len(), 32);
        assert_ne!(first.device_id, first.workspace_id);

        // Distinct stores get distinct identities (no accidental reuse).
        let other = Store::open_in_memory().unwrap();
        let third = ensure_local_identity(&other).unwrap();
        assert_ne!(first.device_id, third.device_id);
        assert_ne!(first.workspace_id, third.workspace_id);
    }
}
