//! JSON-line API server for the sandbox daemon.
//!
//! The daemon listens on a Unix socket and serves AcquireSandbox,
//! ReleaseSandbox, container lifecycle proxies, and Status RPCs
//! using a newline-delimited JSON protocol.
//!
//! The daemon is root and this is its one door, so the door is narrow: the socket is root's (0600) and a connection from any other user
//! is closed; a request line, the number of connections and the sizes a request may ask for are limited; and every string of a request
//! that becomes a path, a name or a size is checked before anything is made (`validate.rs`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use log::{info, warn};
use tokio::sync::Mutex;

use crate::config::DaemonConfig;
use crate::pool::Pool;
use crate::shadow::SnapshotManager;
use crate::validate;

/// Monotonic counter for unique warm VM IDs.
static WARM_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The name of a VM restored from a warm snapshot. It carries the daemon's pid: the counter starts at 0 in every daemon, and a taken-over VM keeps
/// the name an earlier daemon gave it, so `warm-<key>-<seq>` alone was made again after a restart and was the name of a VM that a pod was using
/// (seen on 2026-10-21: "link memory-ranges from base snapshot: File exists", the warm snapshot dropped, and VMs that the next daemon could not take
/// over because their API socket was gone). The pool's and the sized VMs' names carry the pid too.
fn warm_vm_id(image_key: &str, pid: u32, seq: u64) -> String {
    format!("warm-{image_key}-{pid}-{seq}")
}

/// Per-VM agent connection, stored after first container start.
pub(crate) type AgentRegistry = Arc<Mutex<HashMap<String, cloudhv_proto::AgentServiceClient>>>;

/// The names of the TAP devices of the VMs that have one. A name is a hash of the VM's ID (see `tap_name_for`); two VMs whose hashes
/// collide must not both have it, or the second would delete the first's device. Taken when a VM's network is set up, given back when
/// the VM is released or the set-up fails.
static TAP_NAMES: std::sync::Mutex<Option<HashSet<String>>> = std::sync::Mutex::new(None);

fn reserve_tap_name(name: &str) -> bool {
    TAP_NAMES
        .lock()
        .unwrap()
        .get_or_insert_with(HashSet::new)
        .insert(name.to_string())
}

fn release_tap_name_of(vm_id: &str) {
    if let Some(set) = TAP_NAMES.lock().unwrap().as_mut() {
        set.remove(&tap_name_for(vm_id));
    }
}

/// What the daemon keeps for a VM besides the VM itself: its agent connection and its TAP name.
pub(crate) async fn forget_vm(agents: &AgentRegistry, vm_id: &str) {
    crate::liveness::forget(vm_id);
    agents.lock().await.remove(vm_id);
    release_tap_name_of(vm_id);
}

/// Stop a VM that was started and is not (or no longer) in the pool's hands: its process, user, directory and the daemon's records.
async fn abort_vm(
    agents: &AgentRegistry,
    vm_id: &str,
    api_socket: &std::path::Path,
    ch_pid: u32,
    state_dir: &std::path::Path,
) {
    forget_vm(agents, vm_id).await;
    crate::vm_lifecycle::shutdown_and_destroy(api_socket, ch_pid, state_dir).await;
}

/// How long a connection may sit without sending a request.
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The daemon API server. Listens on a Unix socket and serves
/// AcquireSandbox, ReleaseSandbox, and Status RPCs.
pub struct ApiServer {
    config: DaemonConfig,
    pool: Arc<Pool>,
    snapshots: Arc<SnapshotManager>,
    agents: AgentRegistry,
}

impl ApiServer {
    pub fn new(config: DaemonConfig, pool: Arc<Pool>, snapshots: Arc<SnapshotManager>) -> Self {
        Self {
            config,
            pool,
            snapshots,
            agents: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Connect again to the agents of the VMs taken over from the earlier daemon (`Pool::adopt`): the connection lived in the earlier
    /// daemon's memory, and every call about a container goes through it. A VM whose agent does not answer is of no use to its pod (the
    /// shim finds out when it asks): it is stopped, as a VM that died would be.
    pub async fn adopt_agents(&self, vms: &[crate::pool::PoolVm]) {
        for vm in vms {
            let connected = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                crate::vm_lifecycle::connect_agent(&vm.vsock_socket),
            )
            .await;
            match connected {
                Ok(Ok((agent, health))) => {
                    crate::liveness::register(&vm.vm_id, health);
                    self.agents.lock().await.insert(vm.vm_id.clone(), agent);
                    // The TAP of the VM (if it has one) is in its pod's network namespace and keeps its name.
                    reserve_tap_name(&tap_name_for(&vm.vm_id));
                    info!(
                        "took over VM {} (cid {}, pid {})",
                        vm.vm_id, vm.cid, vm.ch_pid
                    );
                }
                other => {
                    let why = match other {
                        Ok(Err(e)) => format!("{e:#}"),
                        _ => "no answer in 10 s".to_string(),
                    };
                    warn!(
                        "VM {} is not taken over, its agent does not answer ({why}): stopping it",
                        vm.vm_id
                    );
                    let _ = self.pool.release(&vm.vm_id).await;
                }
            }
        }
    }

    /// Watch the VMs handed to pods (see `liveness.rs`): a VM whose Cloud Hypervisor is gone is released, and one whose guest does not answer
    /// the agent's `Check` for long enough has its Cloud Hypervisor killed.
    pub fn spawn_liveness(&self) {
        let c = &self.config;
        crate::liveness::spawn(
            Arc::clone(&self.pool),
            self.agents.clone(),
            crate::liveness::Settings {
                process_every: std::time::Duration::from_secs(c.process_check_secs.max(1)),
                check_every: std::time::Duration::from_secs(c.guest_check_secs),
                check_timeout: std::time::Duration::from_secs(c.guest_check_timeout_secs.max(1)),
                failures: c.guest_check_failures.max(1),
                action: c.guest_check_action,
            },
        );
    }

    /// Release the VMs taken over from the earlier daemon that no pod's shim has spoken about for `adopted_claim_secs`: their pods are gone
    /// (deleted while the daemon was away), and nothing else would ever release them.
    pub fn spawn_unclaimed_reaper(&self) {
        let pool = Arc::clone(&self.pool);
        let agents = self.agents.clone();
        let after = std::time::Duration::from_secs(self.config.adopted_claim_secs.max(1));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                for id in pool.take_unclaimed(after) {
                    warn!("VM {id} was taken over and no pod has asked about it for {}s: releasing it", after.as_secs());
                    forget_vm(&agents, &id).await;
                    let _ = pool.release(&id).await;
                }
            }
        });
    }

    /// Start the ttrpc server. Blocks until shutdown.
    pub async fn serve(&self) -> Result<()> {
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        let socket_path = &self.config.socket_path;

        // Remove a stale socket (and nothing else that happens to be at the path)
        match std::fs::symlink_metadata(socket_path) {
            Ok(m) if m.file_type().is_socket() => {
                let _ = std::fs::remove_file(socket_path);
            }
            Ok(_) => anyhow::bail!("{socket_path} exists and is not a socket"),
            Err(_) => {}
        }

        // Ensure parent directory exists. It is not made 0700: the state directory is usually below it, and a VM's user has to pass
        // through to its own directory. The socket itself is root's.
        if let Some(parent) = std::path::Path::new(socket_path).parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Bound with a umask that leaves no gap in which another user could connect, then fixed to 0600.
        let listener = {
            let old = unsafe { libc::umask(0o177) };
            let bound = tokio::net::UnixListener::bind(socket_path);
            unsafe { libc::umask(old) };
            bound?
        };
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
        info!("daemon API listening on {}", socket_path);

        let limit = Arc::new(tokio::sync::Semaphore::new(
            self.config.max_connections.max(1),
        ));
        loop {
            let (stream, _) = listener.accept().await?;
            // Only root: the shim runs as root. Whoever else could reach the socket (the mode is 0600, so this is a second wall) is not served.
            match stream.peer_cred() {
                Ok(cred) if cred.uid() == 0 => {}
                Ok(cred) => {
                    warn!("closing a connection from user {}", cred.uid());
                    continue;
                }
                Err(e) => {
                    warn!("closing a connection whose user is unknown: {e}");
                    continue;
                }
            }
            let Ok(permit) = Arc::clone(&limit).try_acquire_owned() else {
                warn!(
                    "closing a connection: {} are open",
                    self.config.max_connections
                );
                continue;
            };
            let pool = Arc::clone(&self.pool);
            let snapshots = Arc::clone(&self.snapshots);
            let config = self.config.clone();
            let agents = Arc::clone(&self.agents);
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, &pool, &snapshots, &config, &agents).await
                {
                    log::warn!("client error: {:#}", e);
                }
                drop(permit);
            });
        }
    }
}

