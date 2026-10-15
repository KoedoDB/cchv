use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use log::{info, warn};
use tokio::sync::Mutex;

use crate::config::DaemonConfig;

/// The file that says the base snapshot is complete and handed over (see `Pool::base_snapshot_valid`).
const BASE_SNAPSHOT_DONE: &str = ".complete";

/// A pre-booted VM in the pool, ready for acquisition.
#[derive(Debug, Clone)]
pub struct PoolVm {
    /// Unique VM identifier.
    pub vm_id: String,
    /// Cloud Hypervisor API socket path.
    pub api_socket: PathBuf,
    /// vsock proxy socket path.
    pub vsock_socket: PathBuf,
    /// vsock CID.
    pub cid: u64,
    /// Cloud Hypervisor process PID.
    pub ch_pid: u32,
    /// When this VM was added to the pool.
    pub created_at: std::time::Instant,
}

/// Manages a pool of pre-booted VMs restored from a base snapshot.
pub struct Pool {
    config: DaemonConfig,
    /// Ready-to-use VMs waiting for acquisition.
    ready: Mutex<Vec<PoolVm>>,
    /// Active VMs acquired by shims, keyed by vm_id.
    active: Mutex<HashMap<String, PoolVm>>,
    /// Count of VMs currently acquired by shims.
    active_count: std::sync::atomic::AtomicUsize,
    /// Held while a VM is being restored for the pool, so that two refills do not both see "one short" and make two.
    refill: Mutex<()>,
    /// Next CID to assign (incremented per VM).
    next_cid: std::sync::atomic::AtomicU64,
    /// Set when the daemon is told to stop: no VM is made from then on, so that none appears after the sweep that stops them all.
    stopping: std::sync::atomic::AtomicBool,
}

