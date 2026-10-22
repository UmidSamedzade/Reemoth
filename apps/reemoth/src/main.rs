mod signal_client;

use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use reemoth_proto::signal::HostId;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Round-trip pings to a host's signaling room and report latency.
    SignalPing {
        /// Signaling server base URL, e.g. wss://signal.example.com
        #[arg(
            long,
            env = "REEMOTH_SIGNAL_URL",
            default_value = "ws://127.0.0.1:8787"
        )]
        server: String,
        /// Nine-digit host ID.
        #[arg(long, default_value = "000000001")]
        id: String,
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..=1000))]
        count: u32,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::SignalPing { server, id, count } => {
            let id = HostId::parse(&id).context("host ID must be nine digits")?;
            let mut client = signal_client::SignalClient::connect(&server, id).await?;
            let mut samples = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let rtt = tokio::time::timeout(Duration::from_secs(5), client.ping())
                    .await
                    .context("ping timed out")??;
                println!("pong in {:.2} ms", rtt.as_secs_f64() * 1e3);
                samples.push(rtt);
            }
            if let (Some(min), Some(max)) = (samples.iter().min(), samples.iter().max()) {
                println!("{count} pings: min {min:.2?}, max {max:.2?}");
            }
            client.close().await
        }
    }
}
