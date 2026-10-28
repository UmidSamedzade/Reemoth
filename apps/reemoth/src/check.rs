//! `reemoth check`: what this machine can do as a host and as a viewer.
//!
//! Every item is tested, not guessed: encoders encode and their output is
//! decoded, the GPU is asked for a device, STUN servers are asked for our
//! public address.

use std::time::Duration;

use reemoth_net::nat::{NatKind, nat_check};

const OK: &str = "ok  ";
const NO: &str = "--  ";
const WARN: &str = "!!  ";

pub async fn run(signal_url: &str) -> anyhow::Result<()> {
    println!("Reemoth system check\n");

    section("System");
    println!("    OS:       {}", os_name());
    println!("    CPU:      {}", cpu_name());
    println!(
        "    threads:  {}",
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );
    #[cfg(target_os = "linux")]
    println!(
        "    session:  {}",
        std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown".into())
    );

    section("Display & GPU (viewer)");
    let render = tokio::task::spawn_blocking(gpu_report).await?;
    for line in &render.lines {
        println!("    {line}");
    }

    section("Hosting");
    let host_size = host_capture();
    let (w, h) = host_size.unwrap_or((1920, 1080));
    println!("    testing encoders at {w}x{h} (encode + decode check; the test pattern changes");
    println!(
        "    every pixel every frame, so times are worst case: a normal desktop is far cheaper)"
    );
    let reports =
        tokio::task::spawn_blocking(move || reemoth_video::probe_all(w as u32, h as u32)).await?;
    let mut best = None;
    for r in &reports {
        match (&r.error, r.encode_ms) {
            (None, Some(ms)) => {
                println!(
                    "    {OK}encoder {}{}: {ms:.1} ms/frame",
                    r.name,
                    if r.hardware { " (hardware)" } else { "" }
                );
                best.get_or_insert(r.clone());
            }
            (Some(e), _) => println!("    {NO}encoder {}: {e}", r.name),
            _ => {}
        }
    }

    section("Viewing");
    match tokio::task::spawn_blocking(move || reemoth_video::probe_decoder(w as u32, h as u32))
        .await?
    {
        Ok(ms) => println!("    {OK}decoder openh264 (software): {ms:.1} ms/frame at {w}x{h}"),
        Err(e) => println!("    {NO}decoder: {e:#}"),
    }

    section("Network");
    match nat_check().await {
        Ok(nat) => {
            for (server, addr) in &nat.mapped {
                println!("    {OK}{server} sees us as {addr}");
            }
            let (mark, text) = match nat.kind {
                NatKind::Open => (OK, "no NAT: directly reachable"),
                NatKind::EndpointIndependent => {
                    (OK, "NAT allows hole punching: direct P2P should work")
                }
                NatKind::Symmetric => (
                    WARN,
                    "symmetric NAT: direct P2P will likely fail unless the other side is open; a TURN relay is needed",
                ),
                NatKind::Unknown => (WARN, "only one STUN server answered; NAT type unknown"),
                NatKind::UdpBlocked => (
                    NO,
                    "no STUN reply: outgoing UDP seems blocked (relay over TCP not supported yet)",
                ),
            };
            println!("    {mark}{text}");
        }
        Err(e) => println!("    {NO}NAT check failed: {e:#}"),
    }
    match tokio::time::timeout(Duration::from_secs(10), signal_rtt(signal_url)).await {
        Ok(Ok(rtt)) => println!("    {OK}signaling server {signal_url}: {rtt} ms"),
        Ok(Err(e)) => println!("    {NO}signaling server {signal_url}: {e:#}"),
        Err(_) => println!("    {NO}signaling server {signal_url}: timed out"),
    }

    section("Summary");
    match (host_size, &best) {
        (Some(_), Some(enc)) => println!(
            "    {OK}can host: screen capture works, encoding with {}{}",
            enc.name,
            if enc.hardware {
                " (hardware)"
            } else {
                " (software; a GPU encoder would be faster)"
            }
        ),
        (None, _) => {
            println!("    {NO}can't host here yet (screen capture is Windows-only until M5)")
        }
        (Some(_), None) => println!("    {NO}can't host: no working encoder"),
    }
    if render.ok {
        println!("    {OK}can view: {}", render.summary);
    } else {
        println!("    {NO}can't view: {}", render.summary);
    }
    Ok(())
}

