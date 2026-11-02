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
    let mut stream = UnixStream::connect(socket_path()).context("connect daemon socket")?;
    stream
        .set_read_timeout(Some(Duration::from_millis(150)))
        .ok();
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

    #[test]
    fn status_preserves_flat_build_fields_on_wire() {
        let value = serde_json::json!({
            "type": "Status", "build_id": "future-build", "protocol_version": 999, "healthy": true,
        });
        let response: Response = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&response).unwrap(), value);
        let Response::Status { build, healthy } = response else {
            panic!("expected status")
        };
        assert!(healthy);
        assert_eq!(build.build_id, "future-build");
        assert!(build.check_compatibility().is_err());
    }
}