impl Pool {
    /// Create a new pool. Does not fill it — call `initialize()` to create
    /// the base snapshot and fill the pool.
    pub fn new(config: DaemonConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            ready: Mutex::new(Vec::new()),
            active: Mutex::new(HashMap::new()),
            active_count: std::sync::atomic::AtomicUsize::new(0),
            refill: Mutex::new(()),
            next_cid: std::sync::atomic::AtomicU64::new(3), // CID 0-2 reserved
            stopping: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Initialize the pool: create base snapshot if needed, fill to pool_size.
    pub async fn initialize(self: &Arc<Self>) -> Result<()> {
        let base_dir = self.config.base_snapshot_dir();

        // Create base snapshot if it doesn't exist or is stale
        if !self.base_snapshot_valid(&base_dir) {
            info!("creating base snapshot...");
            self.create_base_snapshot(&base_dir).await?;
            info!("base snapshot created at {}", base_dir.display());
        } else {
            info!("reusing existing base snapshot at {}", base_dir.display());
        }

        // Fill pool to target size
        let target = self.config.pool_size;
        info!("filling pool to {} VMs...", target);
        for i in 0..target {
            match self.restore_one().await {
                Ok(vm) => {
                    info!(
                        "pool VM {}/{} ready: {} (cid={}, pid={})",
                        i + 1,
                        target,
                        vm.vm_id,
                        vm.cid,
                        vm.ch_pid
                    );
                    self.ready.lock().await.push(vm);
                }
                Err(e) => {
                    warn!("failed to create pool VM {}/{}: {:#}", i + 1, target, e);
                }
            }
        }

        let ready_count = self.ready.lock().await.len();
        info!("pool initialized: {}/{} VMs ready", ready_count, target);
        Ok(())
    }

    /// Acquire a VM from the pool. Returns immediately if one is available,
    /// otherwise restores synchronously from the base snapshot.
    pub async fn acquire(&self) -> Result<PoolVm> {
        if self.is_stopping() {
            anyhow::bail!("the daemon is stopping");
        }
        // Try to pop from the ready pool
        let vm = self.ready.lock().await.pop();
        if let Some(vm) = vm {
            self.active_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.active
                .lock()
                .await
                .insert(vm.vm_id.clone(), vm.clone());
            let remaining = self.ready.lock().await.len();
            info!(
                "acquired pool VM {} (pool={} active={})",
                vm.vm_id,
                remaining,
                self.active_count()
            );
            return Ok(vm);
        }

        // Pool empty — synchronous fallback
        info!("pool empty, restoring synchronously...");
        let vm = self.restore_one().await?;
        self.active_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.active
            .lock()
            .await
            .insert(vm.vm_id.clone(), vm.clone());
        info!("acquired VM {} (synchronous restore)", vm.vm_id);
        Ok(vm)
    }

    /// Release a VM. The VM is destroyed (not recycled). Only a VM that was handed out (is active) is touched: any other name is not a VM of
    /// this daemon's, and nothing is done for it. Returns whether a VM was destroyed.
    pub async fn release(&self, vm_id: &str) -> Result<bool> {
        let vm = self.active.lock().await.remove(vm_id);
        let Some(vm) = vm else {
            info!("release of {vm_id}: not an active VM, nothing to do");
            return Ok(false);
        };
        self.active_count
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        info!(
            "released VM {} (active={}), destroying...",
            vm_id,
            self.active_count()
        );
        self.destroy_vm(&vm).await;
        Ok(true)
    }

    /// Look up an active VM by its ID.
    pub async fn get_active(&self, vm_id: &str) -> Option<PoolVm> {
        self.active.lock().await.get(vm_id).cloned()
    }

    /// Register an externally-created VM (e.g. warm restore) as active.
    /// This ensures release() can find and clean it up properly.
    pub async fn register_active(&self, vm: PoolVm) {
        self.active_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.active.lock().await.insert(vm.vm_id.clone(), vm);
    }

    /// Replenish the pool by restoring one VM from the base snapshot.
    /// Called asynchronously after each acquire.
    pub async fn replenish_one(&self) {
        let _refilling = self.refill.lock().await;
        let current = self.ready.lock().await.len();
        if current >= self.config.pool_size {
            return;
        }

        match self.restore_one().await {
            Ok(vm) => {
                let name = vm.vm_id.clone();
                self.ready.lock().await.push(vm);
                let new_count = self.ready.lock().await.len();
                info!("pool replenished: {} (pool={})", name, new_count);
            }
            Err(e) => {
                warn!("pool replenish failed: {:#}", e);
            }
        }
    }

    /// Start a task that, every `interval`, finds the ready VMs whose Cloud Hypervisor process has died on its own (killed by the OOM
    /// killer, crashed), takes them out of the pool, removes their directories and makes up the difference. Without it a dead VM stays
    /// in the pool until a pod is given it, and that pod fails to start. A VM that has been handed to a pod is not looked at here: the
    /// shim watches its process, and releasing it removes it.
    pub fn spawn_reaper(self: &Arc<Self>, interval: std::time::Duration) {
        let pool = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let dead = pool.take_dead_ready().await;
                for vm in &dead {
                    warn!(
                        "pool VM {} (pid {}) died on its own, removing it",
                        vm.vm_id, vm.ch_pid
                    );
                    pool.destroy_vm(vm).await;
                }
                // Make up the pool to its size: what died, and what was taken and not made up (a pod's VM that was released, the VM a
                // shadow snapshot used). Without this the pool only ever shrank.
                let missing = pool
                    .config
                    .pool_size
                    .saturating_sub(pool.ready_count().await);
                for _ in 0..missing {
                    pool.replenish_one().await;
                }
            }
        });
    }

    /// Remove from the ready list, and return, the VMs whose process no longer exists.
    async fn take_dead_ready(&self) -> Vec<PoolVm> {
        let mut ready = self.ready.lock().await;
        let (alive, dead): (Vec<PoolVm>, Vec<PoolVm>) = ready
            .drain(..)
            .partition(|vm| crate::ctl::pid_alive(vm.ch_pid));
        *ready = alive;
        dead
    }

    /// Whether the daemon has been told to stop.
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Number of ready VMs in the pool.
    pub async fn ready_count(&self) -> usize {
        self.ready.lock().await.len()
    }

    /// Number of VMs currently acquired by shims.
    pub fn active_count(&self) -> usize {
        self.active_count.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Allocate the next vsock CID.
    fn next_cid(&self) -> u64 {
        self.next_cid
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    /// Public CID allocator for use by snapshot restore.
    pub fn next_cid_pub(&self) -> u64 {
        self.next_cid()
    }

    /// Check if the base snapshot is valid (exists and matches current kernel/rootfs).
    fn base_snapshot_valid(&self, base_dir: &std::path::Path) -> bool {
        let config_path = base_dir.join("config.json");
        let memory_path = base_dir.join("memory-ranges");
        let state_path = base_dir.join("state.json");
        // The marker is written by the daemon after the VM that made the snapshot was destroyed and the snapshot handed over: a snapshot
        // without it was not finished by a daemon that vouches for it (it may be what a crash left).
        config_path.exists() && memory_path.exists() && state_path.exists() && base_dir.join(BASE_SNAPSHOT_DONE).exists()
    }

    /// Create a base snapshot by cold-booting a VM, waiting for the agent,
    /// then pausing and snapshotting.
    ///
    /// The VM writes the snapshot, so what is in the directory is its work. The VM is destroyed (its process and its user's processes
    /// gone) before the directory is handed over (`rootless::hand_over_snapshot`), and nothing in it is read before that.
    async fn create_base_snapshot(&self, base_dir: &std::path::Path) -> Result<()> {
        use crate::vm_lifecycle;

        // Nothing of an earlier, failed attempt.
        let _ = std::fs::remove_dir_all(base_dir);
        std::fs::create_dir_all(base_dir)?;
        let vm_id = format!("base-snapshot-{}", std::process::id());
        let cid = self.next_cid();
        let state_dir = PathBuf::from(&self.config.state_dir).join(&vm_id);
        std::fs::create_dir_all(&state_dir)?;

        // Boot a clean VM (no container disks, no networking). A failed boot removes the VM and its directory itself.
        let ch_pid = match vm_lifecycle::boot_vm(
            &self.config,
            &vm_id,
            &state_dir,
            cid,
            None, // no TAP
            None, // no MAC
            &[],  // no extra disks
        )
        .await
        {
            Ok(pid) => pid,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&state_dir);
                let _ = std::fs::remove_dir_all(base_dir);
                return Err(e.context("boot base VM"));
            }
        };

        let api_socket = state_dir.join("api.sock");
        let vsock_socket = state_dir.join("vsock.sock");

        let steps = async {
            // Wait for agent to be ready
            vm_lifecycle::wait_for_agent(&vsock_socket)
                .await
                .context("agent connect for base snapshot")?;
            info!("base VM ready (pid={}), snapshotting...", ch_pid);
            // Pause → snapshot
            vm_lifecycle::pause_vm(&api_socket).await?;
            vm_lifecycle::snapshot_vm(&api_socket, base_dir).await
        }
        .await;

        // Shut down the base VM, whatever happened
        vm_lifecycle::shutdown_and_destroy(&api_socket, ch_pid, &state_dir).await;

        if let Err(e) = steps {
            let _ = std::fs::remove_dir_all(base_dir);
            return Err(e);
        }
        if let Err(e) = crate::rootless::hand_over_snapshot(base_dir, true) {
            let _ = std::fs::remove_dir_all(base_dir);
            return Err(e);
        }
        if let Err(e) = std::fs::write(base_dir.join(BASE_SNAPSHOT_DONE), b"") {
            let _ = std::fs::remove_dir_all(base_dir);
            return Err(e.into());
        }

        info!("base snapshot created successfully");
        Ok(())
    }

    /// Restore one VM from the base snapshot.
    async fn restore_one(&self) -> Result<PoolVm> {
        use crate::vm_lifecycle;

        if self.is_stopping() {
            anyhow::bail!("the daemon is stopping");
        }

        let t0 = std::time::Instant::now();

        let vm_id = format!(
            "pool-{}-{}",
            std::process::id(),
            self.next_cid.load(std::sync::atomic::Ordering::SeqCst)
        );
        let cid = self.next_cid();
        let state_dir = PathBuf::from(&self.config.state_dir).join(&vm_id);
        std::fs::create_dir_all(&state_dir)?;

        let base_dir = self.config.base_snapshot_dir();

        // 1. Prepare snapshot: patch CID, vsock socket, serial console
        let vsock_socket = state_dir.join("vsock.sock");
        let snapshot_dir = match vm_lifecycle::prepare_snapshot(&self.config, &base_dir, &state_dir, cid, &vsock_socket, None) {
            Ok(d) => d,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&state_dir);
                return Err(e);
            }
        };
        let t_prepare = t0.elapsed().as_millis();

        // 2. Spawn CH process
        let ch_pid = match vm_lifecycle::spawn_ch(&self.config, &state_dir).await {
            Ok(p) => p,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&state_dir);
                return Err(e);
            }
        };
        let t_spawn = t0.elapsed().as_millis();

        // 3-6. The VM is up when CH answers, the snapshot is restored and resumed, and the agent answers. If any step fails, the CH
        // process (and, rootless, its user and ACLs) is stopped and the directory removed; it must not be left behind.
        let api_socket = state_dir.join("api.sock");
        let steps = async {
            vm_lifecycle::wait_ch_ready(&api_socket).await?;
            let t_ready = t0.elapsed().as_millis();
            vm_lifecycle::restore_vm(&api_socket, &snapshot_dir).await?;
            let t_restore = t0.elapsed().as_millis();
            vm_lifecycle::resume_vm(&api_socket).await?;
            let t_resume = t0.elapsed().as_millis();
            vm_lifecycle::wait_for_agent(&vsock_socket)
                .await
                .context("agent connect after restore")?;
            Ok::<_, anyhow::Error>((t_ready, t_restore, t_resume, t0.elapsed().as_millis()))
        }
        .await;
        let (t_ready, t_restore, t_resume, t_agent) = match steps {
            Ok(t) => t,
            Err(e) => {
                vm_lifecycle::shutdown_and_destroy(&api_socket, ch_pid, &state_dir).await;
                return Err(e);
            }
        };

        info!(
            "TIMING restore_one {}: prepare={}ms spawn={}ms ch_ready={}ms restore={}ms resume={}ms agent={}ms total={}ms",
            vm_id, t_prepare, t_spawn - t_prepare, t_ready - t_spawn,
            t_restore - t_ready, t_resume - t_restore, t_agent - t_resume, t_agent
        );

        Ok(PoolVm {
            vm_id,
            api_socket,
            vsock_socket,
            cid,
            ch_pid,
            created_at: std::time::Instant::now(),
        })
    }

    /// Stop every VM this daemon owns: the ready ones, the ones handed to pods, and any Cloud Hypervisor process that is left in the
    /// state directory (a shadow VM, or one of an earlier daemon that was killed). Called when the daemon is told to stop, so that its
    /// VMs do not outlive it: a VM that does is held in memory (one resident guest, 38 to 133 MiB) and known to nobody.
    pub async fn shutdown_all(&self) {
        self.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut vms: Vec<PoolVm> = self.ready.lock().await.drain(..).collect();
        vms.extend(self.active.lock().await.drain().map(|(_, vm)| vm));
        self.active_count
            .store(0, std::sync::atomic::Ordering::SeqCst);
        info!("stopping {} VMs", vms.len());
        for vm in &vms {
            self.destroy_vm(vm).await;
        }
        let left = self.sweep_orphans().await;
        if left > 0 {
            info!("stopped {left} more Cloud Hypervisor processes found in the state directory");
        }
    }

    /// Make the state directory ready: it exists, a VM's user can pass through it (0711: enter, not list, so that one VM cannot see the
    /// names of the others), and the daemon's registry (`ctl.rs`) in it is root's alone.
    pub fn prepare_state_dir(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let root = PathBuf::from(&self.config.state_dir);
        std::fs::create_dir_all(&root)?;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o711))?;
        std::fs::create_dir_all(self.config.snapshot_cache_dir())?;
        // The cache is passed through by the VM that writes a snapshot into it, and listed by no one but root.
        std::fs::set_permissions(self.config.snapshot_cache_dir(), std::fs::Permissions::from_mode(0o711))?;
        let ctl = root.join(crate::ctl::CTL_DIR);
        std::fs::create_dir_all(&ctl)?;
        std::fs::set_permissions(&ctl, std::fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    /// Stop every Cloud Hypervisor process the registry records, and remove every VM directory in the state directory. The directories
    /// kept are the base snapshot and the cache of warm snapshots; everything else (`pool-*`, `warm-*`, `shadow-*`, the base VM's) belongs
    /// to a VM that is, or was, someone's, and when this runs (at start, before the pool is filled; at stop, after the VMs were stopped)
    /// nothing is using it. A process is signalled only if it is verified to be that VM's Cloud Hypervisor (`ctl::signal_ch`), so that a
    /// PID the system has given to something else is left alone. Returns how many processes were stopped.
    ///
    /// This is what makes the cleanup certain. A daemon that is stopped (SIGTERM, SIGINT) cleans up before it exits; one that is killed
    /// (SIGKILL, the OOM killer, a crash) cannot run anything, and its VMs and directories are removed by the next daemon at its start.
    pub async fn sweep_orphans(&self) -> usize {
        let root = PathBuf::from(&self.config.state_dir);
        let mut stopped = 0;
        let mut seen: HashSet<String> = HashSet::new();

        // The registry: every VM this version of the daemon made.
        if let Ok(entries) = std::fs::read_dir(root.join(crate::ctl::CTL_DIR)) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !crate::validate::is_id(&name) {
                    let _ = std::fs::remove_dir_all(entry.path());
                    continue;
                }
                let dir = root.join(&name);
                seen.insert(name);
                if self.sweep_vm(&dir).await {
                    stopped += 1;
                }
            }
        }

        // Directories the registry does not know: those of an earlier version of the daemon, which kept `ch.pid` in the VM's directory
        // (written by the VM's own user when rootless, so it is only a hint: the process is signalled only if its command line names
        // this directory's API socket), or of a daemon that died before it recorded the VM.
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || name == "base-snapshot" || name == "snapshot-cache" || seen.contains(&name) {
                    continue;
                }
                let dir = entry.path();
                if !dir.is_dir() {
                    continue;
                }
                if self.sweep_vm(&dir).await {
                    stopped += 1;
                }
            }
        }
        stopped
    }

    /// Stop the Cloud Hypervisor of one VM directory if it is still there, clear its user, remove it. Returns whether a process was stopped.
    async fn sweep_vm(&self, dir: &Path) -> bool {
        let pid = crate::ctl::read_pid(dir).or_else(|| legacy_pid(dir));
        let mut stopped = false;
        if let Some(pid) = pid {
            if crate::ctl::pid_alive(pid) && crate::ctl::signal_ch(pid, dir, libc::SIGTERM) {
                stopped = true;
                for _ in 0..200 {
                    if !crate::ctl::pid_alive(pid) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                if crate::ctl::pid_alive(pid) {
                    crate::ctl::signal_ch(pid, dir, libc::SIGKILL);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        }
        crate::rootless::release(dir).await;
        crate::ctl::remove(dir);
        let _ = std::fs::remove_dir_all(dir);
        stopped
    }

    /// Destroy a VM: its Cloud Hypervisor, its user's processes, its ACL entries, its record and its directory. The directory is the
    /// one the daemon names for the VM (`state_dir/<vm_id>`), and only for an ID that is a plain identifier.
    async fn destroy_vm(&self, vm: &PoolVm) {
        use crate::vm_lifecycle;

        if !crate::validate::is_id(&vm.vm_id) {
            warn!("not destroying a VM with the name {:?}", vm.vm_id);
            return;
        }
        let state_dir = PathBuf::from(&self.config.state_dir).join(&vm.vm_id);
        let api_socket = state_dir.join("api.sock");
        vm_lifecycle::shutdown_and_destroy(&api_socket, vm.ch_pid, &state_dir).await;
    }
}

/// The pid an earlier version of the daemon kept in the VM's own directory (see `sweep_orphans`).
fn legacy_pid(dir: &Path) -> Option<u32> {
    std::fs::read_to_string(dir.join("ch.pid"))
        .ok()
        .and_then(|t| t.trim().parse::<u32>().ok())
        .filter(|p| crate::ctl::valid_pid(*p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(id: &str, pid: u32) -> PoolVm {
        PoolVm {
            vm_id: id.to_string(),
            api_socket: PathBuf::from(format!("/nonexistent/{id}/api.sock")),
            vsock_socket: PathBuf::from(format!("/nonexistent/{id}/vsock.sock")),
            cid: 3,
            ch_pid: pid,
            created_at: std::time::Instant::now(),
        }
    }

    fn test_config() -> DaemonConfig {
        serde_json::from_str(r#"{"kernel_path":"/k","rootfs_path":"/r"}"#).expect("config")
    }

    #[tokio::test]
    async fn the_reaper_takes_out_only_the_vms_whose_process_is_gone() {
        let pool = Pool::new(test_config());
        let me = std::process::id(); // a process that exists
        let gone = 4_194_300; // above the usual pid_max, so no such process
        pool.ready.lock().await.push(vm("alive", me));
        pool.ready.lock().await.push(vm("dead", gone));
        let dead = pool.take_dead_ready().await;
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].vm_id, "dead");
        let ready = pool.ready.lock().await;
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].vm_id, "alive");
    }
}
