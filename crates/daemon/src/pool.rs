use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use log::{info, warn};
use tokio::sync::Mutex;

use crate::config::DaemonConfig;

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

    /// Release a VM. The VM is destroyed (not recycled).
    pub async fn release(&self, vm_id: &str) -> Result<()> {
        let removed = self.active.lock().await.remove(vm_id).is_some();
        if removed {
            self.active_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        info!(
            "released VM {} (active={}), destroying...",
            vm_id,
            self.active_count()
        );

        // Destroy the VM
        self.destroy_vm(vm_id).await;
        Ok(())
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
                    if let Some(dir) = vm.api_socket.parent() {
                        let _ = std::fs::remove_dir_all(dir);
                    }
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
            .partition(|vm| std::path::Path::new(&format!("/proc/{}", vm.ch_pid)).exists());
        *ready = alive;
        dead
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
        config_path.exists() && memory_path.exists() && state_path.exists()
    }

    /// Create a base snapshot by cold-booting a VM, waiting for the agent,
    /// then pausing and snapshotting.
    async fn create_base_snapshot(&self, base_dir: &std::path::Path) -> Result<()> {
        use crate::vm_lifecycle;

        std::fs::create_dir_all(base_dir)?;
        let vm_id = format!("base-snapshot-{}", std::process::id());
        let cid = self.next_cid();
        let state_dir = PathBuf::from(&self.config.state_dir).join(&vm_id);
        std::fs::create_dir_all(&state_dir)?;

        // Boot a clean VM (no container disks, no networking)
        let ch_pid = vm_lifecycle::boot_vm(
            &self.config,
            &vm_id,
            &state_dir,
            cid,
            None, // no TAP
            None, // no MAC
            &[],  // no extra disks
        )
        .await
        .context("boot base VM")?;

        let api_socket = state_dir.join("api.sock");
        let vsock_socket = state_dir.join("vsock.sock");

        // Wait for agent to be ready
        vm_lifecycle::wait_for_agent(&vsock_socket)
            .await
            .context("agent connect for base snapshot")?;

        info!("base VM ready (pid={}), snapshotting...", ch_pid);

        // Pause → snapshot → destroy
        vm_lifecycle::pause_vm(&api_socket).await?;
        vm_lifecycle::snapshot_vm(&api_socket, base_dir).await?;

        // Shut down the base VM
        vm_lifecycle::shutdown_and_destroy(&api_socket, ch_pid, &state_dir).await;

        info!("base snapshot created successfully");
        Ok(())
    }

    /// Restore one VM from the base snapshot.
    async fn restore_one(&self) -> Result<PoolVm> {
        use crate::vm_lifecycle;

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
        let snapshot_dir =
            vm_lifecycle::prepare_snapshot(&base_dir, &state_dir, cid, &vsock_socket)?;
        let t_prepare = t0.elapsed().as_millis();

        // 2. Spawn CH process
        let ch_pid = vm_lifecycle::spawn_ch(&self.config, &state_dir).await?;
        let t_spawn = t0.elapsed().as_millis();

        // 3. Wait for CH API socket
        let api_socket = state_dir.join("api.sock");
        vm_lifecycle::wait_ch_ready(&api_socket).await?;
        let t_ready = t0.elapsed().as_millis();

        // 4. vm.restore
        vm_lifecycle::restore_vm(&api_socket, &snapshot_dir).await?;
        let t_restore = t0.elapsed().as_millis();

        // 5. vm.resume
        vm_lifecycle::resume_vm(&api_socket).await?;
        let t_resume = t0.elapsed().as_millis();

        // 6. Wait for agent
        vm_lifecycle::wait_for_agent(&vsock_socket)
            .await
            .context("agent connect after restore")?;
        let t_agent = t0.elapsed().as_millis();

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

    /// Destroy a VM by shutting down CH and cleaning up state.
    /// Stop every VM this daemon owns: the ready ones, the ones handed to pods, and any Cloud Hypervisor process that is left in the
    /// state directory (a shadow VM, or one of an earlier daemon that was killed). Called when the daemon is told to stop, so that its
    /// VMs do not outlive it: a VM that does is held in memory (one resident guest, 38 to 133 MiB) and known to nobody.
    pub async fn shutdown_all(&self) {
        let mut ids: Vec<String> = self.ready.lock().await.drain(..).map(|v| v.vm_id).collect();
        ids.extend(self.active.lock().await.drain().map(|(id, _)| id));
        self.active_count
            .store(0, std::sync::atomic::Ordering::SeqCst);
        info!("stopping {} VMs", ids.len());
        for id in ids {
            self.destroy_vm(&id).await;
        }
        let left = self.sweep_orphans().await;
        if left > 0 {
            info!("stopped {left} more Cloud Hypervisor processes found in the state directory");
        }
    }

    /// Stop every Cloud Hypervisor process the state directory records, and remove every VM directory in it. The directories kept are the
    /// base snapshot and the cache of warm snapshots; everything else (`pool-*`, `warm-*`, `shadow-*`, the base VM's) belongs to a VM that
    /// is, or was, someone's, and when this runs (at start, before the pool is filled; at stop, after the VMs were stopped) nothing is
    /// using it. A process is signalled only if its command line names cloud-hypervisor and that VM's directory, so that a PID the
    /// system has given to something else is left alone. Returns how many processes were stopped.
    ///
    /// This is what makes the cleanup certain. A daemon that is stopped (SIGTERM, SIGINT) cleans up before it exits; one that is killed
    /// (SIGKILL, the OOM killer, a crash) cannot run anything, and its VMs and directories are removed by the next daemon at its start.
    pub async fn sweep_orphans(&self) -> usize {
        let root = PathBuf::from(&self.config.state_dir);
        let mut stopped = 0;
        let Ok(entries) = std::fs::read_dir(&root) else {
            return 0;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if !dir.is_dir() || name == "base-snapshot" || name == "snapshot-cache" {
                continue;
            }
            let pid = std::fs::read_to_string(dir.join("ch.pid"))
                .ok()
                .and_then(|t| t.trim().parse::<u32>().ok());
            if let Some(pid) = pid {
                let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
                if cmdline.contains("cloud-hypervisor") && cmdline.contains(&*dir.to_string_lossy())
                {
                    vm_lifecycle_stop(pid, &dir).await;
                    stopped += 1;
                    continue;
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
        stopped
    }

    async fn destroy_vm(&self, vm_id: &str) {
        use crate::vm_lifecycle;

        let state_dir = PathBuf::from(&self.config.state_dir).join(vm_id);
        let api_socket = state_dir.join("api.sock");

        // Find CH PID from the state dir
        // Best effort — if the process is already gone, that's fine
        if let Ok(entries) = std::fs::read_dir(&state_dir) {
            for entry in entries.flatten() {
                if entry.file_name() == "ch.pid" {
                    if let Ok(pid_str) = std::fs::read_to_string(entry.path()) {
                        if let Ok(pid) = pid_str.trim().parse::<u32>() {
                            vm_lifecycle::shutdown_and_destroy(&api_socket, pid, &state_dir).await;
                            return;
                        }
                    }
                }
            }
        }

        // Fallback: just clean up the directory
        let _ = std::fs::remove_dir_all(&state_dir);
    }
}

/// SIGTERM, then (after a second) SIGKILL; the VM's directory goes with it.
async fn vm_lifecycle_stop(pid: u32, dir: &std::path::Path) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    for _ in 0..200 {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_dir_all(dir);
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    let _ = std::fs::remove_dir_all(dir);
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