/// Read one request line of at most `max` bytes (without reading on past the limit). Ok(false) at the end of the stream.
async fn read_request_line<R>(reader: &mut R, max: usize, buf: &mut Vec<u8>) -> Result<bool>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    buf.clear();
    let mut limited = (&mut *reader).take(max as u64 + 1);
    let n = limited.read_until(b'\n', buf).await?;
    if n == 0 {
        return Ok(false);
    }
    if buf.len() > max || (buf.len() == max && buf.last() != Some(&b'\n')) {
        anyhow::bail!("a request is longer than {max} bytes");
    }
    Ok(true)
}

/// Handle a single client connection (JSON-line protocol).
async fn handle_connection(
    stream: tokio::net::UnixStream,
    pool: &Arc<Pool>,
    snapshots: &Arc<SnapshotManager>,
    config: &DaemonConfig,
    agents: &AgentRegistry,
) -> Result<()> {
    use tokio::io::{AsyncWriteExt, BufReader};

    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = Vec::new();

    loop {
        let more = tokio::time::timeout(
            IDLE_TIMEOUT,
            read_request_line(&mut buf_reader, config.max_request_bytes, &mut line),
        )
        .await
        .map_err(|_| anyhow::anyhow!("no request for {}s", IDLE_TIMEOUT.as_secs()))??;
        if !more {
            break;
        }
        let request: serde_json::Value = serde_json::from_slice(&line)?;
        let method = request.get("method").and_then(|v| v.as_str()).unwrap_or("");
        // A request about a VM shows that it has a pod (see `Pool::claim`).
        if let Some(id) = request.get("vm_id").and_then(|v| v.as_str()) {
            pool.claim(id);
        }

        log::debug!(
            "request: method={} vm_id={} container_id={}",
            method,
            request.get("vm_id").and_then(|v| v.as_str()).unwrap_or("-"),
            request
                .get("container_id")
                .and_then(|v| v.as_str())
                .unwrap_or("-")
        );

        let response = match method {
            "AcquireSandbox" => handle_acquire(pool, snapshots, config, agents, &request).await,
            "ReleaseSandbox" => handle_release(pool, agents, &request).await,
            "AddContainer" => handle_add_container(pool, config, agents, &request).await,
            "KillContainer" => handle_kill_container(agents, &request).await,
            "WaitContainer" => handle_wait_container(agents, &request).await,
            "ReadLogs" => handle_read_logs(agents, &request).await,
            m if crate::exec::is_exec_method(m) => {
                crate::exec::handle(m, pool, agents, &request).await
            }
            "DeleteContainer" => handle_delete_container(agents, &request).await,
            "Status" => handle_status(pool, snapshots).await,
            _ => serde_json::json!({"error": format!("unknown method: {}", method)}),
        };

        let mut resp_str = serde_json::to_string(&response)?;
        resp_str.push('\n');
        writer.write_all(resp_str.as_bytes()).await?;
        writer.flush().await?;
        // Only the size: an answer can hold a container's output (ReadLogs), which is the customer's and may hold secrets.
        log::info!("responded: {} ({} bytes)", method, resp_str.len());
    }

    Ok(())
}

fn field<'a>(request: &'a serde_json::Value, key: &str) -> &'a str {
    request.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

fn error(what: &str, e: impl std::fmt::Display) -> serde_json::Value {
    serde_json::json!({"error": format!("{what}: {e}")})
}

/// An identifier field of a request that must be there and must be plain.
pub(crate) fn required_id<'a>(
    request: &'a serde_json::Value,
    key: &str,
) -> Result<&'a str, serde_json::Value> {
    let v = field(request, key);
    validate::check_id(key, v).map_err(|e| error("bad request", e))?;
    Ok(v)
}

