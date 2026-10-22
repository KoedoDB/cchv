use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use log::info;

mod api;
mod blockvol;
mod cgroup;
mod config;
mod confine;
mod consolefwd;
mod ctl;
mod emptydir;
mod exec;
mod fsutil;
mod imagekey;
mod liveness;
mod lvm;
mod netns;
mod pool;
mod rootless;
pub mod shadow;
mod validate;
mod vm_lifecycle;

fn main() -> Result<()> {
    if cloudhv_common::buildinfo::wants_version(std::env::args().nth(1).as_deref()) {
        use cloudhv_common::buildinfo as b;
        print!(
            "{}",
            b::report(
                "cloudhv-sandbox-daemon",
                &[("release bundles Cloud Hypervisor", b::CH_VERSION)]
            )
        );
        return Ok(());
    }
    // The launcher of a confined Cloud Hypervisor is this same binary: it must not start a runtime (it forks and unshares).
    if std::env::args().nth(1).as_deref() == Some("--ch-launcher") {
        confine::launcher_main();
    }
    // The volume helper (lvm.rs) is this same binary in a unit of its own: it runs LVM for the daemon, which does not.
    if std::env::args().nth(1).as_deref() == Some("--volume-helper") {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
        let path = std::env::args()
            .nth(2)
            .unwrap_or_else(|| "/opt/cloudhv/daemon.json".to_string());
        let config = config::DaemonConfig::load(&PathBuf::from(path))?;
        return lvm::run_helper(&config);
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
    raise_open_files_limit(config.max_connections);
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
        // The VMs the earlier daemon handed to pods and that still run are taken over; every other VM it left (the ones waiting in its pool,
        // those it was making, those of a daemon that died before it recorded them) is held in memory and known to nobody: stopped.
        let adopted = pool.adopt().await;
        let kept_vms: std::collections::HashSet<String> =
            adopted.iter().map(|v| v.vm_id.clone()).collect();
        let kept_uids: std::collections::HashSet<u32> = adopted
            .iter()
            .filter_map(|v| {
                rootless::uid_of(&std::path::Path::new(&config.state_dir).join(&v.vm_id))
            })
            .collect();
        // Rootless: what an earlier daemon left of its VMs' users (processes, ACL entries on the images) goes before any user is given
        // out. The users of the VMs just taken over stay.
        cgroup::sweep(&kept_vms).await;
        rootless::sweep_processes(&config, &kept_uids).await;
        rootless::sweep_acls(&config, &kept_uids);
        rootless::sweep_outboxes(std::path::Path::new(&config.state_dir), &kept_vms);
        // The disks of the emptyDirs of the VMs just stopped go with them; the directory is made if it is not there.
        emptydir::init(&config, &kept_vms);
        blockvol::init(&config);
        // A pod's persistent volume is locked again, or the VM goes: it would share the volume with whoever is given it next.
        for vm in &adopted {
            let dir = std::path::Path::new(&config.state_dir).join(&vm.vm_id);
            if !blockvol::adopt(&dir) {
                log::warn!(
                    "VM {} cannot be taken over (its block devices), stopping it",
                    vm.vm_id
                );
                let _ = pool.release(&vm.vm_id).await;
            }
        }
        consolefwd::init(&config);
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
        // The guests' consoles go to the log (they are read from the sockets the Cloud Hypervisors make).
        consolefwd::spawn();

        // Initialize snapshot manager (for warm workload snapshots)
        let snapshots = shadow::SnapshotManager::new(config.clone(), Arc::clone(&pool));

        // Start API server.
        let server = api::ApiServer::new(config.clone(), Arc::clone(&pool), snapshots);
        // The connections to the guests' agents are made again for the VMs taken over: the pods' calls go through them.
        server.adopt_agents(&adopted).await;
        server.spawn_unclaimed_reaper();
        // A VM whose Cloud Hypervisor is gone is released, and one whose guest is hung is killed: the daemon decides, not the registry or the shim.
        server.spawn_liveness();
        server.serve().await
    };

    // On SIGTERM or SIGINT, stop the VMs that are not a pod's before exiting; the pods' VMs run on, and the next daemon takes them over
    // (`keep_vms_on_stop`, `Pool::adopt`). From then on no request makes a VM (`Pool::is_stopping`).
    tokio::select! {
        result = run => result,
        _ = stop.wait() => {
            info!("stop requested: stopping the VMs that are not a pod's");
            pool.shutdown_all(config.keep_vms_on_stop).await;
            let _ = std::fs::remove_file(&socket_path);
            info!("done, exiting");
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

/// Descriptors the daemon may hold open: one per connection, and the sockets, files and pipes of its VMs. The usual limit of 1024 would
/// stop it far below `max_connections`.
fn wanted_open_files(max_connections: usize) -> u64 {
    (max_connections as u64)
        .saturating_mul(2)
        .saturating_add(4096)
}

fn raise_open_files_limit(max_connections: usize) {
    let want = wanted_open_files(max_connections);
    let mut cur = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut cur) } != 0 || cur.rlim_cur >= want {
        return;
    }
    let new = libc::rlimit {
        rlim_cur: want,
        rlim_max: want.max(cur.rlim_max),
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &new) } != 0 {
        // Not allowed to raise the hard limit: take what the hard limit allows.
        let capped = libc::rlimit {
            rlim_cur: cur.rlim_max,
            rlim_max: cur.rlim_max,
        };
        let _ = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &capped) };
        log::warn!("open files limit is {} (wanted {want}); at most about that many connections can be served", cur.rlim_max);
    }
}

#[cfg(test)]
mod nofile_tests {
    #[test]
    fn the_descriptor_limit_covers_the_connections() {
        assert_eq!(super::wanted_open_files(8192), 8192 * 2 + 4096);
        assert!(
            super::wanted_open_files(usize::MAX) >= u64::MAX / 2,
            "no overflow"
        );
    }
}
