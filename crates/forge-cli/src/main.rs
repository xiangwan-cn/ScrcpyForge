use clap::{Parser, Subcommand};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "forge", version, about = "ScrcpyForge backend client")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Health,
    Devices,
    Scan,
    Connect { endpoint: String },
    Tap { serial: String, x: i32, y: i32 },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let base = format!(
        "http://{}/api/v1",
        std::env::var("SCRCPYFORGE_ADDR").unwrap_or_else(|_| "127.0.0.1:27180".into())
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()?;
    let value: Option<serde_json::Value> = match args.command {
        Command::Health => Some(
            client
                .get(format!("{base}/health"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?,
        ),
        Command::Devices => Some(
            client
                .get(format!("{base}/devices"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?,
        ),
        Command::Scan => Some(
            client
                .post(format!("{base}/devices/scan"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?,
        ),
        Command::Connect { endpoint } => Some(
            client
                .post(format!("{base}/devices/connect"))
                .json(&serde_json::json!({"endpoint": endpoint}))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?,
        ),
        Command::Tap { serial, x, y } => {
            client
                .post(format!(
                    "{base}/devices/{}/input",
                    encode_path_segment(&serial)
                ))
                .json(&serde_json::json!({"type":"tap","x":x,"y":y}))
                .send()
                .await?
                .error_for_status()?;
            None
        }
    };
    if let Some(value) = value {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(())
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::encode_path_segment;

    #[test]
    fn encodes_serial_path_segments() {
        assert_eq!(
            encode_path_segment("[fe80::1]:5555"),
            "%5Bfe80%3A%3A1%5D%3A5555"
        );
    }
}
