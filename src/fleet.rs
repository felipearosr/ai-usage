//! Fleet mutations that preserve history and propagate through encrypted sync.

use std::fmt;

use crate::store::Store;
use crate::sync::{enqueue_record, DeviceRevocation, RelayClient, SyncConfig, SyncRecord};

#[derive(Debug)]
pub enum FleetError {
    Store(crate::error::AiuError),
    InvalidName,
    NotFound(String),
    AmbiguousName(String),
    CannotRemoveCurrent,
    Relay(crate::sync::RelayError),
}

impl fmt::Display for FleetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FleetError::Store(error) => write!(f, "fleet storage failed: {error}"),
            FleetError::InvalidName => write!(f, "machine name cannot be empty"),
            FleetError::NotFound(device) => write!(f, "machine not found: {device}"),
            FleetError::AmbiguousName(name) => {
                write!(f, "machine name is ambiguous; use its device id: {name}")
            }
            FleetError::CannotRemoveCurrent => write!(f, "cannot remove the current machine"),
            FleetError::Relay(error) => write!(f, "machine revocation failed: {error}"),
        }
    }
}

impl From<crate::sync::RelayError> for FleetError {
    fn from(error: crate::sync::RelayError) -> Self {
        FleetError::Relay(error)
    }
}

impl std::error::Error for FleetError {}

impl From<crate::error::AiuError> for FleetError {
    fn from(error: crate::error::AiuError) -> Self {
        FleetError::Store(error)
    }
}

pub fn remove_machine(
    store: &Store,
    relay: &mut dyn RelayClient,
    config: &SyncConfig,
    device: &str,
) -> Result<String, FleetError> {
    let device_id = resolve_device(store, device)?;
    if device_id == config.device_id {
        return Err(FleetError::CannotRemoveCurrent);
    }
    let revoked_at_utc = crate::utc::now_rfc3339();
    let record = SyncRecord::DeviceRevocation(Box::new(DeviceRevocation {
        workspace_id: config.workspace_id.clone(),
        device_id: device_id.clone(),
        revoked_at_utc: revoked_at_utc.clone(),
    }));
    relay.revoke_device(&config.device_credential, &config.workspace_id, &device_id)?;
    enqueue_record(store, &record)?;
    store.mark_device_revoked(&device_id, &revoked_at_utc)?;
    Ok(device_id)
}

pub fn rename_machine(
    store: &Store,
    workspace_id: &str,
    device: &str,
    new_name: &str,
) -> Result<String, FleetError> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return Err(FleetError::InvalidName);
    }
    let device_id = resolve_device(store, device)?;
    store.rename_device(&device_id, new_name, &crate::utc::now_rfc3339())?;
    let state = store.device_sync_state(workspace_id, &device_id, None)?;
    enqueue_record(store, &SyncRecord::DeviceState(Box::new(state)))?;
    Ok(device_id)
}

fn resolve_device(store: &Store, device: &str) -> Result<String, FleetError> {
    let ids = store.device_ids_matching(device)?;
    match ids.as_slice() {
        [] => Err(FleetError::NotFound(device.to_string())),
        [id] => Ok(id.clone()),
        _ => Err(FleetError::AmbiguousName(device.to_string())),
    }
}
