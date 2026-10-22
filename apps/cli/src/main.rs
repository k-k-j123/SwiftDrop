//! SwiftDrop application — the web UI plus scriptable CLI verbs.
//!
//! ```text
//! swiftdrop serve   [--listen 0.0.0.0:8080] [--dest DIR] [--open]
//! swiftdrop receive [--listen 0.0.0.0:52915] [--dest DIR] [--yes]
//! swiftdrop send    [--yes] <host[:port]> <file>...
//! ```
//!
//! `serve` **is the app**: it runs the web UI from `swiftdrop_core::web`,
//! whose single page drives computer-to-computer send and receive and the
//! no-install phone upload path. `--open` also launches the local browser,
//! which is what the desktop launcher uses.
//!
//! `send` and `receive` are the scriptable equivalents of that page's
//! buttons: the same `swiftdrop_core` session code, driven from a terminal
//! with stdin prompts instead of the browser.
//!
//! `--yes` disables every approval prompt. It exists for scripts, tests,
//! and benchmarking — never for normal use (spec §20 forbids shipping
//! auto-accept as the default; here it is an explicit opt-in flag).

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use swiftdrop_core::{
    receive, send, AutoAccept, OfferLimits, Peer, ReceiveOptions, SendOptions, SessionPrompt,
    TransferProgress,
};
use swiftdrop_protocol::messages::TransferOffer;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Keep logs (which never contain secrets or file contents — spec
    // NFR-SEC-007) on stderr so stdout stays machine-readable.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        match args.first().map(String::as_str) {
            Some("receive") => cmd_receive(&args[1..]).await,
            Some("send") => cmd_send(&args[1..]).await,
            Some("serve") => cmd_serve(&args[1..]).await,
            _ => {
                eprintln!("usage: swiftdrop serve [--listen ADDR] [--dest DIR] [--open]");
                eprintln!("       swiftdrop receive [--listen ADDR] [--dest DIR] [--yes]");
                eprintln!("       swiftdrop send [--yes] <host[:port]> <file>...");
                std::process::exit(2);
            }
        }
    })
}

/// `swiftdrop receive` — listen until killed, saving approved transfers.
async fn cmd_receive(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let settings = swiftdrop_core::Settings::load();
    let mut listen = format!("0.0.0.0:{}", settings.listen_port);
    let mut dest = settings.dest_dir.clone();
    let mut auto = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--listen" => {
                listen = args.get(i + 1).expect("--listen needs an address").clone();
                i += 2;
            }
            "--dest" => {
                dest = PathBuf::from(args.get(i + 1).expect("--dest needs a path"));
                i += 2;
            }
            "--yes" => {
                auto = true;
                i += 1;
            }
            other => return Err(format!("unknown receive flag: {other}").into()),
        }
    }
    std::fs::create_dir_all(&dest)?;

    // Binds before printing so "listening on …" is never a lie.
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    eprintln!("listening on {listen}, saving into {}", dest.display());

    let progress = Arc::new(TransferProgress::new());
    // Spawn live CLI progress reporter in the background.
    tokio::spawn(show_progress(progress.clone()));

    let opts = ReceiveOptions {
        config_dir: swiftdrop_core::config::config_dir(),
        device_name: settings.device_name.clone(),
        dest_dir: dest,
        collision_policy: settings.collision_policy,
        limits: OfferLimits::default(),
        auto_accept: auto,
        progress,
    };
    // `AutoAccept` when `--yes`, otherwise the terminal prompt: `receive`
    // is generic over the prompt, so the choice is static, not a branch
    // inside the session loop.
    let received = if auto {
        receive(listener, opts, AutoAccept).await
    } else {
        receive(listener, opts, TerminalPrompt).await
    };
    received.map_err(|e| Box::from(e) as Box<dyn std::error::Error>)
}

