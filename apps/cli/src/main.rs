//! SwiftDrop command-line interface — the first UI over `swiftdrop-core`
//! (spec §21, §7.3). Three verbs:
//!
//! ```text
//! swiftdrop receive [--listen 0.0.0.0:52915] [--dest DIR] [--yes]
//! swiftdrop send    [--yes] <host[:port]> <file>...
//! swiftdrop serve   [--listen 0.0.0.0:8080]  [--dest DIR]
//! ```
//!
//! This is a *development* UI, not the product UI: it reads prompts from
//! stdin and prints results to stdout. The real UI is the Flutter app
//! (spec §7.3, milestone M3); this binary exists so the transfer path is
//! exercisable end-to-end before any UI exists.
//!
//! `--yes` disables every approval prompt. It exists for scripts, tests,
//! and benchmarking — never for normal use (spec §20 forbids shipping
//! auto-accept as the default; here it is an explicit opt-in flag).
//!
//! `serve` is the no-install phone path: it runs the plain-HTTP upload
//! server (`swiftdrop_core::web`) so a browser on the same WiFi can push
//! files to this machine without the phone speaking the SwiftDrop wire
//! protocol. Unauthenticated by design choice of the owner — see
//! `web.rs` for exactly what that trades away.

use std::io::Write as _;
use std::path::PathBuf;
use swiftdrop_core::{
    receive, send, AutoAccept, OfferLimits, Peer, ReceiveOptions, SendOptions, SessionPrompt,
};
use swiftdrop_protocol::messages::TransferOffer;

/// Default listen port, matching what mDNS will advertise (spec §8.2).
const DEFAULT_PORT: u16 = 52915;

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
                eprintln!("usage: swiftdrop receive [--listen ADDR] [--dest DIR] [--yes]");
                eprintln!("       swiftdrop send [--yes] <host[:port]> <file>...");
                eprintln!("       swiftdrop serve [--listen ADDR] [--dest DIR]");
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

    let opts = ReceiveOptions {
        config_dir: swiftdrop_core::config::config_dir(),
        device_name: settings.device_name.clone(),
        dest_dir: dest,
        collision_policy: settings.collision_policy,
        limits: OfferLimits::default(),
        auto_accept: auto,
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
        None => (target.clone(), DEFAULT_PORT),
    };
    let addr = tokio::net::lookup_host((host.as_str(), port))
        .await?
        .next()
        .ok_or_else(|| format!("could not resolve {host}:{port}"))?;

    let opts = SendOptions {
        config_dir: swiftdrop_core::config::config_dir(),
        device_name: settings.device_name.clone(),
        limits: OfferLimits::default(),
        auto_accept: auto,
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

/// `swiftdrop serve` — run the plain-HTTP upload server until killed.
///
/// This is the phone path with no app on the phone: a browser on the same
/// WiFi opens the printed URL and pushes files straight to disk. It does
/// NOT speak the SwiftDrop wire protocol (browsers can't) and does NOT
/// authenticate — the owner's explicit priority was raw speed on very
/// large files, and `web.rs` documents exactly what that skips.
async fn cmd_serve(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut listen = "0.0.0.0:8080".to_string();
    let mut dest = swiftdrop_core::web::default_dest();
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
            other => return Err(format!("unknown serve flag: {other}").into()),
        }
    }

    let opts = swiftdrop_core::web::WebOptions {
        listen,
        dest_dir: dest,
        collision_policy: swiftdrop_core::CollisionPolicy::Rename,
    };
    swiftdrop_core::web::serve(opts)
        .await
        .map_err(|e| Box::from(e) as Box<dyn std::error::Error>)
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