async fn handle_acquire(
    pool: &Arc<Pool>,
    snapshots: &Arc<SnapshotManager>,
    config: &DaemonConfig,
    agents: &AgentRegistry,
    request: &serde_json::Value,
) -> serde_json::Value {
    if pool.is_stopping() {
        return serde_json::json!({"error": "the daemon is stopping"});
    }
    let netns = field(request, "netns");
    let erofs_path = field(request, "erofs_path");
    let container_id = field(request, "container_id");
    let config_json_b64 = field(request, "config_json");
    if let Err(e) = validate::check_cgroup_delegate(config, config_json_b64) {
        return error("bad request (cgroup delegate)", format!("{e:#}"));
    }

    // Everything that becomes a path, a name or a size is checked before anything is made.
    if !netns.is_empty() {
        if let Err(e) = crate::netns::check_netns(netns) {
            return error("bad request (netns)", format!("{e:#}"));
        }
    }
    if !container_id.is_empty() {
        if let Err(e) = validate::check_id("container_id", container_id) {
            return error("bad request", e);
        }
    }
    if !erofs_path.is_empty() {
        if let Err(e) = validate::check_erofs_path(config, erofs_path) {
            return error("bad request (erofs_path)", format!("{e:#}"));
        }
    }
    let want_memory = request
        .get("memory_mb")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let want_vcpus = request.get("vcpus").and_then(|v| v.as_u64()).unwrap_or(0);
    if let Err(e) = validate::check_size(config, want_memory, want_vcpus) {
        return error("bad request (size)", e);
    }
    let want_vcpus = want_vcpus as u32; // checked above: at most max_vcpus
    if pool.active_count() >= config.max_active_vms {
        return serde_json::json!({"error": format!("{} VMs are active, the most max_active_vms allows", pool.active_count())});
    }
    let volumes = volumes_from_request(request);
    let disks = match crate::emptydir::parse(request) {
        Ok(d) => d,
        Err(e) => return error("bad request (volumes)", e),
    };
    let blocks = match crate::blockvol::parse(request) {
        Ok(b) => b,
        Err(e) => return error("bad request (volumes)", e),
    };
    let terminal = terminal_from_request(request);

    // A pod whose limits ask for a VM of another size than the pool's (and the warm snapshots') cannot be served from them: their
    // memory and CPUs are those of the snapshot. Such a pod gets a VM of its own size, booted for it.
    if is_other_size(config, want_memory, want_vcpus) {
        return acquire_sized(
            pool,
            config,
            agents,
            &RunContainerParams {
                api_socket: std::path::Path::new(""), // filled in once the VM exists
                vsock_socket: std::path::Path::new(""),
                vm_id: "",
                netns,
                erofs_path,
                container_id,
                config_json_b64,
                volumes: &volumes,
                disks: &disks,
                blocks: &blocks,
                terminal,
            },
            want_memory,
            want_vcpus,
        )
        .await;
    }

    // The key of the warm snapshot is made here, from the content of the image (the one the caller sends is not used: with it, a pod could be
    // given the warm guest of another pod's image). Only for a pod that can be served from the pool or a warm snapshot: a pod of another size
    // (above) does not use it, and hashing a PostgreSQL image (451 MB) was about 2 s of its start (measured 2026-10-26).
    let image_key = if erofs_path.is_empty() {
        String::new()
    } else {
        match crate::imagekey::content_key(erofs_path).await {
            Ok(k) => k,
            Err(e) => return error("image", format!("{e:#}")),
        }
    };
    if let Some(sent) = request.get("image_key").and_then(|v| v.as_str()) {
        log::debug!("image_key sent {sent:?} not used; the image's own is {image_key}");
    }

    // Check for warm workload snapshot
    if !image_key.is_empty() && snapshots.has_snapshot(&image_key).await {
        let seq = WARM_COUNTER.fetch_add(1, Ordering::Relaxed);
        let vm_id = warm_vm_id(&image_key, std::process::id(), seq);
        let state_dir = std::path::PathBuf::from(&config.state_dir).join(&vm_id);
        // The directory must be new: a name that is already there is a VM of this or of an earlier daemon (a taken-over VM keeps its
        // name), and restoring into its directory would damage it. (`create_dir`, not `create_dir_all`, which accepts one that exists.)
        if let Err(e) = std::fs::create_dir(&state_dir) {
            return serde_json::json!({"error": format!("create state dir: {e}")});
        }

        let cid = pool.next_cid_pub();
        match snapshots
            .restore_from_snapshot(&image_key, cid, &state_dir, erofs_path)
            .await
        {
            Ok((ch_pid, api_socket, vsock_socket)) => {
                info!("warm restore for image_key={}", image_key);
                // From here the VM exists. Whatever fails, it is stopped and everything of it removed.
                match finish_warm_restore(
                    pool,
                    agents,
                    &vm_id,
                    cid,
                    ch_pid,
                    &api_socket,
                    &vsock_socket,
                    netns,
                    container_id,
                    config_json_b64,
                    &volumes,
                    &disks,
                    &blocks,
                    terminal,
                )
                .await
                {
                    Ok(json) => return json,
                    Err(e) => {
                        warn!("warm restore of {vm_id}: {e:#}");
                        abort_vm(agents, &vm_id, &api_socket, ch_pid, &state_dir).await;
                        return error("warm restore", format!("{e:#}"));
                    }
                }
            }
            Err(e) => {
                warn!("warm restore failed, falling back to pool: {:#}", e);
                let _ = std::fs::remove_dir_all(&state_dir);
                // Fall through to pool acquire
            }
        }
    }

    match pool.acquire().await {
        Ok(vm) => {
            // Trigger async pool replenishment
            let pool_clone = Arc::clone(pool);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                pool_clone.replenish_one().await;
            });

            // Trigger shadow VM for this image (if not already snapshotted)
            if !image_key.is_empty() {
                snapshots.trigger_shadow(&image_key, erofs_path).await;
            }

            // Hot-plug rootfs and run container inside the VM
            let (container_pid, tap_name_out) =
                if !erofs_path.is_empty() && !container_id.is_empty() {
                    match run_container_in_vm(
                        &RunContainerParams {
                            api_socket: &vm.api_socket,
                            vsock_socket: &vm.vsock_socket,
                            vm_id: &vm.vm_id,
                            netns,
                            erofs_path,
                            container_id,
                            config_json_b64,
                            volumes: &volumes,
                            disks: &disks,
                            blocks: &blocks,
                            terminal,
                        },
                        agents,
                    )
                    .await
                    {
                        Ok((pid, tap)) => (pid, tap),
                        Err(e) => {
                            // The VM was handed out and the pod does not start: it is not left active.
                            forget_vm(agents, &vm.vm_id).await;
                            let _ = pool.release(&vm.vm_id).await;
                            return serde_json::json!({"error": format!("run container: {e:#}")});
                        }
                    }
                } else {
                    (0, String::new())
                };

            serde_json::json!({
                "vm_id": vm.vm_id,
                "api_socket": vm.api_socket.to_string_lossy(),
                "vsock_socket": vm.vsock_socket.to_string_lossy(),
                "cid": vm.cid,
                "ch_pid": vm.ch_pid,
                "ch_cgroup": cgroup_of(&vm.vm_id),
                "from_snapshot": false,
                "container_pid": container_pid,
                "tap_name": tap_name_out,
            })
        }
        Err(e) => {
            serde_json::json!({"error": format!("acquire failed: {:#}", e)})
        }
    }
}

