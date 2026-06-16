use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::agent::persist::{Snapshot, UiState, state_dir};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    GetState,
    Subscribe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Response {
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

pub fn get_state() -> Result<(Option<Snapshot>, UiState)> {
    let mut stream = UnixStream::connect(socket_path()).context("connect daemon socket")?;
    stream
        .set_read_timeout(Some(Duration::from_millis(150)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_millis(150)))
        .ok();

    let request = serde_json::to_string(&Request::GetState).context("encode daemon request")?;
    writeln!(stream, "{request}").context("write daemon request")?;

    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("read daemon response")?;
    let response: Response = serde_json::from_str(&line).context("decode daemon response")?;
    match response {
        Response::State { snapshot, ui_state } => Ok((snapshot, ui_state)),
        Response::Error { message } => Err(anyhow!(message)),
    }
}