/// `swiftdrop send` — push one or more files to a peer in a single offer.
async fn cmd_send(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let settings = swiftdrop_core::Settings::load();
    let mut auto = false;
    let mut positional: Vec<String> = Vec::new();
    for a in args {
        if a == "--yes" {
            auto = true;
        } else {
            positional.push(a.clone());
        }
    }
    let (target, files) = positional
        .split_first()
        .ok_or("send needs a target: swiftdrop send <host[:port]> <file>…")?;
    if files.is_empty() {
        return Err("send needs at least one file".into());
    }

    // `host[:port]` → resolved socket address + the SNI name to present.
    let (host, port) = match target.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>()?),
        None => (target.clone(), swiftdrop_core::config::DEFAULT_LISTEN_PORT),
    };
    let addr = tokio::net::lookup_host((host.as_str(), port))
        .await?
        .next()
        .ok_or_else(|| format!("could not resolve {host}:{port}"))?;

    let progress = Arc::new(TransferProgress::new());
    // Spawn live CLI progress reporter in the background.
    tokio::spawn(show_progress(progress.clone()));

    let opts = SendOptions {
        config_dir: swiftdrop_core::config::config_dir(),
        device_name: settings.device_name.clone(),
        limits: OfferLimits::default(),
        auto_accept: auto,
        progress,
    };
    let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
    let sent: Result<_, swiftdrop_core::SessionError> = if auto {
        send(addr, &host, files, opts, AutoAccept).await
    } else {
        send(addr, &host, files, opts, TerminalPrompt).await
    };
    let outcome = sent.map_err(|e| Box::from(e) as Box<dyn std::error::Error>)?;

    println!(
        "saved {} file(s), {} bytes total",
        outcome.saved_names.len(),
        outcome.total_bytes
    );
    for name in &outcome.saved_names {
        println!("  {name}");
    }
    Ok(())
}

/// `swiftdrop serve` — run the SwiftDrop app (web UI) until killed.
///
/// This is the product interface: open the printed URL (or let `--open`
/// launch the browser) to send to another computer, receive from one, or
/// let a phone browser upload without installing anything. The phone path
/// is deliberately unauthenticated — `web.rs` documents exactly what that
/// trades away and which endpoints do enforce the protocol's approvals.
async fn cmd_serve(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let settings = swiftdrop_core::Settings::load();
    let mut listen = "0.0.0.0:8080".to_string();
    let mut dest = settings.dest_dir.clone();
    let mut open = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--listen" => {
                listen = args.get(i + 1).ok_or("--listen needs an address")?.clone();
                i += 2;
            }
            "--dest" => {
                dest = PathBuf::from(args.get(i + 1).ok_or("--dest needs a path")?);
                i += 2;
            }
            "--open" => {
                open = true;
                i += 1;
            }
            other => return Err(format!("unknown serve flag: {other}").into()),
        }
    }

    let opts = swiftdrop_core::web::WebOptions {
        listen,
        dest_dir: dest,
        collision_policy: settings.collision_policy,
        config_dir: swiftdrop_core::config::config_dir(),
        device_name: settings.device_name,
        receive_port: settings.listen_port,
        open,
    };
    swiftdrop_core::web::serve(opts)
        .await
        .map_err(|e| Box::from(e) as Box<dyn std::error::Error>)
}

/// Format byte counts into human-readable strings (B, KB, MB, GB).
fn format_bytes(b: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if b >= GB {
        format!("{:.2} GB", b as f64 / GB as f64)
    } else if b >= MB {
        format!("{:.2} MB", b as f64 / MB as f64)
    } else if b >= KB {
        format!("{:.1} KB", b as f64 / KB as f64)
    } else {
        format!("{b} B")
    }
}

/// Format transfer rate into human-readable speed.
fn format_rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec <= 0.0 {
        return "0 KB/s".to_string();
    }
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * KB;
    const GB: f64 = 1024.0 * MB;
    if bytes_per_sec >= GB {
        format!("{:.2} GB/s", bytes_per_sec / GB)
    } else if bytes_per_sec >= MB {
        format!("{:.2} MB/s", bytes_per_sec / MB)
    } else {
        format!("{:.1} KB/s", bytes_per_sec / KB)
    }
}