/// The rest of a warm restore, once the VM exists: connect to its agent, set up its network, run the pod's container and register the VM
/// as active. On an error the caller stops the VM (`abort_vm`).
#[allow(clippy::too_many_arguments)]
async fn finish_warm_restore(
    pool: &Arc<Pool>,
    agents: &AgentRegistry,
    vm_id: &str,
    cid: u64,
    ch_pid: u32,
    api_socket: &std::path::Path,
    vsock_socket: &std::path::Path,
    netns: &str,
    container_id: &str,
    config_json_b64: &str,
    volumes: &[cloudhv_proto::VolumeMount],
    disks: &[crate::emptydir::DiskRequest],
    blocks: &[crate::blockvol::BlockRequest],
    terminal: bool,
) -> Result<serde_json::Value> {
    // Connect to the guest agent of the restored VM. It is needed for the network configuration
    // and, always, to hand the warm container over to the pod's container ID (below).
    let (agent, health) = crate::vm_lifecycle::connect_agent(vsock_socket)
        .await
        .context("agent connect")?;
    crate::liveness::register(vm_id, health);
    agents.lock().await.insert(vm_id.to_string(), agent.clone());

    // Set up networking: TAP in host → CH add-net → move to pod netns + TC
    let tap_name_out = if !netns.is_empty() {
        let (tap, net_info) = setup_vm_networking(netns, vm_id, api_socket)
            .await
            .context("networking")?;
        if let Err(e) = configure_guest_network(&agent, &net_info.ip_cidr, &net_info.gateway).await
        {
            warn!("configure_network on warm restore: {e:#}");
        }
        tap
    } else {
        String::new()
    };

    // The snapshot holds a VM whose runtime is warm: the guest has booted, the image's erofs rootfs is
    // attached (/dev/vdb) and its pages are cached. The container the shadow VM ran was only there to
    // warm that up (an empty OCI config, which exits at once). The pod's own container still has to be
    // run, with its own OCI config and under the ID the pod's runtime uses; without this the pod's
    // runtime asks the agent about an ID it does not know ("unknown container", exit 137), or, if the
    // shadow's container is simply handed over, gets the exit status of a container that ended long ago.
    crate::emptydir::attach(api_socket, disks)
        .await
        .context("emptyDir disks")?;
    crate::blockvol::attach(api_socket, blocks)
        .await
        .context("block volumes")?;
    let container_pid =
        run_container_in_warm_vm(&agent, container_id, config_json_b64, volumes, terminal)
            .await
            .context("run container")?;

    // Register as active so release() works correctly
    pool.register_active(crate::pool::PoolVm {
        vm_id: vm_id.to_string(),
        api_socket: api_socket.to_path_buf(),
        vsock_socket: vsock_socket.to_path_buf(),
        cid,
        ch_pid,
        created_at: std::time::Instant::now(),
    })
    .await;

    Ok(serde_json::json!({
        "vm_id": vm_id,
        "api_socket": api_socket.to_string_lossy(),
        "vsock_socket": vsock_socket.to_string_lossy(),
        "cid": cid,
        "ch_pid": ch_pid,
        "ch_cgroup": cgroup_of(vm_id),
        "from_snapshot": true,
        "container_pid": container_pid,
        "tap_name": tap_name_out,
    }))
}

/// Whether a requested VM size differs from the pool's (0 means "the default").
fn is_other_size(config: &DaemonConfig, memory_mb: u64, vcpus: u32) -> bool {
    (memory_mb != 0 && memory_mb != config.default_memory_mb)
        || (vcpus != 0 && vcpus != config.default_vcpus)
}

static SIZED_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Boot a VM of the requested size, hot-plug the pod's rootfs and run its container. The cost is a normal boot (a few hundred
/// milliseconds) instead of a restore from the pool (some tens), which a pod that runs for hours does not mind. A failure
/// takes the VM down again.
async fn acquire_sized(
    pool: &Arc<Pool>,
    config: &DaemonConfig,
    agents: &AgentRegistry,
    params: &RunContainerParams<'_>,
    memory_mb: u64,
    vcpus: u32,
) -> serde_json::Value {
    let memory_mb = if memory_mb == 0 {
        config.default_memory_mb
    } else {
        memory_mb
    };
    let vcpus = if vcpus == 0 {
        config.default_vcpus
    } else {
        vcpus
    };
    let seq = SIZED_COUNTER.fetch_add(1, Ordering::Relaxed);
    let vm_id = format!("sized-{}-{}", std::process::id(), seq);
    let state_dir = std::path::PathBuf::from(&config.state_dir).join(&vm_id);
    if let Err(e) = std::fs::create_dir_all(&state_dir) {
        return serde_json::json!({"error": format!("create state dir: {e}")});
    }
    let api_socket = state_dir.join("api.sock");
    let vsock_socket = state_dir.join("vsock.sock");
    let cid = pool.next_cid_pub();
    let t0 = std::time::Instant::now();

    // A failed boot stops the VM and removes its directory itself.
    let ch_pid = match crate::vm_lifecycle::boot_vm_sized(
        config,
        &vm_id,
        &state_dir,
        cid,
        None,
        None,
        &[],
        memory_mb,
        vcpus,
        false,
    )
    .await
    {
        Ok(pid) => pid,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&state_dir);
            return serde_json::json!({"error": format!("boot a VM of {memory_mb} MiB: {e:#}")});
        }
    };
    if let Err(e) = crate::vm_lifecycle::wait_for_agent(&vsock_socket).await {
        abort_vm(agents, &vm_id, &api_socket, ch_pid, &state_dir).await;
        return serde_json::json!({"error": format!("agent of the new VM: {e:#}")});
    }
    pool.register_active(crate::pool::PoolVm {
        vm_id: vm_id.clone(),
        api_socket: api_socket.clone(),
        vsock_socket: vsock_socket.clone(),
        cid,
        ch_pid,
        created_at: std::time::Instant::now(),
    })
    .await;

    let run = RunContainerParams {
        api_socket: &api_socket,
        vsock_socket: &vsock_socket,
        vm_id: &vm_id,
        ..*params
    };
    let (container_pid, tap_name) = match run_container_in_vm(&run, agents).await {
        Ok(v) => v,
        Err(e) => {
            forget_vm(agents, &vm_id).await;
            let _ = pool.release(&vm_id).await;
            return serde_json::json!({"error": format!("run container: {e:#}")});
        }
    };
    info!(
        "sized VM {} ({} MiB, {} vCPUs) ready in {}ms",
        vm_id,
        memory_mb,
        vcpus,
        t0.elapsed().as_millis()
    );
    serde_json::json!({
        "vm_id": vm_id,
        "api_socket": api_socket.to_string_lossy(),
        "vsock_socket": vsock_socket.to_string_lossy(),
        "cid": cid,
        "ch_pid": ch_pid,
        "ch_cgroup": cgroup_of(&vm_id),
        "from_snapshot": false,
        "container_pid": container_pid,
        "tap_name": tap_name,
    })
}

/// Parameters for running a container inside a VM.
#[derive(Clone, Copy)]
struct RunContainerParams<'a> {
    api_socket: &'a std::path::Path,
    vsock_socket: &'a std::path::Path,
    vm_id: &'a str,
    netns: &'a str,
    erofs_path: &'a str,
    container_id: &'a str,
    config_json_b64: &'a str,
    /// What the pod's runtime bind-mounts into the container; the agent writes it into the VM.
    volumes: &'a [cloudhv_proto::VolumeMount],
    /// The disks (emptyDirs) the volumes name: made and hot-plugged before the container runs.
    disks: &'a [crate::emptydir::DiskRequest],
    /// The persistent block volumes the volumes name: locked, given a node and hot-plugged before the container runs.
    blocks: &'a [crate::blockvol::BlockRequest],
    /// Whether the container's process has a terminal (`tty: true`).
    terminal: bool,
}

