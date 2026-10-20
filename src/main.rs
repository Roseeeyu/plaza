use std::error::Error;
use std::path::PathBuf;

use clap::Parser;
use tokio::net::TcpListener;

use plaza::{Config, serve};

/// A multiplayer pixel canvas server.
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Address to serve the web frontend on
    #[arg(default_value = "127.0.0.1:8080")]
    addr: String,

    /// Also serve the raw line protocol (for nc) on this address
    #[arg(long)]
    tcp: Option<String>,

    /// Canvas width in pixels (ignored when restoring a saved canvas)
    #[arg(long, default_value_t = 64)]
    width: u32,

    /// Canvas height in pixels (ignored when restoring a saved canvas)
    #[arg(long, default_value_t = 64)]
    height: u32,

    /// Cooldown charged per pixel written, in milliseconds
    #[arg(long, default_value_t = 0)]
    cooldown: u64,

    /// Maximum diffs queued to a subscriber before it is cut off
    #[arg(long, default_value_t = 1024)]
    lag_budget: usize,

    /// Directory the canvas is saved in
    #[arg(long, default_value = "data")]
    data: PathBuf,

    /// Keep the canvas in memory only
    #[arg(long)]
    no_persist: bool,

    /// Commits between snapshots of the saved canvas
    #[arg(long, default_value_t = 1000)]
    snapshot_every: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    let web = TcpListener::bind(&args.addr).await?;
    eprintln!("Plaza frontend on http://{}", web.local_addr()?);
    let tcp = match &args.tcp {
        Some(addr) => {
            let listener = TcpListener::bind(addr).await?;
            eprintln!("Line protocol on {}", listener.local_addr()?);
            Some(listener)
        }
        None => None,
    };

    let config = Config {
        width: args.width,
        height: args.height,
        cooldown_ms: args.cooldown,
        lag_budget: args.lag_budget,
        data_dir: (!args.no_persist).then_some(args.data),
        snapshot_every: args.snapshot_every,
    };

    serve(web, tcp, config, shutdown_signal()).await?;
    Ok(())
}

/// Resolves on Ctrl+C, or on SIGTERM, which is what `docker stop` and most
/// process managers send. Running as PID 1 in a container, an unhandled
/// SIGTERM is ignored and the process is killed outright after a timeout.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
