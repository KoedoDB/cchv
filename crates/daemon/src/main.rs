use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use log::info;

mod api;
mod cgroup;
mod config;
mod confine;
mod ctl;
mod exec;
mod fsutil;
mod imagekey;
mod netns;
mod pool;
mod rootless;
pub mod shadow;
mod validate;
mod vm_lifecycle;

fn main() -> Result<()> {
    // The launcher of a confined Cloud Hypervisor is this same binary: it must not start a runtime (it forks and unshares).
    if std::env::args().nth(1).as_deref() == Some("--ch-launcher") {
        confine::launcher_main();
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(daemon_main())
}

async fn daemon_main() -> Result<()> {
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
    rootless::init(&config)?;
    validate::init(&config);
    cgroup::init(&config)?;
    info!(
        "pool_size: {}, memory: {}MiB, vcpus: {}",
        config.pool_size, config.default_memory_mb, config.default_vcpus
    );

    // The signal handlers are installed now, before the start, which can take half a minute (the base snapshot): a request to stop during
    // it is not lost, and stops what was started.
    let mut stop = StopSignals::new();

    let pool = pool::Pool::new(config.clone());
    let socket_path = config.socket_path.clone();
    let run = async {
        pool.prepare_state_dir()?;
        // VMs of an earlier daemon that died without cleaning up are held in memory and known to nobody: stop them first.
        let swept = pool.sweep_orphans().await;
        if swept > 0 {
            info!("stopped {swept} Cloud Hypervisor processes left by an earlier daemon");
        }
        // Rootless: what an earlier daemon left of its VMs' users (processes, ACL entries on the images) goes before any user is given
        // out.
        cgroup::sweep().await;
        rootless::sweep_processes(&config).await;
        rootless::sweep_acls(&config);
        rootless::sweep_outboxes(std::path::Path::new(&config.state_dir));
        // The workload snapshots of an earlier daemon are unknown to this one (its cache is empty) and were made by VMs it can no longer
        // vouch for: they go. The base snapshot is reused only if the daemon marked it complete (see `Pool::base_snapshot_valid`).
        if let Ok(entries) = std::fs::read_dir(config.snapshot_cache_dir()) {
            for e in entries.flatten() {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }

        // Initialize pool (creates base snapshot if needed, fills pool)
        pool.initialize().await?;
        // A pool VM whose process died on its own (OOM kill, crash) is replaced within a few seconds, not when a pod is given it.
        pool.spawn_reaper(std::time::Duration::from_secs(5));

        // Initialize snapshot manager (for warm workload snapshots)
        let snapshots = shadow::SnapshotManager::new(config.clone(), Arc::clone(&pool));

        // Start API server.
        let server = api::ApiServer::new(config.clone(), Arc::clone(&pool), snapshots);
        server.serve().await
    };

    // On SIGTERM or SIGINT, stop every VM before exiting: the VMs are this daemon's children and must not outlive it. From then on no
    // request makes a VM (`Pool::is_stopping`).
    tokio::select! {
        result = run => result,
        _ = stop.wait() => {
            info!("stop requested: stopping all VMs");
            pool.shutdown_all().await;
            let _ = std::fs::remove_file(&socket_path);
            info!("all VMs stopped, exiting");
            Ok(())
        }
    }
}

/// SIGTERM (systemd stop, docker stop) and SIGINT (Ctrl-C), registered when this is made.
struct StopSignals {
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
}

impl StopSignals {
    fn new() -> Self {
        use tokio::signal::unix::{signal, SignalKind};
        Self {
            term: signal(SignalKind::terminate()).expect("SIGTERM handler"),
            int: signal(SignalKind::interrupt()).expect("SIGINT handler"),
        }
    }

    /// Resolves when either has arrived.
    async fn wait(&mut self) {
        tokio::select! {
            _ = self.term.recv() => {}
            _ = self.int.recv() => {}
        }
    }
}