/// The first characters of a container ID, for the name of its disk (the ID is a plain identifier here, but never slice by bytes).
fn disk_id_for(container_id: &str) -> String {
    format!("ctr-{}", container_id.chars().take(12).collect::<String>())
}

/// Hot-plug an erofs rootfs and run a container via the guest agent.
/// If `netns` is non-empty, sets up TAP networking first.
async fn run_container_in_vm(
    params: &RunContainerParams<'_>,
    agents: &AgentRegistry,
) -> anyhow::Result<(u32, String)> {
    use crate::vm_lifecycle;

    // Set up networking (TAP in host → CH add-net → move to pod netns + TC)
    let (tap_name, net_info) = if !params.netns.is_empty() {
        let (tap, info) = setup_vm_networking(params.netns, params.vm_id, params.api_socket)
            .await
            .context("networking setup")?;
        (tap, Some(info))
    } else {
        (String::new(), None)
    };

    // Hot-plug rootfs disk
    let disk_id = disk_id_for(params.container_id);
    let disk_json = serde_json::json!({
        "path": params.erofs_path,
        "readonly": true,
        "image_type": "Raw",
        "id": disk_id,
    });
    if let Some(state_dir) = params.api_socket.parent() {
        crate::rootless::allow_read(std::path::Path::new(params.erofs_path), state_dir)
            .context("let the VM read the rootfs")?;
    }
    vm_lifecycle::api_request_with_body(
        params.api_socket,
        "PUT",
        "/api/v1/vm.add-disk",
        &disk_json.to_string(),
    )
    .await
    .context("hot-plug rootfs")?;
    crate::emptydir::attach(params.api_socket, params.disks)
        .await
        .context("emptyDir disks")?;
    crate::blockvol::attach(params.api_socket, params.blocks)
        .await
        .context("block volumes")?;

    // Connect to agent (or reuse existing connection)
    let agent = {
        let existing = agents.lock().await.get(params.vm_id).cloned();
        if let Some(a) = existing {
            a
        } else {
            let (a, _health) = vm_lifecycle::connect_agent(params.vsock_socket)
                .await
                .context("agent connect for container")?;
            agents
                .lock()
                .await
                .insert(params.vm_id.to_string(), a.clone());
            a
        }
    };

    // Configure guest networking
    if let Some(info) = &net_info {
        configure_guest_network(&agent, &info.ip_cidr, &info.gateway)
            .await
            .context("configure guest network")?;
    }

    // Decode OCI config
    use base64::Engine;
    let config_json = if params.config_json_b64.is_empty() {
        b"{}".to_vec()
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(params.config_json_b64)
            .unwrap_or_else(|_| b"{}".to_vec())
    };

    // Run container
    let mut req = cloudhv_proto::CreateContainerRequest::new();
    req.container_id = params.container_id.to_string();
    req.config_json = config_json;
    req.volumes = params.volumes.to_vec();
    req.terminal = params.terminal;
    req.rootfs_preattached = false;
    req.erofs_layers = 1;

    let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(30));
    let resp = agent
        .run_container(ctx, &req)
        .await
        .context("RunContainer")?;

    info!(
        "container {} started in VM (pid={})",
        params.container_id, resp.pid
    );
    Ok((resp.pid, tap_name))
}

async fn handle_add_container(
    pool: &Arc<Pool>,
    config: &DaemonConfig,
    agents: &AgentRegistry,
    request: &serde_json::Value,
) -> serde_json::Value {
    let vm_id = match required_id(request, "vm_id") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let container_id = match required_id(request, "container_id") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let erofs_path = field(request, "erofs_path");
    if let Err(e) = validate::check_erofs_path(config, erofs_path) {
        return error("bad request (erofs_path)", format!("{e:#}"));
    }
    let config_json_b64 = field(request, "config_json");
    if let Err(e) = validate::check_cgroup_delegate(config, config_json_b64) {
        return error("bad request (cgroup delegate)", format!("{e:#}"));
    }
    let volumes = volumes_from_request(request);
    let disks = match crate::emptydir::parse(request) {
        Ok(d) => d,
        Err(e) => return error("bad request (volumes)", e),
    };
    let blocks = match crate::blockvol::parse(request) {
        Ok(b) => b,
        Err(e) => return error("bad request (volumes)", e),
    };
    let terminal = terminal_from_request(request);

    let vm = match pool.get_active(vm_id).await {
        Some(vm) => vm,
        None => {
            return serde_json::json!({"error": format!("VM {} not found in active set", vm_id)});
        }
    };

    match run_container_in_vm(
        &RunContainerParams {
            api_socket: &vm.api_socket,
            vsock_socket: &vm.vsock_socket,
            vm_id,
            netns: "",
            erofs_path,
            container_id,
            config_json_b64,
            volumes: &volumes,
            disks: &disks,
            blocks: &blocks,
            terminal,
        },
        agents,
    )
    .await
    {
        Ok((pid, _)) => serde_json::json!({"container_pid": pid}),
        Err(e) => serde_json::json!({"error": format!("add container: {e:#}")}),
    }
}

async fn handle_release(
    pool: &Arc<Pool>,
    agents: &AgentRegistry,
    request: &serde_json::Value,
) -> serde_json::Value {
    let vm_id = match required_id(request, "vm_id") {
        Ok(v) => v,
        Err(e) => return e,
    };

    // Remove agent connection for this VM
    forget_vm(agents, vm_id).await;

    // Only a VM that was handed out is destroyed; any other name is answered with success and nothing is touched (a release is
    // idempotent: the shim may repeat it, or ask after the daemon restarted).
    match pool.release(vm_id).await {
        Ok(_) => serde_json::json!({"ok": true}),
        Err(e) => serde_json::json!({"error": format!("release failed: {:#}", e)}),
    }
}

async fn handle_kill_container(
    agents: &AgentRegistry,
    request: &serde_json::Value,
) -> serde_json::Value {
    let vm_id = match required_id(request, "vm_id") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let container_id = match required_id(request, "container_id") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let signal = request.get("signal").and_then(|v| v.as_u64()).unwrap_or(15);
    if signal == 0 || signal > 64 {
        return serde_json::json!({"error": "bad request: signal"});
    }

    let agent = match agents.lock().await.get(vm_id).cloned() {
        Some(a) => a,
        None => return serde_json::json!({"error": format!("no agent for VM {vm_id}")}),
    };

    let mut req = cloudhv_proto::KillContainerRequest::new();
    req.container_id = container_id.to_string();
    req.signal = signal as u32;
    let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(5));

    match agent.kill_container(ctx, &req).await {
        Ok(_) => serde_json::json!({"ok": true}),
        Err(e) => serde_json::json!({"error": format!("kill_container: {e}")}),
    }
}

