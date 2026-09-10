use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DeviceState {
    Device,
    Offline,
    Unauthorized,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceInfo {
    pub serial: String,
    pub state: DeviceState,
    pub product: Option<String>,
    pub model: Option<String>,
    pub transport_id: Option<String>,
    pub wireless: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairingService {
    pub name: String,
    pub endpoint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputAction {
    Tap {
        x: i32,
        y: i32,
    },
    Swipe {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        duration_ms: u64,
    },
    Text {
        value: String,
    },
    Key {
        code: i32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ForgeEvent {
    DeviceSnapshot { devices: Vec<DeviceInfo> },
    ScriptStarted { run_id: Uuid, serial: String },
    ScriptLog { run_id: Uuid, message: String },
    ScriptStopped { run_id: Uuid, error: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunScriptRequest {
    pub serial: String,
    pub source: String,
    pub name: Option<String>,
}
