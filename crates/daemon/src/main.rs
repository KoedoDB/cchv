use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use log::info;

mod api;
mod config;
mod netns;
mod pool;
pub mod shadow;
mod vm_lifecycle;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config_path = std::env::args()
        .nth(1)
        .or_else(|| {
            std::env::args()
                .find(|a| a.starts_with("--config="))
                .map(|a| a[9..].to_string())
        })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/cloudhv/daemon.json"));

    info!(
        "cloudhv-sandbox-daemon starting (version {})",
        env!("CARGO_PKG_VERSION")
    );
    info!("config: {}", config_path.display());

    let config = config::DaemonConfig::load(&config_path)?;
    info!(
        "pool_size: {}, memory: {}MiB, vcpus: {}",
        config.pool_size, config.default_memory_mb, config.default_vcpus
    );

    // Initialize pool (creates base snapshot if needed, fills pool)
    let pool = pool::Pool::new(config.clone());
    // VMs of an earlier daemon that died without cleaning up are held in memory and known to nobody: stop them first.
    let swept = pool.sweep_orphans().await;
    if swept > 0 {
        info!("stopped {swept} Cloud Hypervisor processes left by an earlier daemon");
    }
    pool.initialize().await?;

    // Initialize snapshot manager (for warm workload snapshots)
    let snapshots = shadow::SnapshotManager::new(config.clone(), Arc::clone(&pool));

    // Start API server. On SIGTERM or SIGINT, stop every VM before exiting: the VMs are this daemon's children and must not outlive it.
    let socket_path = config.socket_path.clone();
    let pool_for_shutdown = Arc::clone(&pool);
    let server = api::ApiServer::new(config, pool, snapshots);
    tokio::select! {
        result = server.serve() => result,
        _ = shutdown_signal() => {
            info!("stop requested: stopping all VMs");
            pool_for_shutdown.shutdown_all().await;
            let _ = std::fs::remove_file(&socket_path);
            info!("all VMs stopped, exiting");
            Ok(())
        }
    }
}

/// Resolves when the process gets SIGTERM (systemd stop, docker stop) or SIGINT (Ctrl-C).
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}