async fn handle_wait_container(
    agents: &AgentRegistry,
    request: &serde_json::Value,
) -> serde_json::Value {
    let vm_id = match required_id(request, "vm_id") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let container_id = match required_id(request, "container_id") {
        Ok(v) => v,
        Err(e) => return e,
    };

    let agent = match agents.lock().await.get(vm_id).cloned() {
        Some(a) => a,
        None => return serde_json::json!({"error": format!("no agent for VM {vm_id}")}),
    };

    let mut req = cloudhv_proto::WaitContainerRequest::new();
    req.container_id = container_id.to_string();
    let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(86400));

    match agent.wait_container(ctx, &req).await {
        Ok(resp) => serde_json::json!({
            "exit_status": resp.exit_status,
            "exited_at": resp.exited_at,
        }),
        Err(e) => serde_json::json!({
            "exit_status": 137,
            "exited_at": "",
            "error": format!("wait_container: {e}"),
        }),
    }
}

/// The most log bytes one ReadLogs call returns (the ttrpc message limit is 4 MiB).
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// Carry a container's new output from the guest agent to the shim (which writes it to the pipes containerd gave it). The offsets are
/// the caller's: what is before them has been taken and the agent drops it. The bytes go as base64 in the JSON line.
async fn handle_read_logs(
    agents: &AgentRegistry,
    request: &serde_json::Value,
) -> serde_json::Value {
    use base64::Engine;
    let vm_id = match required_id(request, "vm_id") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let container_id = match required_id(request, "container_id") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let agent = match agents.lock().await.get(vm_id).cloned() {
        Some(a) => a,
        None => return serde_json::json!({"error": format!("no agent for VM {vm_id}")}),
    };

    let mut req = cloudhv_proto::ReadLogsRequest::new();
    req.container_id = container_id.to_string();
    req.stdout_offset = request
        .get("stdout_offset")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    req.stderr_offset = request
        .get("stderr_offset")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    req.max_bytes = request
        .get("max_bytes")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .min(MAX_LOG_BYTES) as u32;
    let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(10));

    match agent.read_logs(ctx, &req).await {
        Ok(resp) => {
            let b64 = base64::engine::general_purpose::STANDARD;
            serde_json::json!({
                "stdout": b64.encode(&resp.stdout),
                "stderr": b64.encode(&resp.stderr),
                "stdout_offset": resp.stdout_offset,
                "stderr_offset": resp.stderr_offset,
                "eof": resp.eof,
                "stdout_dropped": resp.stdout_dropped,
                "stderr_dropped": resp.stderr_dropped,
            })
        }
        Err(e) => serde_json::json!({"error": format!("read_logs: {e}")}),
    }
}

async fn handle_delete_container(
    agents: &AgentRegistry,
    request: &serde_json::Value,
) -> serde_json::Value {
    let vm_id = match required_id(request, "vm_id") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let container_id = match required_id(request, "container_id") {
        Ok(v) => v,
        Err(e) => return e,
    };

    let agent = match agents.lock().await.get(vm_id).cloned() {
        Some(a) => a,
        None => return serde_json::json!({"ok": true}), // VM already gone, nothing to delete
    };

    let mut req = cloudhv_proto::DeleteContainerRequest::new();
    req.container_id = container_id.to_string();
    let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(5));
    let _ = agent.delete_container(ctx, &req).await;

    serde_json::json!({"ok": true})
}

/// The name of a VM's TAP device. A Linux interface name has 15 characters at most, so the name cannot hold the VM's ID; it holds a hash of it.
/// The name used to be the first 8 characters of the ID, which for the pool's VMs ("pool-<pid>-<n>"), the warm ones and the sized ones
/// ("sized-<pid>-<n>") is the same for every VM of one daemon: two pods starting at about the same time were given the same TAP name,
/// and the second failed with "Resource busy" (and a TAP was left behind in the host's network namespace).
pub fn tap_name_for(vm_id: &str) -> String {
    // FNV-1a, 64 bits; 44 bits of it as 11 hex digits: "tap_" and 11 digits are the 15 characters.
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in vm_id.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("tap_{:011x}", hash & 0xfff_ffff_ffff)
}

/// Set up networking for a VM: create TAP in host → CH add-net → move to pod netns + TC.
/// The TAP's name is reserved for the VM (a collision of two hashes is refused, not resolved by deleting the other's device). If any
/// step fails the device is deleted wherever it is and the name given back.
async fn setup_vm_networking(
    netns: &str,
    vm_id: &str,
    api_socket: &std::path::Path,
) -> anyhow::Result<(String, crate::netns::PodNetInfo)> {
    let tap_name = tap_name_for(vm_id);
    if !reserve_tap_name(&tap_name) {
        anyhow::bail!("the TAP name {tap_name} is already used by another VM");
    }
    match setup_vm_networking_inner(netns, vm_id, api_socket, &tap_name).await {
        Ok(v) => Ok(v),
        Err(e) => {
            let (ns, tap) = (netns.to_string(), tap_name.clone());
            let _ = tokio::task::spawn_blocking(move || crate::netns::delete_tap(&ns, &tap)).await;
            release_tap_name_of(vm_id);
            Err(e)
        }
    }
}

async fn setup_vm_networking_inner(
    netns: &str,
    vm_id: &str,
    api_socket: &std::path::Path,
    tap_name: &str,
) -> anyhow::Result<(String, crate::netns::PodNetInfo)> {
    let tap_name = tap_name.to_string();

    // Phase 1: probe pod netns for veth info, create TAP in host netns
    let hand_over = crate::rootless::enabled();
    let (info, tap_fd) = {
        let ns = netns.to_string();
        let tap = tap_name.clone();
        tokio::task::spawn_blocking(move || crate::netns::prepare_tap(&ns, &tap, hand_over))
            .await
            .context("prepare_tap panicked")?
            .context("prepare_tap")?
    };

    // Phase 2: hot-plug TAP into CH. As root, CH opens it by name (it can see it in the host netns). Rootless, CH may not enable the
    // device (that needs CAP_NET_ADMIN, which would let it change the host's network), so the descriptor is handed to it over the API
    // socket (SCM_RIGHTS) and the device is brought up later, in the pod's namespace, by the daemon.
    match tap_fd {
        Some(fd) => {
            let net_json = serde_json::json!({ "mac": &info.mac, "offload_csum": true, "offload_tso": true, "offload_ufo": true });
            crate::vm_lifecycle::api_request_with_fds(
                api_socket,
                "PUT",
                "/api/v1/vm.add-net",
                &net_json.to_string(),
                &[std::os::unix::io::AsRawFd::as_raw_fd(&fd)],
            )
            .await
            .context("hot-plug TAP (descriptor)")?;
        }
        None => {
            let net_json = serde_json::json!({
                "tap": &tap_name,
                "mac": &info.mac,
                "offload_csum": true,
                "offload_tso": true,
                "offload_ufo": true,
            });
            crate::vm_lifecycle::api_request_with_body(
                api_socket,
                "PUT",
                "/api/v1/vm.add-net",
                &net_json.to_string(),
            )
            .await
            .context("hot-plug TAP")?;
        }
    }
    info!("TAP {} hot-plugged into VM {}", tap_name, vm_id);

    // Phase 3: move TAP to pod netns, set up TC redirects
    {
        let ns = netns.to_string();
        let tap = tap_name.clone();
        tokio::task::spawn_blocking(move || crate::netns::activate_tap(&ns, &tap))
            .await
            .context("activate_tap panicked")?
            .context("activate_tap")?
    };
    info!("TAP {} moved to pod netns, TC active", tap_name);

    Ok((tap_name, info))
}

