//! Blocking HTTPS transport for the hosted opaque relay.

use std::fmt;
use std::time::Duration;

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::setup::{EncryptedGrant, JoinRequest, PairingOffer, PairingRelay, PairingRelayError};
use crate::sync::{DownloadBatch, EncryptedRecord, RelayClient, RelayError};

pub const DEFAULT_RELAY_URL: &str = "https://relay.aiu.sh";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayUrlError;

impl fmt::Display for RelayUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "relay URL must use HTTPS; plain HTTP is allowed only for localhost testing"
        )
    }
}

impl std::error::Error for RelayUrlError {}

pub struct HttpRelayClient {
    base_url: String,
    agent: ureq::Agent,
}

impl HttpRelayClient {
    pub fn new(base_url: &str) -> Result<Self, RelayUrlError> {
        let base_url = base_url.trim_end_matches('/');
        let uri: ureq::http::Uri = base_url.parse().map_err(|_| RelayUrlError)?;
        let is_https = uri.scheme_str() == Some("https");
        let is_loopback = uri.scheme_str() == Some("http")
            && matches!(uri.host(), Some("127.0.0.1" | "localhost" | "::1"));
        if base_url.is_empty() || (!is_https && !is_loopback) {
            return Err(RelayUrlError);
        }
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .https_only(is_https)
            .max_redirects(0)
            .build();
        Ok(Self {
            base_url: base_url.to_string(),
            agent: config.into(),
        })
    }

    pub fn from_env() -> Result<Self, RelayUrlError> {
        let url = std::env::var("AIU_RELAY_URL").unwrap_or_else(|_| DEFAULT_RELAY_URL.into());
        Self::new(&url)
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn post<Body: Serialize, Response: DeserializeOwned>(
        &self,
        path: &str,
        credential: Option<&str>,
        body: &Body,
    ) -> Result<Response, PairingRelayError> {
        let mut request = self
            .agent
            .post(self.endpoint(path))
            .header("Content-Type", "application/json");
        if let Some(credential) = credential {
            request = request.header("Authorization", &format!("Bearer {credential}"));
        }
        let mut response = request.send_json(body).map_err(map_error)?;
        response
            .body_mut()
            .read_json()
            .map_err(|_| PairingRelayError::Unavailable)
    }

    fn post_empty<Body: Serialize>(
        &self,
        path: &str,
        credential: Option<&str>,
        body: &Body,
    ) -> Result<(), PairingRelayError> {
        let mut request = self
            .agent
            .post(self.endpoint(path))
            .header("Content-Type", "application/json");
        if let Some(credential) = credential {
            request = request.header("Authorization", &format!("Bearer {credential}"));
        }
        request.send_json(body).map_err(map_error)?;
        Ok(())
    }
}

fn map_error(error: ureq::Error) -> PairingRelayError {
    match error {
        ureq::Error::StatusCode(status) => match status {
            401 | 403 => PairingRelayError::Unauthorized,
            404 => PairingRelayError::NotFound,
            409 => PairingRelayError::Used,
            410 => PairingRelayError::Expired,
            _ => PairingRelayError::Unavailable,
        },
        _ => PairingRelayError::Unavailable,
    }
}

#[derive(Serialize)]
struct RegisterWorkspace<'a> {
    workspace_id: &'a str,
    device_credential: &'a str,
}

#[derive(Serialize)]
struct RequestJoin {
    request: JoinRequest,
    now_epoch: u64,
}

#[derive(Serialize)]
struct PendingJoin {
    now_epoch: u64,
}

#[derive(Deserialize)]
struct PendingJoinResponse {
    request: Option<JoinRequest>,
}

#[derive(Serialize)]
struct CompleteJoin<'a> {
    request_id: &'a str,
    workspace_id: &'a str,
    joined_device_credential: &'a str,
    grant: EncryptedGrant,
    now_epoch: u64,
}

#[derive(Serialize)]
struct TakeGrant<'a> {
    request_id: &'a str,
    now_epoch: u64,
}

#[derive(Serialize)]
struct UploadRecords<'a> {
    records: &'a [EncryptedRecord],
}

#[derive(Serialize)]
struct DownloadRecords<'a> {
    workspace_id: &'a str,
    after_cursor: Option<&'a str>,
    limit: usize,
}

impl PairingRelay for HttpRelayClient {
    fn register_workspace(
        &mut self,
        workspace_id: &str,
        device_credential: &str,
    ) -> Result<(), PairingRelayError> {
        self.post_empty(
            "/v1/workspaces",
            None,
            &RegisterWorkspace {
                workspace_id,
                device_credential,
            },
        )
    }

    fn publish_offer(
        &mut self,
        device_credential: &str,
        offer: PairingOffer,
    ) -> Result<(), PairingRelayError> {
        self.post_empty("/v1/pairing/offers", Some(device_credential), &offer)
    }

    fn request_join(
        &mut self,
        locator: &str,
        request: JoinRequest,
        now_epoch: u64,
    ) -> Result<PairingOffer, PairingRelayError> {
        self.post(
            &format!("/v1/pairing/{locator}/requests"),
            None,
            &RequestJoin { request, now_epoch },
        )
    }

    fn pending_join(
        &mut self,
        device_credential: &str,
        locator: &str,
        now_epoch: u64,
    ) -> Result<Option<JoinRequest>, PairingRelayError> {
        let response: PendingJoinResponse = self.post(
            &format!("/v1/pairing/{locator}/pending"),
            Some(device_credential),
            &PendingJoin { now_epoch },
        )?;
        Ok(response.request)
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
        self.post_empty(
            &format!("/v1/pairing/{locator}/grant"),
            Some(device_credential),
            &CompleteJoin {
                request_id,
                workspace_id,
                joined_device_credential,
                grant,
                now_epoch,
            },
        )
    }

    fn take_grant(
        &mut self,
        locator: &str,
        request_id: &str,
        now_epoch: u64,
    ) -> Result<EncryptedGrant, PairingRelayError> {
        self.post(
            &format!("/v1/pairing/{locator}/take"),
            None,
            &TakeGrant {
                request_id,
                now_epoch,
            },
        )
    }
}

impl RelayClient for HttpRelayClient {
    fn upload(
        &mut self,
        device_credential: &str,
        records: &[EncryptedRecord],
    ) -> Result<(), RelayError> {
        self.post_empty(
            "/v1/records/upload",
            Some(device_credential),
            &UploadRecords { records },
        )
        .map_err(map_sync_error)
    }

    fn download(
        &mut self,
        device_credential: &str,
        workspace_id: &str,
        after_cursor: Option<&str>,
        limit: usize,
    ) -> Result<DownloadBatch, RelayError> {
        self.post(
            "/v1/records/download",
            Some(device_credential),
            &DownloadRecords {
                workspace_id,
                after_cursor,
                limit,
            },
        )
        .map_err(map_sync_error)
    }
}

fn map_sync_error(error: PairingRelayError) -> RelayError {
    match error {
        PairingRelayError::Unauthorized => RelayError::Revoked,
        _ => RelayError::Unavailable,
    }
}
