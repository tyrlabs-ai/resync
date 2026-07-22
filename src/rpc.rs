use crate::state::state_paths;
use anyhow::{Result, bail};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

pub fn rpc(request: &Value, socket_path: Option<&Path>) -> Result<Value> {
    let path = socket_path
        .map(Path::to_owned)
        .unwrap_or_else(|| state_paths().socket);
    let mut stream = UnixStream::connect(&path).map_err(|error| {
        anyhow::anyhow!("daemon unavailable: {error}; start it with `resync daemon`")
    })?;
    serde_json::to_writer(&mut stream, request)?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let response: Value = serde_json::from_str(&line)?;
    if response.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    } else {
        bail!(
            "{}",
            response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("daemon request failed")
        )
    }
}
