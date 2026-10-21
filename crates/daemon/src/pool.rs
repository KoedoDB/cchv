use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use log::{info, warn};
use tokio::sync::Mutex;

use crate::config::DaemonConfig;

/// The file that says the base snapshot is complete and handed over (see `Pool::base_snapshot_valid`). It holds the fingerprint of what the snapshot was made from
/// (`base_fingerprint`).
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
    /// The VMs taken over from an earlier daemon (`adopt`) that no shim has spoken to since, with the time they were taken over. A pod's
    /// shim asks about its VM at once (it was waiting for the daemon to come back); one that nobody asks about has no pod any more.
    unclaimed: std::sync::Mutex<HashMap<String, std::time::Instant>>,
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
            unclaimed: std::sync::Mutex::new(HashMap::new()),
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
            self.mark_handed_out(&vm);
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
        self.mark_handed_out(&vm);
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
        self.mark_handed_out(&vm);
        self.active_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.active.lock().await.insert(vm.vm_id.clone(), vm);
    }

    /// Leave in the registry what the next daemon needs to take this VM over (see `adopt`). Failing to is not an error for the pod: the
    /// VM runs, and is only stopped, as it always was, if the daemon is replaced.
    fn mark_handed_out(&self, vm: &PoolVm) {
        let state_dir = PathBuf::from(&self.config.state_dir).join(&vm.vm_id);
        if let Err(e) = crate::ctl::mark_active(&state_dir, vm.cid) {
            warn!(
                "VM {} cannot be recorded for a restart of the daemon, and will be stopped by it: {e:#}",
                vm.vm_id
            );
        }
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

    /// The VMs handed to pods whose Cloud Hypervisor is gone: no process, a zombie, or a process that is not that VM's (the pid was reused).
    /// A VM without a pid yet is left alone. This looks at the process table, not at the registry (see `liveness.rs`).
    pub async fn dead_active(&self) -> Vec<PoolVm> {
        let state_dir = PathBuf::from(&self.config.state_dir);
        self.active
            .lock()
            .await
            .values()
            .filter(|vm| {
                vm.ch_pid != 0
                    && (!crate::ctl::pid_alive(vm.ch_pid)
                        || !crate::ctl::is_ch_of(vm.ch_pid, &state_dir.join(&vm.vm_id)))
            })
            .cloned()
            .collect()
    }

    /// The Cloud Hypervisor API socket of a VM handed to a pod.
    pub fn api_socket_of(&self, vm_id: &str) -> Option<PathBuf> {
        self.active
            .try_lock()
            .ok()?
            .get(vm_id)
            .map(|vm| vm.api_socket.clone())
    }

    /// Kill the Cloud Hypervisor of a VM handed to a pod (its guest is hung). Only if that process is that VM's. Returns whether it was signalled;
    /// the VM is released by the watch of the processes after that.
    pub fn kill_ch(&self, vm_id: &str) -> bool {
        let Ok(active) = self.active.try_lock() else {
            return false;
        };
        let Some(vm) = active.get(vm_id) else {
            return false;
        };
        let dir = PathBuf::from(&self.config.state_dir).join(&vm.vm_id);
        vm.ch_pid != 0 && crate::ctl::signal_ch(vm.ch_pid, &dir, libc::SIGKILL)
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

    /// The VM is for the daemon's own use (a shadow VM that warms an image up), not a pod's: the next daemon must not take it over (it would
    /// have no owner, as the work that used it is gone with the daemon), so it is not recorded as handed to a pod.
    pub fn disown(&self, vm_id: &str) {
        let state_dir = PathBuf::from(&self.config.state_dir).join(vm_id);
        crate::ctl::clear_active(&state_dir);
    }

    /// A shim has spoken about the VM: it has a pod.
    pub fn claim(&self, vm_id: &str) {
        let mut unclaimed = self.unclaimed.lock().unwrap_or_else(|e| e.into_inner());
        if !unclaimed.is_empty() {
            unclaimed.remove(vm_id);
        }
    }

    /// The taken-over VMs that nobody has spoken about for `after`; they are no longer listed as unclaimed.
    pub fn take_unclaimed(&self, after: std::time::Duration) -> Vec<String> {
        let mut unclaimed = self.unclaimed.lock().unwrap_or_else(|e| e.into_inner());
        let stale: Vec<String> = unclaimed
            .iter()
            .filter(|(_, since)| since.elapsed() >= after)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &stale {
            unclaimed.remove(id);
        }
        stale
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

    /// Check if the base snapshot is valid: complete, and made from the kernel, the rootfs and the VM size that are in place now. A snapshot holds the guest's memory, so one made from an
    /// older rootfs.erofs still runs the older agent in it: reusing it would leave a new agent (or kernel) unused until someone removes the directory by hand.
    fn base_snapshot_valid(&self, base_dir: &std::path::Path) -> bool {
        let config_path = base_dir.join("config.json");
        let memory_path = base_dir.join("memory-ranges");
        let state_path = base_dir.join("state.json");
        // The marker is written by the daemon after the VM that made the snapshot was destroyed and the snapshot handed over: a snapshot
        // without it was not finished by a daemon that vouches for it (it may be what a crash left).
        if !(config_path.exists() && memory_path.exists() && state_path.exists()) {
            return false;
        }
        match std::fs::read_to_string(base_dir.join(BASE_SNAPSHOT_DONE)) {
            Ok(made_from) if made_from == base_fingerprint(&self.config) => true,
            Ok(made_from) => {
                // An empty one is a snapshot of a daemon that wrote no fingerprint.
                info!(
                    "the base snapshot was made from something else (made from {:?}, now {:?}): it is made again",
                    made_from,
                    base_fingerprint(&self.config)
                );
                false
            }
            Err(_) => false,
        }
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
            crate::rootless::discard_outbox(&state_dir);
            let _ = std::fs::remove_dir_all(base_dir);
            return Err(e);
        }
        // A confined VM wrote its snapshot into its outbox: it is moved to where it will live, still untrusted until handed over.
        if let Err(e) = crate::rootless::collect_outbox(&state_dir, base_dir) {
            let _ = std::fs::remove_dir_all(base_dir);
            return Err(e);
        }
        if let Err(e) = crate::rootless::hand_over_snapshot(base_dir, true) {
            let _ = std::fs::remove_dir_all(base_dir);
            return Err(e);
        }
        if let Err(e) = std::fs::write(
            base_dir.join(BASE_SNAPSHOT_DONE),
            base_fingerprint(&self.config),
        ) {
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
        let snapshot_dir = match vm_lifecycle::prepare_snapshot(
            &self.config,
            &base_dir,
            &state_dir,
            cid,
            &vsock_socket,
            None,
        ) {
            Ok(d) => d,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&state_dir);
                return Err(e);
            }
        };
        let t_prepare = t0.elapsed().as_millis();

        // 2. Spawn CH process
        let ch_pid = match vm_lifecycle::spawn_ch(
            &self.config,
            &state_dir,
            self.config.default_memory_mb,
            self.config.default_vcpus,
        )
        .await
        {
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

    /// The daemon is told to stop. The VMs handed to pods keep running: they are the customers' (a database, a web server), and replacing the
    /// daemon (an update, a restart) must not end them; the next daemon takes them over (`adopt`). Everything else goes: the VMs waiting in
    /// the pool, and any Cloud Hypervisor process left in the state directory that is not a pod's (a shadow VM, one that was being made).
    /// `keep_active` false stops the pods' VMs too (the daemon is being removed, or the operator wants everything down).
    pub async fn shutdown_all(&self, keep_active: bool) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mut vms: Vec<PoolVm> = self.ready.lock().await.drain(..).collect();
        let kept: HashSet<String> = if keep_active {
            self.active.lock().await.keys().cloned().collect()
        } else {
            vms.extend(self.active.lock().await.drain().map(|(_, vm)| vm));
            self.active_count
                .store(0, std::sync::atomic::Ordering::SeqCst);
            HashSet::new()
        };
        info!(
            "stopping {} VMs, leaving {} to their pods",
            vms.len(),
            kept.len()
        );
        for vm in &vms {
            self.destroy_vm(vm).await;
        }
        let left = self.sweep_orphans(&kept).await;
        if left > 0 {
            info!("stopped {left} more Cloud Hypervisor processes found in the state directory");
        }
    }

    /// At start: take over the VMs the earlier daemon handed to pods and that still run. A VM is taken over if its registry says it was
    /// handed out (`ctl::mark_active`), its Cloud Hypervisor is that VM's (`ctl::is_ch_of`: the command line names the VM's API socket and,
    /// rootless, it runs as the VM's user) and answers on its API socket. It is put back among the active VMs (so the counter, the
    /// CIDs and the pod's calls are as they were); its agent connection is made again by the API server (`ApiServer::adopt`). Every other
    /// VM of the earlier daemon is stopped, as before (`sweep_orphans`). Returns the VMs taken over.
    pub async fn adopt(&self) -> Vec<PoolVm> {
        let root = PathBuf::from(&self.config.state_dir);
        let mut taken: Vec<PoolVm> = Vec::new();
        for name in crate::ctl::registered(&root) {
            if !crate::validate::is_id(&name) {
                continue;
            }
            let dir = root.join(&name);
            let Some(cid) = crate::ctl::read_active_cid(&dir) else {
                continue;
            };
            let Some(pid) = crate::ctl::read_pid(&dir) else {
                continue;
            };
            if !crate::ctl::pid_alive(pid) || !crate::ctl::is_ch_of(pid, &dir) {
                continue;
            }
            let api_socket = dir.join("api.sock");
            if let Err(e) = crate::vm_lifecycle::api_request_timeout(
                &api_socket,
                "GET",
                "/api/v1/vmm.ping",
                None,
                std::time::Duration::from_secs(5),
            )
            .await
            {
                warn!("VM {name} (pid {pid}) is not taken over, its Cloud Hypervisor does not answer: {e:#}");
                continue;
            }
            taken.push(PoolVm {
                vm_id: name,
                api_socket,
                vsock_socket: dir.join("vsock.sock"),
                cid,
                ch_pid: pid,
                created_at: std::time::Instant::now(),
            });
        }
        // The next CID after the largest in use: a CID is the guest's address on the host's vsock, and two VMs must not have one.
        if let Some(max) = taken.iter().map(|v| v.cid).max() {
            self.next_cid
                .fetch_max(max + 1, std::sync::atomic::Ordering::SeqCst);
        }
        {
            let mut active = self.active.lock().await;
            for vm in &taken {
                crate::rootless::adopt(&root.join(&vm.vm_id));
                active.insert(vm.vm_id.clone(), vm.clone());
            }
        }
        self.active_count
            .fetch_add(taken.len(), std::sync::atomic::Ordering::SeqCst);
        {
            let now = std::time::Instant::now();
            let mut unclaimed = self.unclaimed.lock().unwrap_or_else(|e| e.into_inner());
            for vm in &taken {
                unclaimed.insert(vm.vm_id.clone(), now);
            }
        }
        let keep: HashSet<String> = taken.iter().map(|v| v.vm_id.clone()).collect();
        let swept = self.sweep_orphans(&keep).await;
        if !taken.is_empty() || swept > 0 {
            info!(
                "took over {} VMs of the earlier daemon, stopped {} that were not a pod's",
                taken.len(),
                swept
            );
        }
        taken
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
        std::fs::set_permissions(
            self.config.snapshot_cache_dir(),
            std::fs::Permissions::from_mode(0o711),
        )?;
        let ctl = root.join(crate::ctl::CTL_DIR);
        std::fs::create_dir_all(&ctl)?;
        std::fs::set_permissions(&ctl, std::fs::Permissions::from_mode(0o700))?;
        // The images' directory is made by the shim; a confined VM has it bound into its root, so it has to exist.
        if self.config.ch_rootless && self.config.ch_confine {
            std::fs::create_dir_all(&self.config.erofs_cache_dir)?;
        }
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
    pub async fn sweep_orphans(&self, keep: &HashSet<String>) -> usize {
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
                seen.insert(name.clone());
                if keep.contains(&name) {
                    continue;
                }
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
                if name.starts_with('.')
                    || name == "base-snapshot"
                    || name == "snapshot-cache"
                    || seen.contains(&name)
                    || keep.contains(&name)
                {
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
/// What a base snapshot depends on: the kernel and the rootfs.erofs (size and modification time of each: reading tens of megabytes at every start to hash them is not worth it), the kernel
/// arguments, and the size of the default VM. A file that cannot be read counts as "missing", which differs from any file that is there.
fn base_fingerprint(config: &DaemonConfig) -> String {
    fn file_part(path: &str) -> String {
        match std::fs::metadata(path) {
            Ok(m) => {
                let nanos = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                format!("{path}:{}:{nanos}", m.len())
            }
            Err(_) => format!("{path}:missing"),
        }
    }
    format!(
        "kernel={};rootfs={};args={};vcpus={};memory_mb={}",
        file_part(&config.kernel_path),
        file_part(&config.rootfs_path),
        config.kernel_args,
        config.default_vcpus,
        config.default_memory_mb
    )
}

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
    #[tokio::test]
    async fn an_active_vm_is_dead_when_its_process_is_gone_or_is_not_its_cloud_hypervisor() {
        let tmp = std::env::temp_dir().join(format!("cchv-dead-active-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("tmp");
        let mut config = test_config();
        config.state_dir = tmp.to_string_lossy().into_owned();
        let pool = Pool::new(config);
        let (_d1, mut alive) = running_vm_dir(&tmp, "alive", true);
        let (_d2, mut dying) = running_vm_dir(&tmp, "dying", true);
        {
            let mut active = pool.active.lock().await;
            active.insert("alive".into(), vm("alive", alive.id()));
            active.insert("dying".into(), vm("dying", dying.id()));
            // A process that exists but is not this VM's Cloud Hypervisor (the pid was reused): this test's own.
            active.insert("reused".into(), vm("reused", std::process::id()));
            // No pid yet: left alone.
            active.insert("nopid".into(), vm("nopid", 0));
        }
        dying.kill().expect("kill");
        dying.wait().expect("wait");
        let mut dead: Vec<String> = pool.dead_active().await.into_iter().map(|v| v.vm_id).collect();
        dead.sort();
        let _ = alive.kill();
        let _ = alive.wait();
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(dead, vec!["dying".to_string(), "reused".to_string()]);
    }

    /// A VM an earlier daemon handed to a pod: a process with `--api-socket <dir>/api.sock` in its command line (a shell that starts a sleep), an
    /// API socket that answers the ping, and the registry's record.
    fn running_vm_dir(state: &Path, name: &str, handed_out: bool) -> (PathBuf, std::process::Child) {
        use std::os::unix::process::CommandExt;
        let dir = state.join(name);
        std::fs::create_dir_all(&dir).expect("vm dir");
        let child = std::process::Command::new("sh")
            // `; :` so that the shell does not replace itself with the sleep (and lose its arguments).
            .args(["-c", "sleep 30; :", "--api-socket"])
            .arg(dir.join("api.sock"))
            .process_group(0)
            .spawn()
            .expect("spawn");
        crate::ctl::write_pid(&dir, child.id()).expect("pid");
        if handed_out {
            crate::ctl::mark_active(&dir, 41).expect("active");
        }
        (dir, child)
    }

    /// Answer every request on `sock` with 204, as the Cloud Hypervisor does a ping.
    fn answer_pings(sock: &Path) {
        let listener = std::os::unix::net::UnixListener::bind(sock).expect("bind");
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
            }
        });
    }

    #[test]
    fn a_taken_over_vm_that_no_shim_speaks_about_is_given_up_after_a_while() {
        let pool = Pool::new(test_config());
        {
            let now = std::time::Instant::now();
            let mut u = pool.unclaimed.lock().unwrap();
            u.insert("sized-1-1".into(), now);
            u.insert("sized-1-2".into(), now);
        }
        pool.claim("sized-1-1"); // its shim came back
        assert!(pool.take_unclaimed(std::time::Duration::from_secs(600)).is_empty(), "not yet");
        assert_eq!(pool.take_unclaimed(std::time::Duration::ZERO), vec!["sized-1-2".to_string()]);
        assert!(pool.take_unclaimed(std::time::Duration::ZERO).is_empty(), "only once");
    }

    #[tokio::test]
    async fn a_new_daemon_takes_over_the_vms_handed_to_pods_and_stops_the_rest() {
        let state = std::env::temp_dir().join(format!("cchv-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        std::fs::create_dir_all(&state).unwrap();
        let config: DaemonConfig = serde_json::from_str(&format!(
            r#"{{"kernel_path":"/k","rootfs_path":"/r","state_dir":"{}"}}"#,
            state.display()
        ))
        .expect("config");

        // A pod's VM, a VM that was only in the pool, and a pod's VM whose Cloud Hypervisor does not answer.
        let (pods, mut pods_proc) = running_vm_dir(&state, "sized-1-1", true);
        answer_pings(&pods.join("api.sock"));
        let (pooled, mut pooled_proc) = running_vm_dir(&state, "pool-1-2", false);
        answer_pings(&pooled.join("api.sock"));
        let (mute, mut mute_proc) = running_vm_dir(&state, "sized-1-3", true);
        let _silent = std::os::unix::net::UnixListener::bind(mute.join("api.sock")).unwrap();

        let pool = Pool::new(config);
        let taken = pool.adopt().await;

        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].vm_id, "sized-1-1");
        assert_eq!(taken[0].cid, 41);
        assert_eq!(pool.active_count(), 1);
        assert!(pool.get_active("sized-1-1").await.is_some());
        assert_eq!(pool.next_cid_pub(), 42, "no CID in use is given again");
        assert!(
            pods_proc.try_wait().unwrap().is_none(),
            "the pod's VM still runs"
        );
        assert!(pods.exists() && crate::ctl::read_active_cid(&pods) == Some(41));
        // The other two are stopped; their directories go. (The process may not be reaped yet: look at the directory, and wait.)
        assert!(!pooled.exists());
        assert!(!mute.exists());
        let _ = pooled_proc.wait();
        let _ = mute_proc.wait();
        assert!(pooled_proc.try_wait().unwrap().is_some());

        // Stopping the daemon leaves the pod's VM; stopping it with keep off does not.
        pool.shutdown_all(true).await;
        assert!(pods_proc.try_wait().unwrap().is_none());
        pool.shutdown_all(false).await;
        let _ = pods_proc.wait();
        assert!(!pods.exists());
        let _ = std::fs::remove_dir_all(&state);
    }

    /// A directory of its own under the temp dir, with a kernel and a rootfs in it.
    fn files_dir(name: &str) -> (PathBuf, DaemonConfig) {
        let dir =
            std::env::temp_dir().join(format!("cchv-fingerprint-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("vmlinux"), b"kernel").expect("kernel");
        std::fs::write(dir.join("rootfs.erofs"), b"rootfs").expect("rootfs");
        let config: DaemonConfig = serde_json::from_str(&format!(
            r#"{{"kernel_path":"{0}/vmlinux","rootfs_path":"{0}/rootfs.erofs"}}"#,
            dir.display()
        ))
        .expect("config");
        (dir, config)
    }

    /// Put a complete-looking base snapshot made from the files as they are now.
    fn snapshot_of(dir: &Path, marker: &str) -> PathBuf {
        let base = dir.join("base-snapshot");
        std::fs::create_dir_all(&base).expect("base");
        for f in ["config.json", "memory-ranges", "state.json"] {
            std::fs::write(base.join(f), b"{}").expect("file");
        }
        std::fs::write(base.join(BASE_SNAPSHOT_DONE), marker).expect("marker");
        base
    }

    #[test]
    fn a_snapshot_made_from_the_files_in_place_is_reused() {
        let (dir, config) = files_dir("same");
        let base = snapshot_of(&dir, &base_fingerprint(&config));
        assert!(Pool::new(config).base_snapshot_valid(&base));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_new_rootfs_or_kernel_makes_the_snapshot_stale() {
        let (dir, config) = files_dir("changed");
        let base = snapshot_of(&dir, &base_fingerprint(&config));
        // A different size, as a new agent in the rootfs makes.
        std::fs::write(dir.join("rootfs.erofs"), b"rootfs with a new agent").expect("rootfs");
        assert!(!Pool::new(config.clone()).base_snapshot_valid(&base));
        let base = snapshot_of(&dir, &base_fingerprint(&config));
        assert!(Pool::new(config.clone()).base_snapshot_valid(&base));
        std::fs::write(dir.join("vmlinux"), b"kernel 2").expect("kernel");
        assert!(!Pool::new(config).base_snapshot_valid(&base));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_snapshot_with_no_fingerprint_or_for_another_vm_size_is_stale() {
        let (dir, config) = files_dir("old");
        // What an earlier daemon wrote: an empty marker.
        let base = snapshot_of(&dir, "");
        assert!(!Pool::new(config.clone()).base_snapshot_valid(&base));
        let base = snapshot_of(&dir, &base_fingerprint(&config));
        let mut bigger = config.clone();
        bigger.default_memory_mb += 64;
        assert!(!Pool::new(bigger).base_snapshot_valid(&base));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_snapshot_without_its_marker_is_stale() {
        let (dir, config) = files_dir("nomarker");
        let base = snapshot_of(&dir, "x");
        std::fs::remove_file(base.join(BASE_SNAPSHOT_DONE)).expect("remove");
        assert!(!Pool::new(config).base_snapshot_valid(&base));
        let _ = std::fs::remove_dir_all(dir);
    }
}
