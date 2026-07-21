use clap::{Parser, Subcommand};

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
    let client = reqwest::Client::new();
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
                .post(format!("{base}/devices/{serial}/input"))
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
