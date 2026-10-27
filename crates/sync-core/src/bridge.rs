use serde::{Deserialize, Serialize};
use std::{path::Path, process::Command};

/// Data channels may reuse the authenticated master, but must never reconnect
/// implicitly. A missing control socket must fail before opening a transport.
pub fn multiplex_command(host: &str, args: &[String], askpass: &Path, config: &Path) -> Command {
    let mut command = Command::new("ssh");
    command
        .args([
            "-o",
            "ControlMaster=no",
            "-o",
            "BatchMode=yes",
            "-o",
            "NumberOfPasswordPrompts=0",
            "-o",
            "ProxyCommand=false",
        ])
        .args(args)
        .arg(host)
        .env("SSH_ASKPASS", askpass)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env("SYNCUI_BRIDGE_CONFIG", config);
    if std::env::var_os("APPIMAGE").is_some() || std::env::var_os("APPDIR").is_some() {
        command
            .env_remove("LD_LIBRARY_PATH")
            .env_remove("LD_PRELOAD");
    }
    command
}
#[derive(Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    pub socket: String,
    pub token: String,
    pub ssh_args: Vec<String>,
    pub host: String,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Message {
    Auth { token: String, prompt: String },
    Bytes { token: String, count: u64 },
}
#[derive(Serialize, Deserialize)]
pub struct AuthReply {
    pub answer: Option<String>,
}