/// Background task that polls `TransferProgress` and renders a live CLI
/// progress bar to stderr with percentage, transferred/total bytes, and speed.
async fn show_progress(progress: Arc<TransferProgress>) {
    let mut last_bytes: u64 = 0;
    let mut last_t = Instant::now();
    let mut last_rate: f64 = 0.0;
    let mut was_active = false;
    let mut ticker = tokio::time::interval(Duration::from_millis(200));

    loop {
        ticker.tick().await;
        let snap = progress.snapshot();
        let active = snap.state.is_active();

        if active {
            if !was_active {
                last_bytes = snap.bytes;
                last_t = Instant::now();
                last_rate = 0.0;
            }
            was_active = true;

            if snap.total > 0 {
                let now = Instant::now();
                let dt = (now - last_t).as_secs_f64();
                if dt > 0.0 && snap.bytes >= last_bytes {
                    let inst = (snap.bytes - last_bytes) as f64 / dt;
                    last_rate = if last_rate == 0.0 {
                        inst
                    } else {
                        last_rate * 0.7 + inst * 0.3
                    };
                }
                last_bytes = snap.bytes;
                last_t = now;

                let pct = (snap.bytes as f64 / snap.total as f64 * 100.0).min(100.0);
                let rate_str = if last_rate > 0.0 {
                    format_rate(last_rate)
                } else {
                    "...".into()
                };

                let width = 20;
                let filled = ((pct / 100.0) * width as f64) as usize;
                let filled = filled.min(width);
                let empty = width - filled;
                let bar = format!("[{}{}]", "=".repeat(filled), " ".repeat(empty));

                eprint!(
                    "\r{} {:.1}%  {} / {}  {}",
                    bar,
                    pct,
                    format_bytes(snap.bytes),
                    format_bytes(snap.total),
                    rate_str
                );
                let _ = std::io::stderr().flush();
            }
        } else if was_active {
            was_active = false;
            if snap.total > 0 {
                let pct = (snap.bytes as f64 / snap.total as f64 * 100.0).min(100.0);
                let width = 20;
                let filled = ((pct / 100.0) * width as f64) as usize;
                let filled = filled.min(width);
                let empty = width - filled;
                let bar = format!("[{}{}]", "=".repeat(filled), " ".repeat(empty));
                eprint!(
                    "\r{} {:.1}%  {} / {}          \n",
                    bar,
                    pct,
                    format_bytes(snap.bytes),
                    format_bytes(snap.total)
                );
                let _ = std::io::stderr().flush();
            }
        }
    }
}

/// Reads approvals from stdin. Prints the SAS both sides must match
/// (spec FR-PAIR-009) and the offer summary before asking (FR-XFER-003).
struct TerminalPrompt;

#[async_trait::async_trait]
impl SessionPrompt for TerminalPrompt {
    async fn confirm_pairing(&self, peer: &Peer, sas: &str) -> bool {
        println!(
            "\npair with \"{0}\" ({1})?",
            peer.display_name,
            short_id(&peer.device_id)
        );
        println!("  pairing code: {sas}   ← must match on their screen");
        ask("  accept? [y/N] ").await
    }

    async fn confirm_offer(&self, peer: &Peer, offer: &TransferOffer) -> bool {
        println!(
            "\n{} offers {} file(s), {} bytes:",
            peer.display_name,
            offer.items.len(),
            offer.total_size_bytes
        );
        for item in &offer.items {
            println!("  {} ({} bytes)", item.name, item.size_bytes);
        }
        ask("  accept? [y/N] ").await
    }
}

/// One blocking line read per prompt. `block_in_place` is safe on the
/// multi-thread runtime this CLI builds (a UI would answer over the
/// bridge instead — spec §7.3).
async fn ask(question: &str) -> bool {
    print!("{question}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    tokio::task::block_in_place(|| std::io::stdin().read_line(&mut line)).unwrap_or(0);
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// First 8 hex chars of a device id — enough to tell two peers apart on
/// a desk, without printing the full identity hash everywhere.
fn short_id(id: &str) -> &str {
    &id[..8.min(id.len())]
}