/// Configure the guest's network interface via the agent.
async fn configure_guest_network(
    agent: &cloudhv_proto::AgentServiceClient,
    ip_cidr: &str,
    gateway: &str,
) -> anyhow::Result<()> {
    let parts: Vec<&str> = ip_cidr.split('/').collect();
    let ip = parts[0];
    let prefix: u32 = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(24);
    let mut req = cloudhv_proto::ConfigureNetworkRequest::new();
    req.ip_address = ip.to_string();
    req.gateway = gateway.to_string();
    req.prefix_len = prefix;
    req.device = "eth0".to_string();
    let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(30));
    agent
        .configure_network(ctx, &req)
        .await
        .context("configure_network RPC")?;
    info!("guest network configured: ip={ip}/{prefix} gw={gateway}");
    Ok(())
}

async fn handle_status(pool: &Arc<Pool>, snapshots: &Arc<SnapshotManager>) -> serde_json::Value {
    serde_json::json!({
        "pool_ready": pool.ready_count().await,
        "active_vms": pool.active_count(),
        "shadow_vms_running": snapshots.active_shadow_count().await,
        "snapshot_keys": snapshots.snapshot_keys().await,
    })
}

/// The ID under which the shadow VM runs the container that a warm snapshot holds (`shadow.rs`).
pub fn shadow_container_id(image_key: &str) -> String {
    format!("shadow-ctr-{}", image_key)
}

/// Run the pod's container in a VM restored from a warm snapshot. The rootfs (the image's erofs) is already attached as the
/// first disk after the boot disk, so the agent is told it is pre-attached and does not wait for a hot-plug. Returns the PID in the guest.
async fn run_container_in_warm_vm(
    agent: &cloudhv_proto::AgentServiceClient,
    container_id: &str,
    config_json_b64: &str,
    volumes: &[cloudhv_proto::VolumeMount],
    terminal: bool,
) -> Result<u32> {
    use base64::Engine;
    let config_json = if config_json_b64.is_empty() {
        b"{}".to_vec()
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(config_json_b64)
            .unwrap_or_else(|_| b"{}".to_vec())
    };
    let mut req = cloudhv_proto::CreateContainerRequest::new();
    req.container_id = container_id.to_string();
    req.config_json = config_json;
    req.volumes = volumes.to_vec();
    req.terminal = terminal;
    req.rootfs_preattached = true;
    req.erofs_layers = 1;
    let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(30));
    let resp = agent
        .run_container(ctx, &req)
        .await
        .context("RunContainer on warm restore")?;
    info!(
        "warm restore: container {} started (pid={})",
        container_id, resp.pid
    );
    Ok(resp.pid)
}

/// The most volumes of a request, and files in a volume. The request line is limited too; these keep the work bounded.
const MAX_VOLUMES: usize = 64;
const MAX_FILES_PER_VOLUME: usize = 1024;

/// Whether the container's process has a terminal (`tty: true`): the optional boolean `terminal` of AcquireSandbox and AddContainer.
/// Anything but `true` is no terminal.
fn terminal_from_request(request: &serde_json::Value) -> bool {
    request
        .get("terminal")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// The `volumes` of a request (see the shim's `volumes.rs`): [{destination, source, readonly, files: [{path, content (base64), mode}]}].
pub(crate) fn volumes_from_request(request: &serde_json::Value) -> Vec<cloudhv_proto::VolumeMount> {
    use base64::Engine;
    let list = match request.get("volumes").and_then(|v| v.as_array()) {
        Some(l) => l,
        None => return Vec::new(),
    };
    list.iter()
        .take(MAX_VOLUMES)
        .map(|v| {
            let mut m = cloudhv_proto::VolumeMount::new();
            m.destination = v["destination"].as_str().unwrap_or("").to_string();
            m.source = v["source"].as_str().unwrap_or("").to_string();
            m.readonly = v["readonly"].as_bool().unwrap_or(false);
            // A disk-backed emptyDir: the guest agent mounts the disk the daemon hot-plugs (`emptydir::attach`), found by its serial.
            if let Some(id) = v["disk"]["id"].as_str() {
                m.volume_type = cloudhv_proto::VolumeType::BLOCK.into();
                m.fs_type = "ext4".to_string();
                m.serial = crate::emptydir::serial_of(id);
                return m;
            }
            // A persistent block volume: the guest agent mounts the device the daemon hot-plugs (`blockvol::attach`), found by its serial.
            if let Some(id) = v["block"]["id"].as_str() {
                m.volume_type = cloudhv_proto::VolumeType::BLOCK.into();
                m.fs_type = v["block"]["fs"].as_str().unwrap_or("ext4").to_string();
                m.serial = crate::blockvol::serial_of(id);
                return m;
            }
            for f in v["files"]
                .as_array()
                .into_iter()
                .flatten()
                .take(MAX_FILES_PER_VOLUME)
            {
                let mut file = cloudhv_proto::InlineFile::new();
                file.path = f["path"].as_str().unwrap_or("").to_string();
                file.content = base64::engine::general_purpose::STANDARD
                    .decode(f["content"].as_str().unwrap_or(""))
                    .unwrap_or_default();
                file.mode = (f["mode"].as_u64().unwrap_or(0o644) as u32) & 0o777;
                m.files.push(file);
            }
            m
        })
        .collect()
}

#[cfg(test)]
mod tap_name_tests {
    use super::*;

    #[test]
    fn the_tap_names_of_vms_that_share_a_prefix_differ_and_fit_an_interface_name() {
        let ids = [
            "pool-30322-4",
            "pool-30322-5",
            "sized-26035-0",
            "sized-26035-1",
            "warm-245df30fdec499a2cec3811ea5403906-0",
            "warm-245df30fdec499a2cec3811ea5403906-1",
        ];
        let names: Vec<String> = ids.iter().map(|id| tap_name_for(id)).collect();
        for name in &names {
            assert!(name.len() <= 15, "{name} is too long for an interface name");
            assert!(name.starts_with("tap_"));
        }
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "{names:?}");
        // The same VM always gets the same name (the shim and the cleanup find the device by it).
        assert_eq!(tap_name_for("pool-30322-4"), names[0]);
    }

    #[test]
    fn a_tap_name_is_reserved_for_one_vm_until_it_is_given_back() {
        let id = "test-reserve-1";
        let name = tap_name_for(id);
        assert!(reserve_tap_name(&name));
        assert!(
            !reserve_tap_name(&name),
            "a second VM with the same name is refused"
        );
        release_tap_name_of(id);
        assert!(reserve_tap_name(&name));
        release_tap_name_of(id);
    }

    #[test]
    fn the_disk_name_is_cut_by_characters_not_bytes() {
        assert_eq!(disk_id_for("0123456789abcdef"), "ctr-0123456789ab");
        assert_eq!(disk_id_for("abc"), "ctr-abc");
        // a multi-byte character across byte 12 used to panic when sliced by bytes
        assert_eq!(disk_id_for("aaaaaaaaaaaあ"), "ctr-aaaaaaaaaaaあ");
    }
}

