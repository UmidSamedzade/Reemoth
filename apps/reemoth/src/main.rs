mod check;
mod client;
mod config;
mod host;
mod monitor;
mod render;
mod signal_client;
mod stream;
mod viewer;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use reemoth_proto::signal::{HostId, Role, SignalMsg};

use crate::config::ConfigFile;

#[derive(Parser)]
#[command(version, about = "Low-latency peer-to-peer remote desktop")]
struct Cli {
    /// Signaling server URL, e.g. wss://reemoth-signaling.example.workers.dev
    /// (saved for next time).
    #[arg(long, global = true)]
    signal: Option<String>,
    /// Config directory (defaults to the per-user app config folder).
    #[arg(long, global = true, env = "REEMOTH_CONFIG_DIR")]
    config_dir: Option<PathBuf>,
    /// More logging (-v info, -vv debug).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Make this PC reachable: register its ID and accept password-authenticated connections.
    Host,
    /// Set or change this host's password (or set REEMOTH_PASSWORD).
    SetPassword,
    /// Connect to a host and test the peer-to-peer link.
    Connect {
        /// The host's nine-digit ID (spaces allowed).
        id: String,
        /// Password (prompted if omitted; or set REEMOTH_PASSWORD).
        #[arg(long)]
        password: Option<String>,
        /// Offer only the TURN relay candidate, to test the relay path.
        #[arg(long)]
        relay_only: bool,
        /// Number of latency pings (0 to skip).
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(0..=10_000))]
        pings: u32,
        /// MiB to send for the throughput test (0 to skip).
        #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u32).range(0..=4096))]
        bulk_mib: u32,
        /// Exit after the tests instead of opening the chat prompt.
        #[arg(long)]
        no_chat: bool,
    },
    /// Show a host's screen in a window.
    View {
        /// The host's nine-digit ID (spaces allowed).
        id: String,
        /// Password (prompted if omitted; or set REEMOTH_PASSWORD).
        #[arg(long)]
        password: Option<String>,
        /// Offer only the TURN relay candidate, to test the relay path.
        #[arg(long)]
        relay_only: bool,
    },
    /// Test what this PC can do: screen capture, encoders, decoder, GPU, network.
    Check,
    /// Show this PC's host ID and settings.
    Info,
    /// Round-trip pings to the signaling server.
    SignalPing {
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..=1000))]
        count: u32,
    },
}

fn main() {
    // rustls cannot pick a crypto backend by itself when none is the default.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let cli = Cli::parse();
    let filter = match cli.verbose {
        0 => "warn,wgpu_hal=error,wgpu_core=error",
        1 => "info,str0m=warn",
        _ => "debug",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_writer(std::io::stderr)
        .init();
    if let Err(e) = run(cli) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let dir = match cli.config_dir {
        Some(dir) => dir,
        None => ConfigFile::default_dir()?,
    };
    let mut cfg = ConfigFile::load(&dir)?;
    let signal_url = cfg.resolve_signal_url(cli.signal)?;
    let rt = tokio::runtime::Runtime::new().context("starting async runtime")?;

    match cli.command {
        // The window needs the main thread, so the viewer drives the runtime itself.
        Command::View {
            id,
            password,
            relay_only,
        } => {
            let id = HostId::parse(&id).context("host ID must be nine digits")?;
            let target = client::Target {
                signal_url,
                id,
                password: password_for(id, password)?,
                relay_only,
            };
            viewer::run(rt, target)
        }
        command => rt.block_on(run_async(&mut cfg, signal_url, command)),
    }
}

fn password_for(id: HostId, flag: Option<String>) -> anyhow::Result<String> {
    match flag.or_else(|| std::env::var("REEMOTH_PASSWORD").ok()) {
        Some(pw) => Ok(pw),
        None => Ok(rpassword::prompt_password(format!(
            "Password for {}: ",
            id.grouped()
        ))?),
    }
}

async fn run_async(
    cfg: &mut ConfigFile,
    signal_url: String,
    command: Command,
) -> anyhow::Result<()> {
    match command {
        Command::View { .. } => unreachable!("handled before the runtime starts"),
        Command::Host => {
            if cfg.host_password()?.is_none() {
                println!("No password set yet. Clients will need it to connect.");
                let pw = host::prompt_new_password()?;
                cfg.set_host_password(&pw)?;
            }
            host::run(cfg, host::HostOptions { signal_url }).await
        }
        Command::SetPassword => {
            let pw = host::prompt_new_password()?;
            cfg.set_host_password(&pw)?;
            println!("Password saved. Restart `reemoth host` if it is running.");
            Ok(())
        }
        Command::Connect {
            id,
            password,
            relay_only,
            pings,
            bulk_mib,
            no_chat,
        } => {
            let id = HostId::parse(&id).context("host ID must be nine digits")?;
            client::run(client::ConnectOptions {
                target: client::Target {
                    signal_url,
                    id,
                    password: password_for(id, password)?,
                    relay_only,
                },
                pings,
                bulk_mib,
                chat: !no_chat,
            })
            .await
        }
        Command::Info => {
            println!("config:    {}", cfg.path().display());
            println!("signaling: {signal_url}");
            match &cfg.config.host {
                Some(h) => {
                    let id = HostId::new(h.id).context("invalid host ID in config")?;
                    println!("host ID:   {}", id.grouped());
                    println!(
                        "password:  {}",
                        if h.password.is_some() {
                            "set"
                        } else {
                            "not set"
                        }
                    );
                }
                None => println!("host ID:   none yet (run `reemoth host`)"),
            }
            Ok(())
        }
        Command::SignalPing { count } => signal_ping(&signal_url, count).await,
        Command::Check => check::run(&signal_url).await,
    }
}

/// Pings the server through an unauthenticated host-role socket in a scratch room.
async fn signal_ping(url: &str, count: u32) -> anyhow::Result<()> {
    let id = HostId::new(1).expect("valid");
    let mut conn = signal_client::SignalConn::connect(url, id, Role::Host).await?;
    let SignalMsg::Challenge { .. } = conn.expect().await? else {
        bail!("unexpected greeting from the signaling server");
    };
    let mut samples = Vec::with_capacity(count as usize);
    for nonce in 1..=u64::from(count) {
        let start = Instant::now();
        conn.send(SignalMsg::Ping { nonce })?;
        let reply = tokio::time::timeout(Duration::from_secs(5), conn.expect())
            .await
            .context("ping timed out")??;
        if reply != (SignalMsg::Pong { nonce }) {
            bail!("expected pong {nonce}, got {reply:?}");
        }
        let rtt = start.elapsed();
        println!("pong in {:.2} ms", rtt.as_secs_f64() * 1e3);
        samples.push(rtt);
    }
    if let (Some(min), Some(max)) = (samples.iter().min(), samples.iter().max()) {
        println!("{count} pings: min {min:.2?}, max {max:.2?}");
    }
    Ok(())
}
