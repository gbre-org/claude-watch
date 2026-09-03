//! mcp-host-bash-server — single-process host-side MCP server.
//!
//! Replaces the `mcp-host-bash` bash launcher + `mcp-proxy` + `cli-mcp-server`
//! + `mcp-proxy-auth-shim` chain with ONE process that speaks streamable-HTTP,
//! does bearer auth inline, and runs `run_command` / `run_script` /
//! `show_security_rules` against the host shell. No upstream hop → the
//! "connected-but-no-tools" wedge is structurally impossible.
//!
//! Cross-platform: builds on macOS and Linux. It is DEPLOYED only in the
//! macOS-containerized topology (Claude in a container reaching the host via
//! `host.docker.internal:8766`); the Linux-uncontainerized topology runs
//! Claude directly on the box and uses the raw Bash tool, never this server.

mod config;
mod exec;
mod mcp;

use std::path::PathBuf;

use clap::Parser;

use config::ServerConfig;

#[derive(Parser, Debug)]
#[command(
    name = "mcp-host-bash-server",
    about = "Single-process host-side MCP server (run_command/run_script over streamable-HTTP)."
)]
struct Args {
    /// Public listen port (default 8766; also settable via MCP_HOST_BASH_PORT).
    #[arg(long)]
    port: Option<u16>,

    /// Operator config file (KEY=VALUE). Defaults to
    /// ~/.config/claude-container/mcp-host-bash.env.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Print the effective policy + bind info and exit (no listener).
    #[arg(long)]
    print_config: bool,
}

fn banner(cfg: &ServerConfig) {
    eprintln!("mcp-host-bash-server: starting");
    eprintln!("  listen:                {}:{}", cfg.bind_host, cfg.port);
    match &cfg.bearer {
        Some(_) => eprintln!("  bearer auth:           ENABLED"),
        None => {
            eprintln!("  bearer auth:           DISABLED (MCP_HOST_BASH_BEARER unset)");
            if cfg.bind_host != "127.0.0.1" && cfg.bind_host != "localhost" {
                eprintln!(
                    "  WARNING:               binding non-loopback ({}) WITHOUT bearer auth exposes",
                    cfg.bind_host
                );
                eprintln!("                         the host shell-exec surface with no authentication.");
            }
        }
    }
    if let Some(w) = &cfg.policy.profile_warning {
        eprintln!("  WARNING:               {w}");
    }
    eprintln!("  config:                {} (present: {})", cfg.config_path.display(), cfg.config_present);
    eprintln!();
    eprintln!("{}", cfg.policy.describe());
    eprintln!();
    eprintln!("In your container .env, set:");
    eprintln!(
        "  CLAUDE_MCP_HTTP_BRIDGE=host-bash=http://host.docker.internal:{}/mcp",
        cfg.port
    );
    if cfg.bearer.is_some() {
        eprintln!("  CLAUDE_HOST_HOOK_BRIDGE_BEARER=<same value as MCP_HOST_BASH_BEARER>");
    }
    eprintln!();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received");
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    let config_path = args.config.unwrap_or_else(ServerConfig::default_config_path);
    let cfg = ServerConfig::load(config_path, args.port);

    if args.print_config {
        println!("listen: {}:{}", cfg.bind_host, cfg.port);
        println!("bearer: {}", if cfg.bearer.is_some() { "enabled" } else { "disabled" });
        println!("config: {} (present: {})", cfg.config_path.display(), cfg.config_present);
        println!();
        println!("{}", cfg.policy.describe());
        return;
    }

    banner(&cfg);

    let addr = format!("{}:{}", cfg.bind_host, cfg.port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("mcp-host-bash-server: FATAL: cannot bind {addr}: {e}");
            eprintln!("  Common cause: a stale prior instance still owns the port.");
            eprintln!("  Check: lsof -nP -iTCP:{} -sTCP:LISTEN", cfg.port);
            std::process::exit(1);
        }
    };
    tracing::info!("listening on {addr}");

    let app = mcp::router(cfg);
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        eprintln!("mcp-host-bash-server: serve error: {e}");
        std::process::exit(1);
    }
}