#[cfg(test)]
mod request_tests {
    use super::*;

    #[test]
    fn a_warm_vm_name_differs_between_daemons_and_is_a_valid_id() {
        let key = "0b9d89849dd606c13fb0147b808d6fc7";
        // The same counter value in two daemons (a restart): two names.
        assert_ne!(warm_vm_id(key, 4086082, 0), warm_vm_id(key, 283149, 0));
        // Within one daemon the counter makes them differ.
        assert_ne!(warm_vm_id(key, 4086082, 0), warm_vm_id(key, 4086082, 1));
        // A pid of 7 digits (the largest, 4194304) and a counter of 6 digits still fit the 64 characters of an id.
        assert!(validate::is_id(&warm_vm_id(key, 4_194_304, 999_999)));
    }

    #[tokio::test]
    async fn a_request_line_is_read_up_to_the_limit_and_no_further() {
        let mut ok = tokio::io::BufReader::new(&b"{\"a\":1}\nnext\n"[..]);
        let mut buf = Vec::new();
        assert!(read_request_line(&mut ok, 64, &mut buf).await.unwrap());
        assert_eq!(buf, b"{\"a\":1}\n");
        assert!(read_request_line(&mut ok, 64, &mut buf).await.unwrap());
        assert_eq!(buf, b"next\n");
        assert!(
            !read_request_line(&mut ok, 64, &mut buf).await.unwrap(),
            "end of stream"
        );

        let long = vec![b'x'; 1000];
        let mut r = tokio::io::BufReader::new(&long[..]);
        assert!(
            read_request_line(&mut r, 100, &mut buf).await.is_err(),
            "no newline within the limit"
        );
        assert!(
            buf.len() <= 101,
            "not read on past the limit: {}",
            buf.len()
        );

        let mut exact = tokio::io::BufReader::new(&b"12345\n"[..]);
        assert!(
            read_request_line(&mut exact, 6, &mut buf).await.is_ok(),
            "a line of exactly the limit"
        );
        let mut over = tokio::io::BufReader::new(&b"123456\n"[..]);
        assert!(
            read_request_line(&mut over, 6, &mut buf).await.is_err(),
            "one byte over"
        );
    }

    #[test]
    fn the_ids_of_a_request_are_checked() {
        let r = serde_json::json!({"vm_id": "../..", "container_id": "abc123"});
        assert!(required_id(&r, "vm_id").is_err());
        assert_eq!(required_id(&r, "container_id").unwrap(), "abc123");
        assert!(required_id(&r, "missing").is_err());
    }

    #[test]
    fn the_terminal_flag_of_a_request_is_a_boolean_that_defaults_to_false() {
        use serde_json::json;
        assert!(terminal_from_request(&json!({"terminal": true})));
        for no in [
            json!({}),
            json!({"terminal": false}),
            json!({"terminal": "true"}),
            json!({"terminal": 1}),
            json!({"terminal": null}),
        ] {
            assert!(!terminal_from_request(&no), "{no}");
        }
    }

    #[test]
    fn volumes_of_a_request_are_bounded_and_modes_masked() {
        let mut vols = Vec::new();
        for _ in 0..100 {
            vols.push(
                serde_json::json!({"destination": "/d", "source": "/s", "readonly": false,
                "files": [{"path": "f", "content": "eA==", "mode": 0o4755}]}),
            );
        }
        let r = serde_json::json!({ "volumes": vols });
        let v = volumes_from_request(&r);
        assert_eq!(v.len(), MAX_VOLUMES);
        assert_eq!(v[0].files[0].mode, 0o755, "the setuid bit is not passed");
    }

    #[test]
    fn a_persistent_block_volume_becomes_a_block_volume_with_its_own_serial() {
        let id = "fedcba9876543210fedcba9876543210";
        let r = serde_json::json!({ "volumes": [
            {"destination": "/var/lib/postgresql", "source": format!("/run/cloudhv-vols/{id}"), "readonly": false, "files": [],
             "block": {"id": id, "major": 252, "minor": 3, "fs": "ext4"}},
        ]});
        let v = volumes_from_request(&r);
        assert_eq!(v[0].volume_type, cloudhv_proto::VolumeType::BLOCK.into());
        assert_eq!(v[0].fs_type, "ext4");
        assert_eq!(v[0].serial, "pvfedcba9876543210");
        assert_ne!(
            v[0].serial,
            crate::emptydir::serial_of(id),
            "not the serial of an emptyDir disk"
        );
        assert!(v[0].files.is_empty());
    }

    #[test]
    fn a_disk_volume_becomes_a_block_volume_with_a_serial_and_no_files() {
        let id = "0123456789abcdef0123456789abcdef";
        let r = serde_json::json!({ "volumes": [
            {"destination": "/data", "source": format!("/run/cloudhv-vols/{id}"), "readonly": false, "files": [],
             "disk": {"id": id, "size_mb": 1024}},
            {"destination": "/etc/hosts", "source": "/run/cloudhv-vols/ab", "readonly": true, "files": []},
        ]});
        let v = volumes_from_request(&r);
        assert_eq!(v[0].volume_type, cloudhv_proto::VolumeType::BLOCK.into());
        assert_eq!(v[0].fs_type, "ext4");
        assert_eq!(v[0].serial, "ed0123456789abcdef");
        assert!(v[0].files.is_empty());
        assert_eq!(v[0].source, format!("/run/cloudhv-vols/{id}"));
        assert_eq!(
            v[1].volume_type,
            cloudhv_proto::VolumeType::FILESYSTEM.into(),
            "other volumes stay file volumes"
        );
        assert!(v[1].serial.is_empty());
    }
}

/// The cgroup directory of the VM (null when cgroups are not in use). `ch_pid` is the launcher's; Cloud Hypervisor itself is its child,
/// in this same cgroup. The shim moves everything in it into the pod's cgroup, so that the pod's limits apply to the VM.
fn cgroup_of(vm_id: &str) -> Option<String> {
    crate::cgroup::path_of(vm_id)
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string())
}
