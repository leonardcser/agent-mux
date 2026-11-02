use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::agent::daemon::BuildInfo;
use crate::agent::persist::{Snapshot, UiState, state_dir};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    GetState,
    SubscribeFresh,
    Subscribe,
    Status,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Response {
    Status {
        #[serde(flatten)]
        build: BuildInfo,
        healthy: bool,
    },
    Stopping,
    State {
        snapshot: Option<Snapshot>,
        ui_state: UiState,
    },
    Error {
        message: String,
    },
}

pub fn socket_path() -> PathBuf {
    state_dir().join("daemon.sock")
}

pub fn request(request: &Request) -> Result<Response> {
    let stream = UnixStream::connect(socket_path()).context("connect daemon socket")?;
    exchange(stream, request)
}

fn exchange(mut stream: UnixStream, request: &Request) -> Result<Response> {
    let timeout = if matches!(request, Request::SubscribeFresh) {
        Duration::from_secs(2)
    } else {
        Duration::from_millis(150)
    };
    stream.set_read_timeout(Some(timeout)).ok();
    stream
        .set_write_timeout(Some(Duration::from_millis(150)))
        .ok();

    let request = serde_json::to_string(request).context("encode daemon request")?;
    writeln!(stream, "{request}").context("write daemon request")?;

    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("read daemon response")?;
    serde_json::from_str(&line).context("decode daemon response")
}

pub fn check_compatibility() -> Result<()> {
    match request(&Request::Status)? {
        Response::Status { build, .. } => build.check_compatibility(),
        _ => Err(anyhow!("unexpected daemon status")),
    }
}

pub fn get_state() -> Result<(Option<Snapshot>, UiState)> {
    check_compatibility()?;
    match request(&Request::GetState)? {
        Response::State { snapshot, ui_state } => Ok((snapshot, ui_state)),
        Response::Error { message } => Err(anyhow!(message)),
        _ => Err(anyhow!("unexpected daemon response")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange_response(response: &[u8]) -> Result<Response> {
        let (client, mut server) = UnixStream::pair().unwrap();
        let response = response.to_vec();
        let peer = std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(&server).read_line(&mut line).unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&line).unwrap(),
                serde_json::json!({"type": "Status"})
            );
            server.write_all(&response).unwrap();
        });
        let result = exchange(client, &Request::Status);
        peer.join().unwrap();
        result
    }

    #[test]
    fn socket_round_trip_preserves_wire_fields_and_compatibility() {
        let current = crate::agent::daemon::PROTOCOL_VERSION;
        for protocol_version in [current, current + 1] {
            let value = serde_json::json!({
                "type": "Status", "build_id": "fixture", "healthy": true,
                "protocol_version": protocol_version,
            });
            let mut bytes = serde_json::to_vec(&value).unwrap();
            bytes.push(b'\n');
            let response = exchange_response(&bytes).unwrap();
            assert_eq!(serde_json::to_value(&response).unwrap(), value);
            let Response::Status { build, .. } = response else {
                panic!("expected status")
            };
            assert_eq!(
                build.check_compatibility().is_ok(),
                protocol_version == current
            );
        }
    }

    #[test]
    fn socket_rejects_disconnects_and_malformed_responses() {
        for response in [b"".as_slice(), b"not-json\n", b"{}\n"] {
            let error = exchange_response(response).unwrap_err();
            assert!(error.to_string().contains("decode daemon response"));
        }
    }

    #[test]
    fn socket_read_deadline_bounds_unresponsive_peer() {
        let (client, _server) = UnixStream::pair().unwrap();
        let error = exchange(client, &Request::Status).unwrap_err();
        assert!(error.to_string().contains("read daemon response"));
    }
}
