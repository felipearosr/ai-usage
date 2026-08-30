use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use aiu::relay::HttpRelayClient;
use aiu::setup::{JoinRequest, PairingOffer, PairingRelay};
use aiu::sync::{EncryptedRecord, RelayClient};

fn serve_once(response: &'static str) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let read = stream.read(&mut buffer).unwrap();
            request.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&request);
            let Some(header_end) = text.find("\r\n\r\n") else {
                continue;
            };
            let content_length = text[..header_end]
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length: ")
                        .or_else(|| line.strip_prefix("Content-Length: "))
                })
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            if request.len() >= header_end + 4 + content_length {
                break;
            }
        }
        let body = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        );
        stream.write_all(body.as_bytes()).unwrap();
        String::from_utf8(request).unwrap()
    });
    (format!("http://{address}"), handle)
}

#[test]
fn pairing_client_sends_the_minimal_join_request() {
    let response = serde_json::to_string(&PairingOffer {
        locator: "0011223344556677".into(),
        workspace_id: "opaque-workspace".into(),
        host_public_key: [7; 32],
        expires_at_epoch: 700,
    })
    .unwrap();
    let response: &'static str = Box::leak(response.into_boxed_str());
    let (base_url, server) = serve_once(response);
    let mut relay = HttpRelayClient::new(&base_url).unwrap();

    let offer = relay
        .request_join(
            "0011223344556677",
            JoinRequest {
                request_id: "request-1".into(),
                joiner_public_key: [9; 32],
                device_id: "device-2".into(),
            },
            100,
        )
        .unwrap();

    assert_eq!(offer.workspace_id, "opaque-workspace");
    let request = server.join().unwrap();
    assert!(request.starts_with("POST /v1/pairing/0011223344556677/requests HTTP/1.1"));
    let body = request.split_once("\r\n\r\n").unwrap().1;
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(json["request"]["device_id"], "device-2");
    assert!(!request.contains("workspace_key"));
}

#[test]
fn plaintext_transport_is_limited_to_loopback_testing() {
    assert!(HttpRelayClient::new("http://relay.example.com").is_err());
    assert!(HttpRelayClient::new("http://localhost.attacker.example").is_err());
    assert!(HttpRelayClient::new("http://127.0.0.1.attacker.example").is_err());
    assert!(HttpRelayClient::new("http://127.0.0.1:1234").is_ok());
    assert!(HttpRelayClient::new("https://relay.aiu.sh").is_ok());
}

#[test]
fn sync_upload_sends_only_opaque_records_with_device_auth() {
    let (base_url, server) = serve_once("{}");
    let mut relay = HttpRelayClient::new(&base_url).unwrap();
    relay
        .upload(
            "device-secret",
            &[EncryptedRecord {
                workspace_id: "opaque-workspace".into(),
                record_id: "opaque-record".into(),
                nonce: [3; 24],
                ciphertext: vec![8, 9, 10],
            }],
        )
        .unwrap();

    let request = server.join().unwrap();
    assert!(request.starts_with("POST /v1/records/upload HTTP/1.1"));
    assert!(request.contains("authorization: Bearer device-secret"));
    assert!(request.contains("\"opaque-workspace\""));
    assert!(!request.contains("model"));
    assert!(!request.contains("machine"));
}