fn section(name: &str) {
    println!("\n  {name}");
}

struct GpuReport {
    ok: bool,
    summary: String,
    lines: Vec<String>,
}

fn gpu_report() -> GpuReport {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
    let mut lines = Vec::new();
    for a in instance.enumerate_adapters(wgpu::Backends::all()) {
        let info = a.get_info();
        lines.push(format!(
            "{OK}{} — {:?} via {:?} ({})",
            info.name, info.device_type, info.backend, info.driver
        ));
    }
    if lines.is_empty() {
        lines.push(format!(
            "{NO}no GPU adapter found (Vulkan/OpenGL/D3D12 drivers missing?)"
        ));
    }
    let chosen = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }));
    match chosen {
        Ok(a) => {
            let info = a.get_info();
            let software = info.device_type == wgpu::DeviceType::Cpu;
            GpuReport {
                ok: true,
                summary: format!(
                    "drawing with {} ({:?}){}",
                    info.name,
                    info.backend,
                    if software {
                        " — software rendering, will be slow"
                    } else {
                        ""
                    }
                ),
                lines,
            }
        }
        Err(e) => GpuReport {
            ok: false,
            summary: format!("no usable GPU for the viewer window: {e}"),
            lines,
        },
    }
}

#[cfg(windows)]
fn host_capture() -> Option<(usize, usize)> {
    match reemoth_video::capture::DesktopCapture::primary() {
        Ok(c) => {
            let (w, h) = c.size();
            println!("    {OK}screen capture (DXGI): {w}x{h}");
            Some((w, h))
        }
        Err(e) => {
            println!("    {NO}screen capture: {e:#}");
            None
        }
    }
}

#[cfg(not(windows))]
fn host_capture() -> Option<(usize, usize)> {
    println!("    {NO}screen capture: not implemented on this OS yet (M5)");
    None
}

async fn signal_rtt(url: &str) -> anyhow::Result<u128> {
    use reemoth_proto::signal::{HostId, Role, SignalMsg};
    let mut conn =
        crate::signal_client::SignalConn::connect(url, HostId::new(1).expect("valid"), Role::Host)
            .await?;
    let SignalMsg::Challenge { .. } = conn.expect().await? else {
        anyhow::bail!("unexpected greeting");
    };
    let start = std::time::Instant::now();
    conn.send(SignalMsg::Ping { nonce: 7 })?;
    conn.expect().await?;
    Ok(start.elapsed().as_millis())
}

#[cfg(target_os = "linux")]
fn os_name() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|v| v.trim_matches('"').to_owned())
        })
        .unwrap_or_else(|| "Linux".into())
}

#[cfg(windows)]
fn os_name() -> String {
    std::process::Command::new("cmd")
        .args(["/c", "ver"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Windows".into())
}

#[cfg(not(any(windows, target_os = "linux")))]
fn os_name() -> String {
    std::env::consts::OS.into()
}

#[cfg(target_os = "linux")]
fn cpu_name() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("model name"))
                .map(|v| v.trim_start_matches([' ', '\t', ':']).to_owned())
        })
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(windows)]
fn cpu_name() -> String {
    std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0",
            "/v",
            "ProcessorNameString",
        ])
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .find_map(|l| l.split("REG_SZ").nth(1).map(|v| v.trim().to_owned()))
        })
        .or_else(|| std::env::var("PROCESSOR_IDENTIFIER").ok())
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(not(any(windows, target_os = "linux")))]
fn cpu_name() -> String {
    "unknown".into()
}
