//! Configuration files shared by the roles.
//!
//! The bridge hands each helper a private runtime config holding only what
//! the helper needs: `{"enabled", "config_dir", "state_dir", "sessions"}`
//! (no TLS material). The bridge and server parse the rest of their own
//! configs; this module holds what several roles share.

use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::fsutil;
use crate::protocol::FRAME_LIMIT;

/// The server's address in a bridge config: `server_url`, or `hub_url` in
/// configs written by earlier versions.
pub fn server_url(config: &Map<String, Value>) -> Option<&Value> {
    config.get("server_url").or_else(|| config.get("hub_url"))
}

/// Read the bridge's private runtime config (owned, mode 0600, not a link)
/// and refuse it unless `enabled` is exactly `true`.
pub fn read_runtime(path: &str) -> Result<Map<String, Value>> {
    let config = fsutil::read_json(path, FRAME_LIMIT, true)?;
    if config.get("enabled") != Some(&Value::Bool(true)) {
        return Err(Error::value("bridge disabled"));
    }
    Ok(config)
}
