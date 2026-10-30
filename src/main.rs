mod agent;
mod capture;
mod convert;
mod display;
mod encoder;
mod input;
mod protocol;
mod server;
mod tls;
mod webrtc;

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use agent::{Agent, AgentConfig};
use server::AppState;

/// Aperture - low-latency private desktop link.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// HTTPS port (or HTTP port with --http).
    #[arg(long, default_value_t = 8443)]
    port: u16,
    /// Address to bind.
    #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::UNSPECIFIED))]
    bind: IpAddr,
    /// Capture / encode frame rate cap.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(5..=240))]
    fps: u32,
    /// Target video bitrate in bits per second.
    #[arg(long, default_value_t = 16_000_000)]
    bitrate: u32,
    /// Maximum encoded width (never upscales).
    #[arg(long, default_value_t = 2560)]
    max_width: u32,
    /// Maximum encoded height (never upscales).
    #[arg(long, default_value_t = 1440)]
    max_height: u32,
    /// Monitor index to stream (see the list printed at startup).
    #[arg(long, default_value_t = 0)]
    monitor: usize,
    /// Serve plain HTTP (WebCodecs needs HTTPS or localhost, so phones won't work).
    #[arg(long)]
    http: bool,
    /// Use a fixed access token instead of the stored one.
    #[arg(long)]
    token: Option<String>,
    /// Disable the hardware encoder (use Microsoft's software H.264 encoder).
    #[arg(long)]
    no_hardware: bool,
    /// Don't print the QR code.
    #[arg(long)]
    no_qr: bool,
    /// Public URL of an aperture-relay instance (e.g. https://relay.example.com).
    /// Enables the friend link: WebRTC straight into this agent, no VPN needed.
    #[arg(long)]
    relay: Option<String>,
    /// TURN server for the friend link, comma-separated:
    /// "turn:host:3478?transport=udp,username,credential". Repeatable.
    #[arg(long = "turn")]
    turn: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // webrtc-rs enables the aws-lc-rs provider while axum-server uses ring;
    // pick one explicitly so rustls never has to guess.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("aperture=info")),
        )
        .with_target(false)
        .init();

    let args = Args::parse();
    let monitors = display::enumerate();
    if monitors.is_empty() {
        anyhow::bail!("no displays found");
    }

    let agent = Agent::start(
        AgentConfig {
            monitor: args.monitor.min(monitors.len() - 1),
            fps: args.fps,
            bitrate: args.bitrate,
            max_width: args.max_width,
            max_height: args.max_height,
            hardware: !args.no_hardware,
        },
        monitors.clone(),
    );
    let ready = agent.wait_ready(Duration::from_secs(10));

    let dir = tls::data_dir();
    let token = tls::load_or_create_token(&dir, args.token.clone())?;

    let lan_ip = local_ip_address::local_ip()
        .ok()
        .filter(|ip| !ip.is_loopback() && !matches!(ip, IpAddr::V6(_)));
    let mut hosts = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    if let Some(ip) = lan_ip {
        hosts.push(ip.to_string());
    }
    // Interface addresses (LAN, Tailscale, hotspots) go into the certificate so
    // the iPad sees a matching name no matter which route it takes.
    if let Ok(interfaces) = local_ip_address::list_afinet_netifas() {
        for (_, ip) in interfaces {
            let IpAddr::V4(v4) = ip else { continue };
            if v4.is_loopback() || v4.is_link_local() {
                continue;
            }
            let text = v4.to_string();
            if !hosts.contains(&text) {
                hosts.push(text);
            }
        }
    }
    if !args.bind.is_unspecified() && !args.bind.is_loopback() {
        let text = args.bind.to_string();
        if !hosts.contains(&text) {
            hosts.push(text);
        }
    }

    let tls_config = if args.http {
        None
    } else {
        Some(tls::load_or_create_tls(&dir, &hosts).await?)
    };

    let scheme = if args.http { "http" } else { "https" };
    let port_part = if args.port == 80 || (args.port == 443 && !args.http) {
        String::new()
    } else {
        format!(":{}", args.port)
    };
    let desktop_url = format!("{scheme}://localhost{port_part}/?t={token}");
    let phone_url = lan_ip.map(|ip| format!("{scheme}://{ip}{port_part}/?t={token}"));

    let share_url = args.relay.as_deref().map(|relay| {
        let room = webrtc::room_for_token(&token);
        format!("{}/r/{room}?t={token}", relay.trim_end_matches('/'))
    });
    if let Some(relay) = args.relay.as_deref() {
        for spec in &args.turn {
            webrtc::parse_turn(spec)?;
        }
        let room = webrtc::room_for_token(&token);
        webrtc::run(
            agent.clone(),
            webrtc::signaling_url(relay, &room),
            room,
            Arc::from(token.as_str()),
            &args.turn,
        );
    }

    print_banner(&agent, &args, &desktop_url, phone_url.as_deref(), share_url.as_deref(), ready);

    if !args.no_qr {
        if let Some(url) = phone_url.as_deref() {
            if let Ok(code) = qrcode::QrCode::new(url.as_bytes()) {
                let rendered = code
                    .render::<qrcode::render::unicode::Dense1x2>()
                    .quiet_zone(true)
                    .module_dimensions(2, 1)
                    .build();
                println!("{rendered}");
                println!("  Scan with your phone camera.\n");
            }
        }
    }
    if !args.no_qr {
        if let Some(url) = share_url.as_deref() {
            println!("  Share link for friends (WebRTC, no app needed):");
            println!("  {url}");
            if let Ok(code) = qrcode::QrCode::new(url.as_bytes()) {
                let rendered = code
                    .render::<qrcode::render::unicode::Dense1x2>()
                    .quiet_zone(true)
                    .module_dimensions(2, 1)
                    .build();
                println!("{rendered}");
                println!("  Friends scan or tap the link — it opens and connects by itself.\n");
            }
        }
    }

    let state = AppState { agent: agent.clone(), token: Arc::from(token.as_str()) };
    tokio::select! {
        result = server::serve(state, args.bind, args.port, tls_config) => result?,
        _ = tokio::signal::ctrl_c() => {
            println!("\nstopping…");
            agent.stop();
        }
    }
    Ok(())
}

