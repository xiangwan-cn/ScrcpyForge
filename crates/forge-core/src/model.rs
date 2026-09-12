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

impl InputAction {
    /// Validate limits that are independent of a device's current dimensions.
    /// Coordinate upper bounds are checked by ScrcpySession once its size is
    /// known; ADB fallback input still gets the resource and sign checks here.
    pub fn validate(&self) -> Result<(), String> {
        const MAX_DURATION_MS: u64 = 60_000;
        const MAX_TEXT_BYTES: usize = 4 * 1024;
        match self {
            Self::Tap { x, y } => {
                if *x < 0 || *y < 0 || *x > 65_535 || *y > 65_535 {
                    return Err("tap coordinates must be between 0 and 65535".into());
                }
            }
            Self::Swipe {
                x1,
                y1,
                x2,
                y2,
                duration_ms,
            } => {
                if [*x1, *y1, *x2, *y2]
                    .iter()
                    .any(|value| *value < 0 || *value > 65_535)
                {
                    return Err("swipe coordinates must be between 0 and 65535".into());
                }
                if !(1..=MAX_DURATION_MS).contains(duration_ms) {
                    return Err(format!(
                        "swipe duration_ms must be between 1 and {MAX_DURATION_MS}"
                    ));
                }
            }
            Self::Text { value } => {
                if value.len() > MAX_TEXT_BYTES {
                    return Err(format!("text must be at most {MAX_TEXT_BYTES} bytes"));
                }
            }
            Self::Key { code } => {
                if !(0..=0xffff).contains(code) {
                    return Err("key code must be between 0 and 65535".into());
                }
            }
        }
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::InputAction;

    #[test]
    fn input_limits_reject_unbounded_values() {
        assert!(InputAction::Tap { x: -1, y: 0 }.validate().is_err());
        assert!(InputAction::Tap { x: 65_536, y: 0 }.validate().is_err());
        assert!(InputAction::Swipe {
            x1: 0,
            y1: 0,
            x2: 1,
            y2: 1,
            duration_ms: 60_001,
        }
        .validate()
        .is_err());
        assert!(InputAction::Text {
            value: "x".repeat(4097),
        }
        .validate()
        .is_err());
        assert!(InputAction::Key { code: -1 }.validate().is_err());
    }
}
