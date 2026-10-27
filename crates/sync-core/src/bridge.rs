use serde::{Deserialize, Serialize};
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
