//! Thin containerd shim instance that delegates VM lifecycle to the
//! sandbox daemon. The shim handles networking (TAP/tc), erofs rootfs
//! conversion, and container lifecycle RPCs. The daemon handles VM
//! boot, pooling, and snapshot management.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use containerd_shimkit::sandbox::instance::{ExecConfig, Instance, InstanceConfig};
use containerd_shimkit::sandbox::sync::WaitableCell;
use containerd_shimkit::sandbox::Error;
use log::info;

use crate::config::{load_config, load_shim_limits};
use crate::dockermode::{self, NetnsHolder};
use crate::daemon_client::{AcquireRequest, DaemonClient};
use crate::erofs_cache::{CacheConfig, Lease};
use crate::exec::ExecProc;
use crate::vmsize::{check_size, limits_from_spec, vm_size, PodLimits};

/// Milliseconds since Unix epoch for TIMING logs.
pub fn epoch_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

const EROFS_CACHE_DIR: &str = "/run/cloudhv/erofs-cache";
const CRI_CONTAINER_TYPE: &str = "/annotations/io.kubernetes.cri.container-type";
const CRI_SANDBOX_ID: &str = "/annotations/io.kubernetes.cri.sandbox-id";

trait ResultExt<T> {
    fn ctx(self, msg: &str) -> Result<T, Error>;
}