fn print_banner(
    agent: &Agent,
    args: &Args,
    desktop_url: &str,
    phone_url: Option<&str>,
    share_url: Option<&str>,
    ready: bool,
) {
    let state = agent.shared.state.read();
    let rule = "─".repeat(64);
    println!();
    println!("  A P E R T U R E");
    println!("  private low-latency desktop link");
    println!("  {rule}");
    if let Some(error) = &state.error {
        println!("  pipeline  !! {error}");
    }
    if ready && state.width > 0 {
        println!(
            "  encoder   {}",
            if state.encoder_name.is_empty() { "starting…" } else { &state.encoder_name }
        );
        println!(
            "  stream    {}x{} @ {} fps · {:.1} Mbps",
            state.width,
            state.height,
            args.fps,
            args.bitrate as f64 / 1_000_000.0
        );
        println!(
            "  monitor   {} · {}x{}",
            state.monitor, state.monitor_rect.width, state.monitor_rect.height
        );
    } else {
        println!("  pipeline  starting…");
    }
    println!("  {rule}");
    for monitor in &agent.shared.monitors {
        println!(
            "  display {:<2} {}x{} {}{}",
            monitor.index,
            monitor.width,
            monitor.height,
            monitor.device,
            if monitor.primary { "  (primary)" } else { "" }
        );
    }
    println!("  {rule}");
    println!("  desktop   {desktop_url}");
    if let Some(url) = phone_url {
        println!("  phone     {url}");
    }
    if let Some(url) = share_url {
        println!("  share     {url}");
    }
    println!("  token     (embedded in the links above)");
    println!("  {rule}");
    if !args.http {
        println!("  Self-signed certificate: on first visit choose 'Advanced → Proceed'.");
    }
    println!();
}