impl<T, E: std::fmt::Display> ResultExt<T> for Result<T, E> {
    fn ctx(self, msg: &str) -> Result<T, Error> {
        self.map_err(|e| Error::Any(anyhow::anyhow!("{msg}: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Static shared state
// ---------------------------------------------------------------------------

static VMS: LazyLock<RwLock<HashMap<String, Arc<SharedVmState>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// One lock per sandbox: two containers of a pod that start together must not both acquire a VM for it.
static SANDBOX_LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The leases on the erofs images the sandbox runs from, kept for as long as the sandbox is (see `erofs_cache`).
static LEASES: LazyLock<Mutex<HashMap<String, Vec<Lease>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn sandbox_lock(sandbox_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    SANDBOX_LOCKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(sandbox_id.to_string())
        .or_default()
        .clone()
}

/// The sandbox is over: forget its state, its lock and its leases on images.
fn forget_sandbox(sandbox_id: &str) {
    VMS.write().unwrap_or_else(|e| e.into_inner()).remove(sandbox_id);
    SANDBOX_LOCKS.lock().unwrap_or_else(|e| e.into_inner()).remove(sandbox_id);
    LEASES.lock().unwrap_or_else(|e| e.into_inner()).remove(sandbox_id);
}

/// Give the VM back to the daemon and clean up the TAP: best effort, for a sandbox that cannot go on.
async fn release_vm(vm_state: &SharedVmState) {
    if !vm_state.daemon_vm_id.is_empty() {
        let client = DaemonClient::new(&vm_state.daemon_socket);
        if let Err(e) = client.release_sandbox(&vm_state.daemon_vm_id).await {
            info!("daemon release failed (non-fatal): {e:#}");
        }
    }
    if let Some(ref cg) = vm_state.cgroups_path {
        remove_pod_cgroup_child(cg);
    }
    if let (Some(ref netns), Some(ref tap)) = (&vm_state.netns, &vm_state.tap_name) {
        crate::netns::cleanup_tap(netns, tap).await;
    }
}

fn get_vm(sandbox_id: &str) -> Option<Arc<SharedVmState>> {
    VMS.read()
        .unwrap_or_else(|e| e.into_inner())
        .get(sandbox_id)
        .cloned()
}

// ---------------------------------------------------------------------------
// Shared VM state (thin — daemon owns the VM)
// ---------------------------------------------------------------------------

struct SharedVmState {
    /// Cloud Hypervisor process PID (for exit watching + cgroup placement).
    ch_pid: u32,
    /// Container count (sandbox counts as 1).
    container_count: AtomicUsize,
    /// TAP device name (returned by daemon, used for cleanup).
    tap_name: Option<String>,
    netns: Option<String>,
    cgroups_path: Option<String>,
    /// VM ID assigned by the daemon (for release).
    daemon_vm_id: String,
    /// Daemon socket path.
    daemon_socket: String,
    /// The VM this pod needs, from its limits (0: the daemon's default). The VM is acquired when the first container starts.
    memory_mb: u64,
    vcpus: u32,
}

// ---------------------------------------------------------------------------
// CloudHvInstance — the shimkit Instance implementation
// ---------------------------------------------------------------------------

pub struct CloudHvInstance {
    id: String,
    sandbox_id: String,
    bundle: PathBuf,
    exit: Arc<WaitableCell<(u32, DateTime<Utc>)>>,
    /// What containerd gave this instance, including the pipes for its stdout and stderr (the container's output is written to them).
    cfg: InstanceConfig,
    /// The exec processes of this container, by exec id (see `exec.rs`).
    execs: Mutex<HashMap<String, Arc<ExecProc>>>,
    /// The forwarding of the stdin pipe to the container's terminal, for a container that has both (`kubectl run -it`, `attach`).
    init_input: Mutex<Option<Arc<ExecProc>>>,
    /// The spec has no CRI annotations: a client that is not Kubernetes (Docker, nerdctl). The one container is the whole "pod"
    /// (see `dockermode.rs`).
    docker: bool,
    /// The process that holds the container's network namespace, when the spec asks the runtime to make one (Docker does).
    holder: Option<Arc<NetnsHolder>>,
}

impl Instance for CloudHvInstance {
    async fn new(id: String, cfg: &InstanceConfig) -> Result<Self, Error> {
        let spec_path = cfg.bundle.join("config.json");
        let spec_str = std::fs::read_to_string(&spec_path)
            .map_err(|e| Error::Any(anyhow::anyhow!("read spec: {e}")))?;
        let spec: serde_json::Value = serde_json::from_str(&spec_str)
            .map_err(|e| Error::Any(anyhow::anyhow!("parse spec: {e}")))?;

        let sandbox_id = spec
            .pointer(CRI_SANDBOX_ID)
            .and_then(|v| v.as_str())
            .unwrap_or(&id)
            .to_string();

        // Not Kubernetes: no CRI annotations. If the spec asks for a new network namespace, make it now: the client takes it from the pid
        // that `State` shows right after `Create`, and sets its network up before `Start`.
        let docker = !dockermode::is_cri(&spec);
        let holder = if docker && dockermode::wants_new_netns(&spec) {
            let h = tokio::task::spawn_blocking(NetnsHolder::spawn)
                .await
                .map_err(|e| Error::Any(anyhow::anyhow!("namespace holder task: {e}")))?
                .map_err(|e| Error::Any(anyhow::anyhow!("{e}")))?;
            info!("container {id}: network namespace made by holder pid {}", h.pid());
            Some(Arc::new(h))
        } else {
            None
        };

        Ok(Self {
            id: id.clone(),
            sandbox_id,
            bundle: cfg.bundle.clone(),
            exit: Arc::new(WaitableCell::new()),
            cfg: cfg.clone(),
            execs: Mutex::default(),
            init_input: Mutex::default(),
            docker,
            holder,
        })
    }

    fn pid_hint(&self) -> Option<u32> {
        self.holder.as_ref().map(|h| h.pid())
    }

    async fn start(&self) -> Result<u32, Error> {
        let spec = std::fs::read_to_string(self.bundle.join("config.json"))
            .map_err(|e| Error::Any(anyhow::anyhow!("read spec: {e}")))?;
        let spec_json: serde_json::Value = serde_json::from_str(&spec)
            .map_err(|e| Error::Any(anyhow::anyhow!("parse spec: {e}")))?;

        let container_type = spec_json
            .pointer(CRI_CONTAINER_TYPE)
            .and_then(|v| v.as_str())
            .unwrap_or("container");

        match container_type {
            "sandbox" => self.start_sandbox(None).await,
            _ if self.docker => self.start_docker().await,
            _ => self.start_container().await,
        }
    }

    async fn kill(&self, signal: u32) -> Result<(), Error> {
        let t0 = std::time::Instant::now();
        // (Under Docker the container is its own sandbox, but a signal is for the container: the VM is given back at delete.)
        let is_sandbox = self.id == self.sandbox_id && !self.docker;
        info!(
            "TIMING kill_enter {} @{} signal={} is_sandbox={}",
            self.id,
            epoch_ms(),
            signal,
            is_sandbox
        );

        if let Some(vm_state) = get_vm(&self.sandbox_id) {
            if is_sandbox && (signal == 9 || signal == 15) {
                let t_release = std::time::Instant::now();

                // Release VM to daemon (if one was acquired)
                if !vm_state.daemon_vm_id.is_empty() {
                    let client = DaemonClient::new(&vm_state.daemon_socket);
                    if let Err(e) = client.release_sandbox(&vm_state.daemon_vm_id).await {
                        info!("daemon release failed (non-fatal): {e:#}");
                    }
                    if let Some(ref cg) = vm_state.cgroups_path {
                        remove_pod_cgroup_child(cg);
                    }
                }

                // Always clean up TAP and remove from VMS, even if no VM
                // was acquired or release failed — prevents leaked resources
                let t_tap = std::time::Instant::now();
                if let (Some(ref netns), Some(ref tap)) = (&vm_state.netns, &vm_state.tap_name) {
                    crate::netns::cleanup_tap(netns, tap).await;
                }
                let tap_ms = t_tap.elapsed().as_millis();

                forget_sandbox(&self.sandbox_id);

                info!(
                    "TIMING kill_sandbox {} @{}: release={}ms tap={}ms total={}ms",
                    self.id,
                    epoch_ms(),
                    t_release.elapsed().as_millis(),
                    tap_ms,
                    t0.elapsed().as_millis()
                );
                let _ = self.exit.set((0, Utc::now()));
            } else {
                // App container signal — proxy through daemon
                let t_rpc = std::time::Instant::now();
                let client = DaemonClient::new(&vm_state.daemon_socket);
                let _ = client
                    .kill_container(&vm_state.daemon_vm_id, &self.id, signal)
                    .await;
                info!(
                    "TIMING kill_container {} @{}: rpc={}ms total={}ms",
                    self.id,
                    epoch_ms(),
                    t_rpc.elapsed().as_millis(),
                    t0.elapsed().as_millis()
                );
            }
        } else {
            let _ = self.exit.set((0, Utc::now()));
            info!(
                "TIMING kill_no_vm {} @{}: total={}ms",
                self.id,
                epoch_ms(),
                t0.elapsed().as_millis()
            );
        }

        Ok(())
    }

    async fn delete(&self) -> Result<(), Error> {
        info!("CloudHvInstance::delete id={}", self.id);
        if let Some(p) = self.init_input.lock().unwrap_or_else(|e| e.into_inner()).take() {
            p.stop();
        }
        let t_total = std::time::Instant::now();

        if let Some(vm_state) = get_vm(&self.sandbox_id) {
            let prev = vm_state.container_count.fetch_sub(1, Ordering::SeqCst);

            if prev <= 1 {
                // Last container — release VM to daemon
                let client = DaemonClient::new(&vm_state.daemon_socket);
                if let Err(e) = client.release_sandbox(&vm_state.daemon_vm_id).await {
                    info!("daemon release failed (non-fatal): {e:#}");
                }
                if let Some(ref cg) = vm_state.cgroups_path {
                    remove_pod_cgroup_child(cg);
                }

                // Clean up TAP
                if let (Some(ref netns), Some(ref tap)) = (&vm_state.netns, &vm_state.tap_name) {
                    crate::netns::cleanup_tap(netns, tap).await;
                    info!("cleaned up TAP {tap}");
                }

                forget_sandbox(&self.sandbox_id);

                info!(
                    "TIMING delete {} @{}: total={}ms (sandbox released)",
                    self.id,
                    epoch_ms(),
                    t_total.elapsed().as_millis()
                );
            } else {
                // Container delete — proxy through daemon
                let client = DaemonClient::new(&vm_state.daemon_socket);
                let _ = client
                    .delete_container(&vm_state.daemon_vm_id, &self.id)
                    .await;

                info!(
                    "TIMING delete {} @{}: total={}ms (container only, {} remaining)",
                    self.id,
                    epoch_ms(),
                    t_total.elapsed().as_millis(),
                    prev - 1
                );
            }
        }

        // The network namespace of a Docker container is not needed any more (dockerd removes its own bind mount of it).
        if let Some(h) = &self.holder {
            h.kill();
        }

        let _ = self.exit.set((0, Utc::now()));
        Ok(())
    }

    async fn wait(&self) -> (u32, DateTime<Utc>) {
        let t0 = std::time::Instant::now();
        info!("TIMING wait_enter {} @{}", self.id, epoch_ms());
        {
            let result = *self.exit.wait().await;
            info!(
                "TIMING wait_done {} @{}: took={}ms exit_code={}",
                self.id,
                epoch_ms(),
                t0.elapsed().as_millis(),
                result.0
            );
            result
        }
    }

    async fn resize_pty(&self, width: u32, height: u32) -> Result<(), Error> {
        let vm = get_vm(&self.sandbox_id)
            .filter(|v| !v.daemon_vm_id.is_empty())
            .ok_or_else(|| Error::FailedPrecondition("the container has no VM".into()))?;
        DaemonClient::new(&vm.daemon_socket)
            .exec_resize(&vm.daemon_vm_id, &self.id, crate::exec::INIT_EXEC_ID, width, height)
            .await
            .map_err(Error::Any)
    }

    async fn close_stdin(&self) -> Result<(), Error> {
        let input = self.init_input.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match input {
            Some(p) => p.close_stdin().await,
            None => Ok(()),
        }
    }

    async fn exec_create(&self, cfg: &ExecConfig) -> Result<(), Error> {
        // (Under Docker the container is its own sandbox, and is the one that runs the exec.)
        if self.id == self.sandbox_id && !self.docker {
            return Err(Error::FailedPrecondition(
                "the sandbox (pause) container cannot run an exec process".into(),
            ));
        }
        let vm = get_vm(&self.sandbox_id)
            .filter(|v| !v.daemon_vm_id.is_empty())
            .ok_or_else(|| Error::FailedPrecondition("the container has no VM".into()))?;
        let proc = ExecProc::new(
            cfg.clone(),
            DaemonClient::new(&vm.daemon_socket),
            vm.daemon_vm_id.clone(),
            self.id.clone(),
        );
        proc.create().await?;
        self.execs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(cfg.exec_id.clone(), proc);
        Ok(())
    }

    async fn exec_start(&self, exec_id: &str) -> Result<u32, Error> {
        self.exec_proc(exec_id)?.start().await
    }

    async fn exec_kill(&self, exec_id: &str, signal: u32) -> Result<(), Error> {
        self.exec_proc(exec_id)?.kill(signal).await
    }

    async fn exec_wait(&self, exec_id: &str) -> Result<(u32, DateTime<Utc>), Error> {
        self.exec_proc(exec_id)?.wait().await
    }

    async fn exec_delete(&self, exec_id: &str) -> Result<(), Error> {
        let proc = self.exec_proc(exec_id)?;
        let res = proc.delete().await;
        self.execs.lock().unwrap_or_else(|e| e.into_inner()).remove(exec_id);
        res
    }

    async fn exec_resize(&self, exec_id: &str, width: u32, height: u32) -> Result<(), Error> {
        self.exec_proc(exec_id)?.resize(width, height).await
    }

    async fn exec_close_stdin(&self, exec_id: &str) -> Result<(), Error> {
        self.exec_proc(exec_id)?.close_stdin().await
    }
}

// ---------------------------------------------------------------------------
// Exec (the task service in shimkit keeps the order of the calls; see exec.rs)
// ---------------------------------------------------------------------------

impl CloudHvInstance {
    fn exec_proc(&self, exec_id: &str) -> Result<Arc<ExecProc>, Error> {
        self.execs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(exec_id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("exec process {exec_id}")))
    }
}

// ---------------------------------------------------------------------------
// Sandbox and container lifecycle
// ---------------------------------------------------------------------------

impl CloudHvInstance {
    /// Set up sandbox state. No VM interaction — the daemon provides the
    /// VM (including networking) when the first container starts.
    ///
    /// `netns`: the network namespace to use instead of the one of the spec (`Some(None)`: no network), for a client that made its own.
    async fn start_sandbox(&self, netns: Option<Option<String>>) -> Result<u32, Error> {
        let sandbox_id = self.id.clone();
        let t_total = std::time::Instant::now();

        let spec_path = self.bundle.join("config.json");
        let sandbox_spec = parse_sandbox_spec(&spec_path, self.docker);

        let config = load_config(None).ctx("config error")?;
        let limits = load_shim_limits(None).ctx("config error")?;

        // The size of this pod's VM, from the limits containerd put on the sandbox.
        let host_cpus = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1);
        let max_vcpus = [config.max_default_vcpus, limits.max_vcpus]
            .into_iter()
            .filter(|&n| n > 0)
            .min()
            .unwrap_or(host_cpus)
            .min(host_cpus);
        let size = vm_size(
            sandbox_spec.limits,
            config.default_memory_mb,
            config.default_vcpus,
            max_vcpus,
        );
        // The pod's limit makes the VM; a pod must not be able to ask for one the host cannot give.
        check_size(size, limits.max_memory_mb).map_err(|e| Error::Any(anyhow::anyhow!(e)))?;
        info!(
            "sandbox {} needs a VM of {} MiB and {} vCPUs (limits {:?})",
            sandbox_id, size.memory_mb, size.vcpus, sandbox_spec.limits
        );

        // Store sandbox state — no VM yet (daemon provides it in start_container)
        let vm_state = Arc::new(SharedVmState {
            ch_pid: 0,
            container_count: AtomicUsize::new(1),
            tap_name: None,
            netns: netns.unwrap_or_else(|| sandbox_spec.netns.clone()),
            cgroups_path: sandbox_spec.cgroups_path.clone(),
            daemon_vm_id: String::new(),
            daemon_socket: config.daemon_socket.clone(),
            memory_mb: size.memory_mb,
            vcpus: size.vcpus,
        });

        VMS.write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(sandbox_id.clone(), vm_state);

        info!(
            "TIMING start_sandbox {} @{}: total={}ms",
            sandbox_id,
            epoch_ms(),
            t_total.elapsed().as_millis()
        );

        Ok(std::process::id())
    }

    /// A container of a client that is not Kubernetes: no sandbox came first, so the sandbox's state is made here, then the container is
    /// started in it. The client has already set up the network in the namespace the holder keeps (it does so before `Start`); with
    /// none there (`--network none`) the VM has no network. A namespace of the spec's own (a path) is used as it is.
    async fn start_docker(&self) -> Result<u32, Error> {
        let netns = match &self.holder {
            Some(h) if h.has_interface() => Some(Some(h.netns_path())),
            Some(_) => Some(None),
            None => None,
        };
        self.start_sandbox(netns).await?;
        // The sandbox is not a container of its own here: the one container is counted once, so that its delete gives the VM back.
        if let Some(vm) = get_vm(&self.sandbox_id) {
            vm.container_count.store(0, Ordering::SeqCst);
        }
        let pid = self.start_container().await;
        match pid {
            // dockerd keeps the task's pid (it already took the namespace from it): the same as `State` showed since `Create`.
            Ok(pid) => Ok(self.holder.as_ref().map(|h| h.pid()).unwrap_or(pid)),
            Err(e) => {
                // The task does not start: the sandbox state made above must not stay.
                forget_sandbox(&self.sandbox_id);
                Err(e)
            }
        }
    }

    /// Acquire a VM from the daemon and start the container.
    async fn start_container(&self) -> Result<u32, Error> {
        let container_id = &self.id;
        let t_total = std::time::Instant::now();
        info!(
            "TIMING start_container_enter {} @{}",
            container_id,
            epoch_ms()
        );

        let limits = load_shim_limits(None).ctx("config error")?;

        // One container of the pod at a time up to the VM being acquired and recorded: two that start together would both find
        // no VM and acquire one each.
        let sandbox_lock = sandbox_lock(&self.sandbox_id);
        let _one_at_a_time = sandbox_lock.lock().await;

        let vm_state = get_vm(&self.sandbox_id).ok_or_else(|| {
            Error::Any(anyhow::anyhow!("sandbox VM not found: {}", self.sandbox_id))
        })?;

        let rootfs_path = {
            let spec: serde_json::Value = std::fs::read_to_string(self.bundle.join("config.json"))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default();
            dockermode::rootfs_path(&self.bundle, &spec, self.docker)
        };

        // The key of the erofs image (and of the daemon's warm snapshot) says what is mounted at the rootfs: the layers of the
        // mount, plus the image's digest if containerd gives it. When what is mounted cannot be told, the container does not start.
        let digest = image_digest_from_spec(&self.bundle).await;
        let image_key = crate::cachekey::rootfs_key(&rootfs_path, digest.as_deref())
            .map_err(|e| Error::Any(anyhow::anyhow!("cannot identify the rootfs of {container_id}: {e}")))?;

        // Convert rootfs to erofs (cached by the key). The shim does this because it has access to the bundle rootfs that
        // containerd prepared. Retry on transient failures (e.g. fork pressure under heavy load); a build is a blocking job
        // (mkfs.erofs for seconds), so it runs off the runtime's threads.
        let t_erofs = std::time::Instant::now();
        let cache = CacheConfig {
            dir: PathBuf::from(EROFS_CACHE_DIR),
            max_bytes: limits.erofs_cache_max_mib.saturating_mul(1024 * 1024),
            mkfs_candidates: limits.mkfs_candidates(),
            mkfs_timeout: Duration::from_secs(limits.mkfs_erofs_timeout_secs),
        };
        let erofs_path = {
            let mut last_err = None;
            let mut result = None;
            for attempt in 0..3u64 {
                let (cache_dir, max_bytes, cands, tmo) = (
                    cache.dir.clone(),
                    cache.max_bytes,
                    cache.mkfs_candidates.clone(),
                    cache.mkfs_timeout,
                );
                let (root, key) = (rootfs_path.clone(), image_key.clone());
                let built = tokio::task::spawn_blocking(move || {
                    crate::erofs_cache::prepare(
                        &CacheConfig {
                            dir: cache_dir,
                            max_bytes,
                            mkfs_candidates: cands,
                            mkfs_timeout: tmo,
                        },
                        &root,
                        &key,
                    )
                })
                .await
                .map_err(|e| format!("erofs build task: {e}"))
                .and_then(|r| r);
                match built {
                    Ok((path, lease)) => {
                        // The image stays in the cache while this sandbox runs.
                        LEASES
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .entry(self.sandbox_id.clone())
                            .or_default()
                            .push(lease);
                        result = Some(path);
                        break;
                    }
                    Err(e) => {
                        info!("prepare_erofs attempt {attempt} failed: {e}");
                        let fatal = e.contains("larger than the whole") || e.contains("cache is full") || e.contains("free space");
                        last_err = Some(e);
                        if fatal {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(100 * (attempt + 1))).await;
                    }
                }
            }
            result.ok_or_else(|| Error::Any(anyhow::anyhow!("erofs image: {}", last_err.unwrap_or_default())))?
        };
        let erofs_ms = t_erofs.elapsed().as_millis();

        // Read OCI config for the container
        let spec_path = self.bundle.join("config.json");
        let config_json = std::fs::read(&spec_path).unwrap_or_default();
        // The volumes are read from the host's files: files and directories of the pod, below the roots the runtime makes them in, and
        // never a link out of them. If one cannot be read, or is not allowed, the container is not started.
        let volumes = {
            let (cfg_json, roots) = (config_json.clone(), limits.volume_source_roots.clone());
            tokio::time::timeout(
                Duration::from_secs(30),
                tokio::task::spawn_blocking(move || crate::volumes::from_spec(&cfg_json, &roots)),
            )
            .await
            .map_err(|_| Error::Any(anyhow::anyhow!("reading the volumes took too long")))?
            .map_err(|e| Error::Any(anyhow::anyhow!("volume task: {e}")))?
            .map_err(|e| Error::Any(anyhow::anyhow!("{e}")))?
        };

        // Check if a VM is already acquired for this sandbox (multi-container pod).
        // Only the first container triggers daemon acquire + warm snapshot lookup.
        let already_acquired = !vm_state.daemon_vm_id.is_empty();

        let (active_state, from_snapshot, acquire_ms, container_pid) = if already_acquired {
            info!(
                "VM already acquired for sandbox {}, adding container {}",
                self.sandbox_id, container_id
            );
            // Additional container — daemon does hot-plug + RunContainer
            let client = DaemonClient::new(&vm_state.daemon_socket)
                .with_timeouts(limits.rpc_timeout_secs, limits.acquire_timeout_secs);
            let t_acquire = std::time::Instant::now();
            let resp = client
                .add_container(
                    &vm_state.daemon_vm_id,
                    &erofs_path.to_string_lossy(),
                    container_id,
                    &config_json,
                    &volumes,
                    self.cfg.terminal,
                )
                .await
                .ctx("daemon add_container")?;
            let acq_ms = t_acquire.elapsed().as_millis();
            (vm_state.clone(), false, acq_ms, resp)
        } else {
            // First container — acquire VM from daemon
            let t_acquire = std::time::Instant::now();
            let client = DaemonClient::new(&vm_state.daemon_socket)
                .with_timeouts(limits.rpc_timeout_secs, limits.acquire_timeout_secs);
            let acquired = client
                .acquire_sandbox(&AcquireRequest {
                    netns: vm_state.netns.as_deref().unwrap_or(""),
                    image_key: &image_key,
                    erofs_path: &erofs_path.to_string_lossy(),
                    container_id,
                    config_json: &config_json,
                    memory_mb: vm_state.memory_mb,
                    vcpus: vm_state.vcpus,
                    volumes: &volumes,
                    terminal: self.cfg.terminal,
                })
                .await
                .ctx("daemon acquire")?;
            let acq_ms = t_acquire.elapsed().as_millis();

            info!(
                "daemon acquired: vm_id={} ch_pid={} from_snapshot={} container_pid={} tap={} in {}ms",
                acquired.vm_id,
                acquired.ch_pid,
                acquired.from_snapshot,
                acquired.container_pid,
                acquired.tap_name,
                acq_ms
            );

            // Update shared state with daemon's VM info
            let new_state = Arc::new(SharedVmState {
                ch_pid: acquired.ch_pid,
                container_count: AtomicUsize::new(vm_state.container_count.load(Ordering::SeqCst)),
                tap_name: if acquired.tap_name.is_empty() {
                    None
                } else {
                    Some(acquired.tap_name.clone())
                },
                netns: vm_state.netns.clone(),
                cgroups_path: vm_state.cgroups_path.clone(),
                daemon_vm_id: acquired.vm_id.clone(),
                daemon_socket: vm_state.daemon_socket.clone(),
                memory_mb: vm_state.memory_mb,
                vcpus: vm_state.vcpus,
            });

            VMS.write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(self.sandbox_id.clone(), new_state.clone());

            // Place CH in the sandbox's cgroup. The sandbox cgroup is created
            // by containerd before start_sandbox, so it should exist. We place
            // CH there so containerd can track the VM process for resource
            // accounting and lifecycle management.
            if let Some(ref cg) = new_state.cgroups_path {
                match place_vm_in_pod_cgroup(acquired.ch_pid, acquired.ch_cgroup.as_deref(), cg) {
                    Ok(()) => info!("CH pid {} placed in cgroup {}", acquired.ch_pid, cg),
                    Err(e) if limits.require_pod_cgroup => {
                        // Outside the pod's cgroup the VM would run without the pod's limits: give it back and fail the pod.
                        log::error!("cgroup placement failed, the pod does not start: {e}");
                        release_vm(&new_state).await;
                        forget_sandbox(&self.sandbox_id);
                        return Err(Error::Any(anyhow::anyhow!(
                            "cannot place the VM in the pod's cgroup {cg}: {e}"
                        )));
                    }
                    Err(e) => info!("cgroup placement failed (require_pod_cgroup is off): {e}"),
                }
            }

            (
                new_state,
                acquired.from_snapshot,
                acq_ms,
                acquired.container_pid,
            )
        };

        // Set up exit watcher — daemon proxies WaitContainer, CH PID as backup
        {
            let exit = self.exit.clone();
            let cid = container_id.to_string();
            let ch_pid = active_state.ch_pid;
            let daemon_socket = active_state.daemon_socket.clone();
            let daemon_vm_id = active_state.daemon_vm_id.clone();
            let watched = active_state.clone();

            tokio::spawn(async move {
                let t0 = std::time::Instant::now();
                let client = DaemonClient::new(&daemon_socket);

                tokio::select! {
                    result = client.wait_container(&daemon_vm_id, &cid) => {
                        let (exit_code, _) = match result {
                            Ok(r) => r,
                            Err(e) => {
                                // The daemon cannot be asked about the container (it died, or the connection broke). Reporting 137
                                // and leaving the VM would orphan a VM that may still be running: give it back to the daemon (which
                                // stops it, or has already) before the pod is restarted.
                                log::warn!("waiting for {cid} failed, releasing its VM: {e:#}");
                                release_vm(&watched).await;
                                (137, String::new())
                            }
                        };
                        let _ = exit.set((exit_code, Utc::now()));
                        log::info!(
                            "TIMING exit_watcher {} @{}: daemon_wait code={} took={}ms",
                            cid, epoch_ms(), exit_code, t0.elapsed().as_millis()
                        );
                    }
                    _ = wait_for_pid_exit(ch_pid) => {
                        let _ = exit.set((137, Utc::now()));
                        log::info!(
                            "TIMING exit_watcher {} @{}: ch_pid_exit took={}ms",
                            cid, epoch_ms(), t0.elapsed().as_millis()
                        );
                    }
                }
            });
        }

        // Carry the container's stdout and stderr from the guest to the pipes containerd gave us. Without this the output
        // reaches nobody and `kubectl logs` is empty.
        {
            let cfg = self.cfg.clone();
            let daemon_socket = active_state.daemon_socket.clone();
            let daemon_vm_id = active_state.daemon_vm_id.clone();
            let cid = container_id.to_string();
            tokio::spawn(async move {
                pump_logs(cfg, daemon_socket, daemon_vm_id, cid).await;
            });
        }

        // A container with a terminal and a stdin pipe (the client has input: `kubectl run -it`, `kubectl attach -i`): forward what is
        // written to the pipe to the container's terminal. (Without a terminal the container's stdin is /dev/null in the guest.)
        if self.cfg.terminal && !self.cfg.stdin.as_os_str().is_empty() {
            let input = ExecProc::for_init(
                container_id,
                self.cfg.stdin.clone(),
                DaemonClient::new(&active_state.daemon_socket),
                active_state.daemon_vm_id.clone(),
            );
            input.start_input_only();
            *self.init_input.lock().unwrap_or_else(|e| e.into_inner()) = Some(input);
        }

        active_state.container_count.fetch_add(1, Ordering::SeqCst);

        info!(
            "TIMING start_container {} @{}: erofs={}ms acquire={}ms total={}ms from_snapshot={} additional={}",
            container_id,
            epoch_ms(),
            erofs_ms,
            acquire_ms,
            t_total.elapsed().as_millis(),
            from_snapshot,
            already_acquired
        );

        Ok(container_pid)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Copy a container's output from the guest to the stdout and stderr pipes containerd made for it, until the container has exited and
/// all its output has been written. The pipes are closed when this returns, which is how containerd (and the log file of the
/// kubelet) learns that there is no more.
///
/// The guest keeps the output only until it is taken (the offsets of each call acknowledge what came before), and keeps at most
/// 1 MiB for a stream nobody reads. Opening a pipe for writing waits for a reader, so it is done on a thread of its own. A
/// container with no pipes (an empty path) has nowhere to write, and nothing is read.
async fn pump_logs(
    cfg: InstanceConfig,
    daemon_socket: String,
    vm_id: String,
    container_id: String,
) {
    use tokio::io::AsyncWriteExt;

    // containerd's pipes are FIFOs, which the shim opens read-write: that does not wait for a reader, so opening cannot hang; but a
    // thread of the blocking pool is used for it, and it is given up on after a time. A write to a pipe nobody reads blocks once the
    // pipe is full (64 KiB): that is bounded too, and the stream is dropped.
    const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
    const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
    let open = |cfg: InstanceConfig, stderr: bool| async move {
        let job = tokio::task::spawn_blocking(move || {
            if stderr {
                cfg.open_stderr()
            } else {
                cfg.open_stdout()
            }
        });
        tokio::time::timeout(OPEN_TIMEOUT, job)
            .await
            .ok()
            .and_then(|r| r.ok())
            .and_then(|r| r.ok())
            .map(tokio::fs::File::from_std)
    };
    let mut out = open(cfg.clone(), false).await;
    let mut err = open(cfg, true).await;
    if out.is_none() && err.is_none() {
        return;
    }

    let client = DaemonClient::new(&daemon_socket);
    let (mut stdout_offset, mut stderr_offset) = (0u64, 0u64);
    let mut failures = 0u32;
    let mut wait = std::time::Duration::from_millis(20);
    loop {
        match client
            .read_logs(&vm_id, &container_id, stdout_offset, stderr_offset)
            .await
        {
            Ok(chunk) => {
                failures = 0;
                let got = !chunk.stdout.is_empty() || !chunk.stderr.is_empty();
                // A pipe whose reader has gone (a write fails) is dropped; the other stream goes on.
                if !chunk.stdout.is_empty() {
                    if let Some(f) = out.as_mut() {
                        let wrote = tokio::time::timeout(WRITE_TIMEOUT, async {
                            f.write_all(&chunk.stdout).await?;
                            f.flush().await
                        })
                        .await;
                        if !matches!(wrote, Ok(Ok(()))) {
                            out = None;
                        }
                    }
                }
                if !chunk.stderr.is_empty() {
                    if let Some(f) = err.as_mut() {
                        let wrote = tokio::time::timeout(WRITE_TIMEOUT, async {
                            f.write_all(&chunk.stderr).await?;
                            f.flush().await
                        })
                        .await;
                        if !matches!(wrote, Ok(Ok(()))) {
                            err = None;
                        }
                    }
                }
                stdout_offset = chunk.stdout_offset;
                stderr_offset = chunk.stderr_offset;
                if chunk.eof || (out.is_none() && err.is_none()) {
                    break;
                }
                if got {
                    wait = std::time::Duration::from_millis(20);
                } else {
                    // Nothing new: ask less often, up to 100 ms (a short wait keeps the end of the output from lagging behind the exit).
                    wait = (wait * 2).min(std::time::Duration::from_millis(100));
                    tokio::time::sleep(wait).await;
                }
            }
            Err(e) => {
                // The VM may be gone (the pod was deleted): a few tries, then stop.
                failures += 1;
                if failures >= 5 {
                    log::warn!("log pump for {container_id} stops: {e:#}");
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

struct SandboxSpec {
    netns: Option<String>,
    cgroups_path: Option<String>,
    limits: PodLimits,
}

/// Poll /proc/{pid} until the process exits.
async fn wait_for_pid_exit(pid: u32) {
    let proc_path = format!("/proc/{pid}");
    loop {
        if !std::path::Path::new(&proc_path).exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn parse_sandbox_spec(spec_path: &std::path::Path, docker: bool) -> SandboxSpec {
    let spec_str = std::fs::read_to_string(spec_path).unwrap_or_default();
    let spec: serde_json::Value = serde_json::from_str(&spec_str).unwrap_or_default();

    let netns = spec
        .pointer("/linux/namespaces")
        .and_then(|ns| ns.as_array())
        .and_then(|ns| {
            ns.iter().find_map(|n| {
                if n.get("type").and_then(|t| t.as_str()) == Some("network") {
                    n.get("path").and_then(|p| p.as_str()).map(String::from)
                } else {
                    None
                }
            })
        });

    let cgroups_path = spec
        .pointer("/linux/cgroupsPath")
        .and_then(|v| v.as_str())
        .map(String::from);

    SandboxSpec {
        netns,
        cgroups_path,
        // Kubernetes puts a pod's limits on the sandbox as annotations; Docker puts a container's on its own resources.
        limits: if docker {
            dockermode::limits_from_resources(&spec)
        } else {
            limits_from_spec(&spec)
        },
    }
}

/// The digest of the image the container was created from, if containerd's image store says one: the CRI image-name annotation,
/// resolved there. It only adds to the cache key (what is mounted decides the key), so no digest is not an error; a value that is
/// not a digest is dropped.
async fn image_digest_from_spec(bundle: &std::path::Path) -> Option<String> {
    let spec_str = std::fs::read_to_string(bundle.join("config.json")).ok()?;
    let spec: serde_json::Value = serde_json::from_str(&spec_str).ok()?;
    let image_name = spec
        .pointer("/annotations/io.kubernetes.cri.image-name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;

    let digest = tokio::time::timeout(Duration::from_secs(10), resolve_image_digest(image_name))
        .await
        .ok()
        .flatten()?;
    if !crate::cachekey::valid_digest(&digest) {
        info!("the digest of {image_name} is not a sha256 digest, left out of the cache key");
        return None;
    }
    info!("resolved image {} → {}", image_name, digest);
    Some(digest)
}

/// Query containerd's gRPC image service to resolve an image reference
/// to its content digest. Returns the digest (e.g. "sha256:b3255e7d...").
async fn resolve_image_digest(image_ref: &str) -> Option<String> {
    use containerd_client::services::v1::images_client::ImagesClient;
    use containerd_client::services::v1::GetImageRequest;
    use containerd_client::tonic::Request;
    use containerd_client::with_namespace;

    let channel = containerd_client::connect("/run/containerd/containerd.sock")
        .await
        .ok()?;

    let req = GetImageRequest {
        name: image_ref.to_string(),
    };
    let req = with_namespace!(req, "k8s.io");

    let image = ImagesClient::new(channel)
        .get(req)
        .await
        .ok()?
        .into_inner()
        .image?;

    image.target.map(|t| t.digest)
}

/// Stable FNV-1a 128-bit hash → hex string. (Not used for any key any more; the benchmark times it.)
#[allow(dead_code)]
pub fn stable_hash_hex(input: &str) -> String {
    let mut hash: u128 = 0x6c62272e07bb0142_62b821756295c58d;
    for byte in input.bytes() {
        hash ^= byte as u128;
        hash = hash.wrapping_mul(0x0000000001000000_000000000000013b);
    }
    format!("{hash:032x}")
}

/// A cgroups path as containerd writes it: the direct form `/k8s.io/<id>` (components of letters, digits and `_.-`; no `..`, no empty
/// component), or the systemd form `<slice>:<prefix>:<id>` (a `.slice` name, a prefix and an id of the same characters, no `/`).
pub fn valid_cgroups_path(p: &str) -> bool {
    let word = |s: &str| {
        !s.is_empty()
            && s.len() <= 255
            && s != "."
            && s != ".."
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    };
    if p.contains(':') {
        let parts: Vec<&str> = p.split(':').collect();
        return parts.len() == 3 && parts[0].ends_with(".slice") && parts.iter().all(|x| word(x));
    }
    let trimmed = p.strip_prefix('/').unwrap_or(p);
    !trimmed.is_empty() && trimmed.len() <= 1024 && trimmed.split('/').all(word)
}

/// Put the whole VM into the pod's cgroup: the process the daemon names (`ch_pid`, the launcher of an unprivileged Cloud Hypervisor) and
/// everything else in the VM's cgroup in the daemon's hierarchy (Cloud Hypervisor itself is the launcher's child and would otherwise stay
/// outside, with none of the pod's limits).
fn place_vm_in_pod_cgroup(ch_pid: u32, vm_cgroup: Option<&str>, cgroups_path: &str) -> Result<(), String> {
    place_in_pod_cgroup(ch_pid, cgroups_path)?;
    let Some(vm_cg) = vm_cgroup else { return Ok(()) };
    if !vm_cg.starts_with("/sys/fs/cgroup/") || vm_cg.contains("..") || vm_cg.contains('\n') {
        return Err(format!("the VM's cgroup {vm_cg:?} is not under /sys/fs/cgroup"));
    }
    // Where did the launcher land? (/proc/<pid>/cgroup says: `0::/kubepods/burstable/pod<uid>/<id>`)
    let line = std::fs::read_to_string(format!("/proc/{ch_pid}/cgroup")).map_err(|e| format!("read the cgroup of {ch_pid}: {e}"))?;
    let dest = dest_cgroup_of(&line).ok_or_else(|| format!("no cgroup in {line:?}"))?;
    let dest_procs = format!("/sys/fs/cgroup{dest}/cgroup.procs");
    for _ in 0..5 {
        let pids = std::fs::read_to_string(format!("{vm_cg}/cgroup.procs")).unwrap_or_default();
        let mut moved = 0;
        for pid in pids.lines().filter_map(|l| l.trim().parse::<u32>().ok()) {
            if std::fs::write(&dest_procs, pid.to_string()).is_ok() {
                moved += 1;
            }
        }
        if moved == 0 {
            return Ok(());
        }
    }
    Ok(())
}

/// The cgroup v2 path in the text of /proc/<pid>/cgroup (the `0::<path>` line).
fn dest_cgroup_of(proc_cgroup: &str) -> Option<String> {
    let line = proc_cgroup.lines().find(|l| l.starts_with("0::"))?;
    let path = line.trim_start_matches("0::").trim();
    if path.is_empty() || path == "/" || path.contains("..") {
        return None;
    }
    Some(path.to_string())
}

/// Take away the child cgroup `place_in_pod_cgroup` made for the VM (best effort: the Cloud Hypervisor process is gone by now, and a
/// cgroup that still has processes cannot be removed, which is fine). The kubelet could not remove the pod's cgroup while it was there.
fn remove_pod_cgroup_child(cgroups_path: &str) {
    if cgroups_path.contains(':') || !valid_cgroups_path(cgroups_path) {
        return;
    }
    let dir = format!("/sys/fs/cgroup/{}", cgroups_path.trim_start_matches('/'));
    let _ = std::fs::remove_dir(&dir);
}

/// Place a process in a pod cgroup. Supports cgroup v2 (unified) with both
/// direct paths (/k8s.io/<id>) and systemd-style paths
/// (kubepods-burstable-pod<uid>.slice:cri-containerd:<id>).
fn place_in_pod_cgroup(pid: u32, cgroups_path: &str) -> Result<(), String> {
    let cgroup_base = "/sys/fs/cgroup";

    // The value comes from the sandbox's OCI spec and is used here, as root, to make a directory and write to a file under the
    // cgroup file system: only the two forms the runtime writes are accepted, and nothing in them may lead out of the tree.
    if !valid_cgroups_path(cgroups_path) {
        return Err(format!("the cgroup path {cgroups_path:?} is not one the runtime writes"));
    }

    // Direct v2 path (e.g. /k8s.io/<id>)
    let full_path = format!("{cgroup_base}/{cgroups_path}/cgroup.procs");
    if std::path::Path::new(&full_path).exists() {
        std::fs::write(&full_path, pid.to_string()).map_err(|e| format!("{e}"))?;
        return Ok(());
    }

    // The cgroup of the container itself is made by the runtime that runs it (runc); a VM runtime has nothing that makes it. The pod's
    // cgroup (its parent) is the kubelet's and exists with the pod's limits, so the VM goes into a child of it, made here: the pod's
    // CPU and memory limits then apply to the Cloud Hypervisor process, as they would to a container. `remove_pod_cgroup_child` takes it away again.
    if !cgroups_path.contains(':') {
        let dir = format!("{cgroup_base}/{}", cgroups_path.trim_start_matches('/'));
        if let Some(parent) = std::path::Path::new(&dir).parent() {
            if parent.join("cgroup.procs").exists() {
                match std::fs::create_dir(&dir) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(format!("create cgroup {dir}: {e}")),
                }
                std::fs::write(format!("{dir}/cgroup.procs"), pid.to_string())
                    .map_err(|e| format!("write to {dir}/cgroup.procs: {e}"))?;
                return Ok(());
            }
        }
    }

    // Systemd cgroup v2: slice:scope_prefix:id
    // e.g. kubepods-burstable-pod<uid>.slice:cri-containerd:<id>
    // maps to: /sys/fs/cgroup/kubepods.slice/kubepods-burstable.slice/
    //          kubepods-burstable-pod<uid>.slice/cri-containerd-<id>.scope
    let parts: Vec<&str> = cgroups_path.split(':').collect();
    if parts.len() == 3 {
        let slice = parts[0]; // kubepods-burstable-pod<uid>.slice
        let prefix = parts[1]; // cri-containerd
        let id = parts[2]; // container id

        // Build the systemd cgroup hierarchy from the slice name
        // kubepods-burstable-pod<uid>.slice → kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod<uid>.slice
        let hierarchy = build_systemd_cgroup_path(slice);
        let scope = format!("{prefix}-{id}.scope");

        let path = format!("{cgroup_base}/{hierarchy}/{scope}");
        let procs_path = format!("{path}/cgroup.procs");

        // Create the cgroup scope if it doesn't exist. Containerd creates
        // scopes for standard runtimes, but our shim manages CH externally
        // so we need to create the scope ourselves.
        if !std::path::Path::new(&procs_path).exists() {
            std::fs::create_dir_all(&path).map_err(|e| format!("create cgroup {path}: {e}"))?;
        }

        std::fs::write(&procs_path, pid.to_string())
            .map_err(|e| format!("write to {procs_path}: {e}"))?;
        return Ok(());
    }

    Err(format!(
        "no cgroup found for path {cgroups_path} (tried v2 direct, systemd v2)"
    ))
}

/// Build the systemd cgroup hierarchy from a leaf slice name.
/// e.g. "kubepods-burstable-pod<uid>.slice"
///    → "kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod<uid>.slice"
fn build_systemd_cgroup_path(slice: &str) -> String {
    let name = slice.strip_suffix(".slice").unwrap_or(slice);
    let mut parts = Vec::new();
    let mut prefix = String::new();

    for segment in name.split('-') {
        if prefix.is_empty() {
            prefix = segment.to_string();
        } else {
            prefix = format!("{prefix}-{segment}");
        }
        parts.push(format!("{prefix}.slice"));
    }

    parts.join("/")
}

#[cfg(test)]
mod cgroup_path_tests {
    use super::valid_cgroups_path as ok;

    #[test]
    fn the_cgroup_of_a_process_is_read_from_its_cgroup_file() {
        use super::dest_cgroup_of as d;
        assert_eq!(d("0::/kubepods/burstable/pod1/abc\n").as_deref(), Some("/kubepods/burstable/pod1/abc"));
        assert_eq!(d("12:cpu:/x\n0::/a/b\n").as_deref(), Some("/a/b"));
        assert_eq!(d("0::/\n"), None);
        assert_eq!(d("0::/a/../b\n"), None);
        assert_eq!(d(""), None);
    }

    #[test]
    fn cgroups_paths_are_checked() {
        assert!(ok("/k8s.io/0123abcd"));
        assert!(ok("kubepods-burstable-podabc_1.slice:cri-containerd:0123abcd"));
        for bad in [
            "", "/", "/k8s.io/../..", "..", "/a//b", "/a/./b", "a b", "x.slice:cri:../x", "x.slice:cri",
            "x.slice:a:b:c", "notaslice:a:b", "/a/b\n",
        ] {
            assert!(!ok(bad), "{bad:?} must be refused");
        }
    }
}
