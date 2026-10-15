//! Low-level VM lifecycle operations: boot, snapshot, restore, destroy.
//!
//! These functions interact with Cloud Hypervisor's API socket directly.
//! They are used by the pool manager and shadow VM logic.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use log::{info, warn};
use tokio::net::UnixStream;
use tokio::process::Command;

use crate::config::DaemonConfig;

/// How long a call to the Cloud Hypervisor API may take. A snapshot or a restore of a big guest takes longer than the rest.
const API_TIMEOUT: Duration = Duration::from_secs(30);
const API_TIMEOUT_SLOW: Duration = Duration::from_secs(120);

/// Whether `path` is a socket and not a link (to some other socket or file). The daemon is root and the Cloud Hypervisor owns the
/// directory its sockets are in: a link put there would have root talk to whatever it points at.
fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

fn check_socket(path: &Path) -> Result<()> {
    if is_socket(path) {
        Ok(())
    } else {
        anyhow::bail!("{} is not a socket", path.display())
    }
}

/// Boot a VM from scratch (cold boot) of the default size. Returns the CH process PID.
pub async fn boot_vm(
    config: &DaemonConfig,
    vm_id: &str,
    state_dir: &Path,
    cid: u64,
    tap_name: Option<&str>,
    tap_mac: Option<&str>,
    extra_disks: &[(String, String, bool)], // (path, id, readonly)
) -> Result<u32> {
    boot_vm_sized(
        config,
        vm_id,
        state_dir,
        cid,
        tap_name,
        tap_mac,
        extra_disks,
        config.default_memory_mb,
        config.default_vcpus,
        true, // the base VM is snapshotted, and a snapshot of a shared (memfd) guest is what the pool is restored from
    )
    .await
}

/// Boot a VM from scratch with the given memory and vCPUs. `shared` backs the guest's RAM with a shared (memfd) mapping, which a VM
/// that will be snapshotted needs; a VM that will not be (a pod's VM of its own size) uses ordinary anonymous memory.
///
/// If any step after the process was started fails, the VM is stopped and everything of it removed (its user, its ACL entries, its
/// directory): a failed boot leaves nothing behind.
#[allow(clippy::too_many_arguments)]
pub async fn boot_vm_sized(
    config: &DaemonConfig,
    vm_id: &str,
    state_dir: &Path,
    cid: u64,
    tap_name: Option<&str>,
    tap_mac: Option<&str>,
    extra_disks: &[(String, String, bool)], // (path, id, readonly)
    memory_mb: u64,
    vcpus: u32,
    shared: bool,
) -> Result<u32> {
    // The console log is made before the Cloud Hypervisor exists, while the directory is still root's: afterwards the directory is the
    // VM's, which could have put a link in the place of the file. `spawn_ch` hands the file to the VM's user.
    let console_log = state_dir.join("console.log");
    let serial = config.serial_console_enabled();
    if serial {
        crate::fsutil::create_new(&console_log).context("create console.log")?;
    }
    let ch_pid = spawn_ch(config, state_dir, memory_mb, vcpus).await?;
    let api_socket = state_dir.join("api.sock");

    let steps = async {
        wait_ch_ready(&api_socket).await?;
        let vsock_socket = state_dir.join("vsock.sock");
        for (path, _, _) in extra_disks {
            crate::rootless::allow_read(Path::new(path), state_dir)?;
        }

        // Build VM config
        let mut disks = vec![serde_json::json!({
            "path": config.rootfs_path,
            "readonly": true,
            "image_type": "Raw",
            "id": "_disk0",
        })];
        for (path, id, readonly) in extra_disks {
            disks.push(serde_json::json!({
                "path": path,
                "readonly": readonly,
                "image_type": "Raw",
                "id": id,
            }));
        }

        let mut net = Vec::new();
        if let (Some(tap), Some(mac)) = (tap_name, tap_mac) {
            net.push(serde_json::json!({
                "tap": tap,
                "mac": mac,
            }));
        }

        let memory_bytes = memory_mb
            .checked_mul(1024 * 1024)
            .context("the memory size does not fit")?;
        let serial_config = if serial {
            serde_json::json!({ "mode": "File", "file": console_log.to_string_lossy() })
        } else {
            serde_json::json!({ "mode": "Off" })
        };
        let vm_config = serde_json::json!({
            "payload": {
                "kernel": config.kernel_path,
                "cmdline": config.kernel_args,
            },
            "cpus": {
                "boot_vcpus": vcpus,
                "max_vcpus": vcpus,
            },
            "memory": {
                "size": memory_bytes,
                "shared": shared,
            },
            "disks": disks,
            "net": net,
            "serial": serial_config,
            "console": { "mode": "Off" },
            "vsock": { "cid": cid, "socket": vsock_socket.to_string_lossy() },
        });

        api_request(
            &api_socket,
            "PUT",
            "/api/v1/vm.create",
            Some(&vm_config.to_string()),
        )
        .await?;
        api_request(&api_socket, "PUT", "/api/v1/vm.boot", None).await
    }
    .await;
    if let Err(e) = steps {
        shutdown_and_destroy(&api_socket, ch_pid, state_dir).await;
        return Err(e);
    }

    info!(
        "VM {} booted (pid={}, cid={}, {} MiB, {} vCPUs)",
        vm_id, ch_pid, cid, memory_mb, vcpus
    );
    Ok(ch_pid)
}

/// Spawn a Cloud Hypervisor process. Returns PID. Records it where only root can (`ctl.rs`). If rootless, the VM gets a user of its own
/// first, who owns the state directory (and `console.log`, if it is there already). A failure leaves no user and no record behind.
pub async fn spawn_ch(config: &DaemonConfig, state_dir: &Path, memory_mb: u64, vcpus: u32) -> Result<u32> {
    match spawn_ch_inner(config, state_dir, memory_mb, vcpus).await {
        Ok(pid) => Ok(pid),
        Err(e) => {
            crate::rootless::release(state_dir).await;
            crate::rootless::drop_outbox(state_dir);
            crate::ctl::remove(state_dir);
            Err(e)
        }
    }
}

async fn spawn_ch_inner(config: &DaemonConfig, state_dir: &Path, memory_mb: u64, vcpus: u32) -> Result<u32> {
    let api_socket = state_dir.join("api.sock");

    crate::ctl::create(state_dir)?;
    let user = crate::rootless::alloc_for(state_dir)?;
    if user.is_some() {
        crate::rootless::give(state_dir, state_dir)?;
        let console = state_dir.join("console.log");
        if console.exists() {
            crate::rootless::give(&console, state_dir)?;
        }
    }
    if let (Some(ids), true) = (user, crate::rootless::confined()) {
        return spawn_confined(config, state_dir, &api_socket, ids, memory_mb, vcpus).await;
    }

    let mut cmd = Command::new(&config.cloud_hypervisor_binary);
    cmd.arg("--api-socket")
        .arg(&api_socket)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some((uid, gid)) = user {
        // std clears the supplementary groups when it drops from root; the group is the one that may open /dev/kvm.
        cmd.uid(uid).gid(gid);
    }

    // Reset the signal mask before exec. The daemon's tokio runtime blocks
    // SIGTERM/SIGINT which CH inherits, preventing CH's signal handler from
    // receiving them. Clear the mask so CH can handle signals normally.
    let rootless = user.is_some();
    let max_processes = config.ch_max_processes;
    unsafe {
        cmd.pre_exec(move || {
            reset_signal_mask()?;
            // No core file: it would hold the guest's memory.
            let no_core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            if libc::setrlimit(libc::RLIMIT_CORE, &no_core) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if rootless {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The VM's user is its own, so this counts the processes and threads of this one VM: a fork bomb in a compromised
                // Cloud Hypervisor stops here, not at the host's limit.
                let nproc = libc::rlimit {
                    rlim_cur: max_processes as libc::rlim_t,
                    rlim_max: max_processes as libc::rlim_t,
                };
                if libc::setrlimit(libc::RLIMIT_NPROC, &nproc) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn().context("spawn cloud-hypervisor")?;
    let pid = child.id().context("get CH pid")?;
    if let Err(e) = crate::ctl::write_pid(state_dir, pid) {
        let _ = child.start_kill();
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        return Err(e);
    }
    // Spawn a background task to reap the child when it exits, avoiding
    // zombie processes without blocking the caller.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(pid)
}

fn reset_signal_mask() -> std::io::Result<()> {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        if libc::sigemptyset(&mut set) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Start a confined Cloud Hypervisor (see `confine.rs`): the daemon's own binary as the launcher, given the instructions on its standard
/// input. The pid returned and recorded is the launcher's, the supervisor of Cloud Hypervisor: it is the process that is signalled and
/// waited for, and it ends when Cloud Hypervisor does.
async fn spawn_confined(
    config: &DaemonConfig,
    state_dir: &Path,
    api_socket: &Path,
    ids: (u32, u32),
    memory_mb: u64,
    vcpus: u32,
) -> Result<u32> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use tokio::io::AsyncWriteExt;

    let vm_name = state_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let out_dir = crate::rootless::make_outbox(state_dir)?;

    // The cgroup, with its limits. Where it cannot be had the VM runs without (said once at start) unless it is required.
    let limits = crate::cgroup::limits_for(
        memory_mb,
        vcpus,
        config.ch_memory_overhead_mib,
        config.ch_max_processes,
        config.ch_cpu_limit,
    );
    let cgroup = match crate::cgroup::create(&vm_name, &limits) {
        Ok(c) => c,
        Err(e) if config.ch_require_cgroup => return Err(e.context("the VM's cgroup (ch_require_cgroup is set)")),
        Err(e) => {
            warn!("VM {vm_name} runs without cgroup limits: {e:#}");
            None
        }
    };

    // Where the new root is built: a directory of root's, in the registry.
    let ctl = crate::ctl::create(state_dir)?;
    let new_root = ctl.join("root");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&new_root)
        .with_context(|| format!("create {}", new_root.display()))?;
    std::fs::set_permissions(&new_root, std::fs::Permissions::from_mode(0o700))?;

    let spec = crate::confine::build_spec(
        &config.cloud_hypervisor_binary,
        vec!["--api-socket".to_string(), api_socket.display().to_string()],
        ids,
        crate::rootless::gid(),
        &new_root,
        state_dir,
        &out_dir,
        &config.kernel_path,
        &config.rootfs_path,
        &config.erofs_cache_dir,
        &config.ch_extra_ro_paths,
        &crate::confine::SYSTEM_LIB_DIRS,
        cgroup.as_deref(),
        config.ch_max_processes,
        &vm_name,
    );
    crate::confine::validate_spec(&spec).map_err(|e| anyhow::anyhow!("the confinement of {vm_name}: {e}"))?;

    let exe = std::env::current_exe().context("the daemon's own path (the launcher)")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode_0600()
        .open(ctl.join("launcher.log"))
        .context("open the launcher log")?;
    let mut cmd = Command::new(exe);
    cmd.arg("--ch-launcher")
        // Not read by the launcher: it makes the process's command line name the VM's API socket, which is how the daemon knows whose
        // process it is (`ctl::cmdline_is_ch_of`).
        .arg("--api-socket")
        .arg(api_socket)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log));
    unsafe {
        cmd.pre_exec(reset_signal_mask);
    }
    let mut child = cmd.spawn().context("spawn the launcher")?;
    let pid = child.id().context("get the launcher's pid")?;
    let wrote = async {
        let mut stdin = child.stdin.take().context("the launcher's standard input")?;
        stdin.write_all(&serde_json::to_vec(&spec)?).await?;
        stdin.shutdown().await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(e) = wrote.and_then(|_| crate::ctl::write_pid(state_dir, pid)) {
        let _ = child.start_kill();
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        return Err(e.context("instruct the launcher"));
    }
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(pid)
}

trait Mode0600 {
    fn mode_0600(&mut self) -> &mut Self;
}

impl Mode0600 for std::fs::OpenOptions {
    fn mode_0600(&mut self) -> &mut Self {
        use std::os::unix::fs::OpenOptionsExt;
        self.mode(0o600)
    }
}

/// Wait for the CH API socket to become ready.
pub async fn wait_ch_ready(api_socket: &Path) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if is_socket(api_socket) && UnixStream::connect(api_socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    anyhow::bail!(
        "CH API socket not ready: {}{}",
        api_socket.display(),
        launcher_log_tail(api_socket)
    )
}

/// What the launcher of a confined VM said (it says something only when it could not build the confinement or start Cloud Hypervisor).
fn launcher_log_tail(api_socket: &Path) -> String {
    let Some(state_dir) = api_socket.parent() else { return String::new() };
    let log = crate::ctl::dir_of(state_dir).join("launcher.log");
    match std::fs::read(&log) {
        Ok(b) if !b.is_empty() => {
            let tail = &b[b.len().saturating_sub(600)..];
            format!(" (launcher: {})", String::from_utf8_lossy(tail).trim())
        }
        _ => String::new(),
    }
}

/// Connect to the guest agent via vsock and verify it responds.
pub async fn wait_for_agent(vsock_socket: &Path) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        if is_socket(vsock_socket) {
            if let Ok(()) = try_agent_health(vsock_socket).await {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    anyhow::bail!("agent not responding on vsock: {}", vsock_socket.display())
}

/// Try to connect to the agent and check health.
async fn try_agent_health(vsock_socket: &Path) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let stream = UnixStream::connect(vsock_socket).await?;
    let (reader, mut writer) = stream.into_split();

    let connect_cmd = format!("CONNECT {}\n", cloudhv_common::AGENT_VSOCK_PORT);
    writer.write_all(connect_cmd.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(2), buf_reader.read_line(&mut response)).await??;

    if response.starts_with("OK") {
        Ok(())
    } else {
        anyhow::bail!("vsock CONNECT failed: {}", response.trim())
    }
}

/// Pause a running VM.
pub async fn pause_vm(api_socket: &Path) -> Result<()> {
    api_request(api_socket, "PUT", "/api/v1/vm.pause", None).await
}

/// Resume a paused VM.
pub async fn resume_vm(api_socket: &Path) -> Result<()> {
    api_request(api_socket, "PUT", "/api/v1/vm.resume", None).await
}

/// Take a snapshot of a paused VM into `dest_dir`, a directory that was just made (by root). Rootless, the VM that takes the snapshot
/// writes it, so it owns the directory meanwhile. What is in the directory afterwards is the VM's work and is not trusted: the caller
/// must destroy the VM (`shutdown_and_destroy`) and then `rootless::hand_over_snapshot` before it reads or uses any of it.
pub async fn snapshot_vm(api_socket: &Path, dest_dir: &Path) -> Result<()> {
    let state_dir = api_socket.parent();
    // A confined VM sees no directory but its own, so it writes into its outbox (`rootless::make_outbox`); the caller moves that to
    // `dest_dir` after the VM is dead (`rootless::collect_outbox`). Otherwise the VM writes into `dest_dir`, which it is given.
    let target = match state_dir {
        Some(sd) if crate::rootless::confined() => {
            crate::rootless::keep_outbox(sd);
            crate::confine::out_dir_of(sd)
        }
        Some(sd) => {
            crate::rootless::give(dest_dir, sd)?;
            dest_dir.to_path_buf()
        }
        None => dest_dir.to_path_buf(),
    };
    let body = serde_json::json!({
        "destination_url": format!("file://{}", target.display()),
    });
    api_request_timeout(
        api_socket,
        "PUT",
        "/api/v1/vm.snapshot",
        Some(&body.to_string()),
        API_TIMEOUT_SLOW,
    )
    .await
}

/// The memory restore mode of `vm.restore`: `OnDemand` (userfaultfd, lazy) or `Copy` (the snapshot is read into guest RAM before the
/// resume). Cloud Hypervisor has `memory_restore_mode` from v52 (the change, cloud-hypervisor#7800, came after v51.1). The daemon asks for
/// `OnDemand`; `CLOUDHV_MEMORY_RESTORE_MODE=Copy` asks for the other. With v53.0 `Copy` leaves a restored VM at about 40 MiB resident,
/// `OnDemand` at about 137 MiB of a 128 MiB guest (measured 2026-10-15): the opposite of what lazy loading is for.
fn memory_restore_mode() -> &'static str {
    match std::env::var("CLOUDHV_MEMORY_RESTORE_MODE").as_deref() {
        Ok("Copy") | Ok("copy") => "Copy",
        _ => "OnDemand",
    }
}

/// Restore a VM from a snapshot directory (see `memory_restore_mode`).
pub async fn restore_vm(api_socket: &Path, source_dir: &Path) -> Result<()> {
    let body = serde_json::json!({
        "source_url": format!("file://{}", source_dir.display()),
        "prefault": false,
        "memory_restore_mode": memory_restore_mode(),
    });
    api_request_timeout(
        api_socket,
        "PUT",
        "/api/v1/vm.restore",
        Some(&body.to_string()),
        API_TIMEOUT_SLOW,
    )
    .await
}

/// Prepare a snapshot directory for a specific VM: patch CID, vsock socket,
/// serial console path. Hardlinks large files (memory-ranges) for CoW.
///
/// The configuration of a snapshot was written by the Cloud Hypervisor that took it. It is checked (`validate_snapshot_config`) and the
/// pod's image is put in the place of the image disk the snapshot names, so that nothing of what the snapshot says about the host (a
/// disk path, a device) is used. `image` is the image of the pod, None for the base snapshot (which has no image disk). A snapshot that
/// does not pass is an error: the caller must not use it.
pub fn prepare_snapshot(
    config: &DaemonConfig,
    base_dir: &Path,
    vm_state_dir: &Path,
    new_cid: u64,
    new_vsock_socket: &Path,
    image: Option<&str>,
) -> Result<PathBuf> {
    let work_dir = vm_state_dir.join("snapshot");
    std::fs::create_dir_all(&work_dir)?;

    // Read, check, patch, and write config.json
    let config_str = std::fs::read_to_string(base_dir.join("config.json"))?;
    let mut cfg: serde_json::Value = serde_json::from_str(&config_str)?;

    // The image disk of a warm snapshot is the one the shadow VM had, named by the path it had then. The image of this pod has the same
    // content (the key is its hash), at whatever path the shim keeps it now: that path is the disk.
    if let Some(disks) = cfg.get_mut("disks").and_then(|d| d.as_array_mut()) {
        let mut extra = 0;
        for d in disks.iter_mut() {
            if d.get("path").and_then(|p| p.as_str()) == Some(config.rootfs_path.as_str()) {
                continue;
            }
            extra += 1;
            match (image, extra) {
                (Some(img), 1) => {
                    d["path"] = serde_json::json!(img);
                }
                _ => anyhow::bail!("the snapshot has a disk the VM may not have"),
            }
        }
    }
    crate::validate::validate_snapshot_config(&cfg, &config.kernel_path, &config.rootfs_path, image)
        .context("the snapshot's configuration is not accepted")?;

    // Hardlink large files
    for name in &["memory-ranges", "state.json"] {
        let src = base_dir.join(name);
        let dst = work_dir.join(name);
        std::fs::hard_link(&src, &dst)
            .or_else(|e| {
                if e.raw_os_error() == Some(18) {
                    std::fs::copy(&src, &dst).map(|_| ())
                } else {
                    Err(e)
                }
            })
            .with_context(|| format!("link {name} from base snapshot"))?;
    }

    // Patch vsock CID and socket
    if let Some(vsock) = cfg.pointer_mut("/vsock") {
        if let Some(obj) = vsock.as_object_mut() {
            obj.insert("cid".to_string(), serde_json::json!(new_cid));
            obj.insert(
                "socket".to_string(),
                serde_json::json!(new_vsock_socket.to_string_lossy()),
            );
        }
    }

    // Patch serial console path. The file is made here, before the Cloud Hypervisor exists (the directory is still root's); `spawn_ch`
    // hands it to the VM's user. A snapshot taken with the console off is restored with it off.
    let console_path = vm_state_dir.join("console.log");
    if let Some(serial) = cfg.pointer_mut("/serial") {
        if let Some(obj) = serial.as_object_mut() {
            if obj.get("mode").and_then(|v| v.as_str()) == Some("File") {
                if config.serial_console_enabled() {
                    crate::fsutil::create_new(&console_path).context("create console.log")?;
                    obj.insert(
                        "file".to_string(),
                        serde_json::json!(console_path.to_string_lossy()),
                    );
                } else {
                    *serial = serde_json::json!({ "mode": "Off" });
                }
            }
        }
    }

    std::fs::write(work_dir.join("config.json"), serde_json::to_string_pretty(&cfg)?)?;

    Ok(work_dir)
}

/// Wait up to `ms` milliseconds for the process to be gone.
async fn wait_gone(pid: u32, ms: u64) -> bool {
    let mut waited = 0;
    while waited <= ms {
        if !crate::ctl::pid_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        waited += 5;
    }
    !crate::ctl::pid_alive(pid)
}

/// Shutdown and destroy a VM: stop its Cloud Hypervisor (only if the process is really that VM's; see `ctl::signal_ch`), kill what its
/// user left, take back its ACL entries, free its user, and remove its record and its directory. Safe to call for a VM that is gone.
pub async fn shutdown_and_destroy(_api_socket: &Path, ch_pid: u32, state_dir: &Path) {
    let t0 = std::time::Instant::now();

    // Send SIGTERM to CH. CH handles SIGTERM by shutting down the VM and
    // exiting cleanly (requires signal mask to be cleared in spawn_ch).
    if crate::ctl::pid_alive(ch_pid) {
        crate::ctl::signal_ch(ch_pid, state_dir, libc::SIGTERM);
        if wait_gone(ch_pid, 1000).await {
            info!("VM {} exited gracefully in {}ms", ch_pid, t0.elapsed().as_millis());
        } else {
            // Fallback: force kill if CH did not exit within 1s
            warn!(
                "VM {} did not exit after {}ms, force killing",
                ch_pid,
                t0.elapsed().as_millis()
            );
            crate::ctl::signal_ch(ch_pid, state_dir, libc::SIGKILL);
            if !wait_gone(ch_pid, 500).await {
                warn!("VM {} is still there after SIGKILL", ch_pid);
            }
        }
    }

    // Rootless: every other process of the VM's user goes too, and only then is the user free for another VM.
    crate::rootless::release(state_dir).await;
    crate::rootless::drop_outbox(state_dir);
    crate::ctl::remove(state_dir);
    let _ = std::fs::remove_dir_all(state_dir);
    info!("VM {} destroyed in {}ms", ch_pid, t0.elapsed().as_millis());
}

/// Send an HTTP request to the CH API socket using hyper.
pub async fn api_request(
    socket_path: &Path,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<()> {
    api_request_timeout(socket_path, method, path, body, API_TIMEOUT).await
}

/// `api_request` that gives up after `timeout`: a Cloud Hypervisor that has been compromised can leave a call hanging for ever.
pub async fn api_request_timeout(
    socket_path: &Path,
    method: &str,
    path: &str,
    body: Option<&str>,
    timeout: Duration,
) -> Result<()> {
    match tokio::time::timeout(timeout, api_request_inner(socket_path, method, path, body)).await {
        Ok(r) => r,
        Err(_) => anyhow::bail!("CH API {method} {path}: no answer in {}s", timeout.as_secs()),
    }
}

async fn api_request_inner(
    socket_path: &Path,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<()> {
    use http_body_util::{BodyExt, Empty, Full};
    use hyper::body::Bytes;
    use hyper::Request;
    use hyper_util::rt::TokioIo;

    check_socket(socket_path)?;
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connect to CH API: {}", socket_path.display()))?;
    let io = TokioIo::new(stream);

    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .context("HTTP handshake with CH API")?;
    tokio::spawn(conn);

    let req = if let Some(b) = body {
        Request::builder()
            .method(method)
            .uri(path)
            .header("Host", "localhost")
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(b.to_string())).boxed())
            .context("build request")?
    } else {
        Request::builder()
            .method(method)
            .uri(path)
            .header("Host", "localhost")
            .body(Empty::<Bytes>::new().boxed())
            .context("build request")?
    };

    let resp = sender.send_request(req).await.context("send request")?;
    let status = resp.status();

    if !status.is_success() {
        let body_bytes = resp.into_body().collect().await?.to_bytes();
        let body_str = String::from_utf8_lossy(&body_bytes);
        anyhow::bail!("CH API {method} {path}: {} {}", status, body_str);
    }

    Ok(())
}

/// Convenience: send a CH API request with a body.
pub async fn api_request_with_body(
    socket_path: &Path,
    method: &str,
    path: &str,
    body: &str,
) -> Result<()> {
    api_request(socket_path, method, path, Some(body)).await
}

/// A request to the CH API that carries file descriptors (SCM_RIGHTS), which the API takes for devices backed by descriptors the caller
/// opened (a TAP). The descriptors go with the first bytes of the request; the answer is read to its end. Reading and writing give up
/// after ten seconds each.
pub async fn api_request_with_fds(
    socket_path: &Path,
    method: &str,
    path: &str,
    body: &str,
    fds: &[std::os::unix::io::RawFd],
) -> Result<()> {
    check_socket(socket_path)?;
    let socket_path = socket_path.to_path_buf();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let fds = fds.to_vec();
    let (method, path) = (method.to_string(), path.to_string());
    tokio::task::spawn_blocking(move || -> Result<()> {
        use std::io::{Read, Write};
        use std::os::unix::io::AsRawFd;
        let mut stream = std::os::unix::net::UnixStream::connect(&socket_path)
            .with_context(|| format!("connect {}", socket_path.display()))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let bytes = request.as_bytes();

        let fd_len = std::mem::size_of_val(fds.as_slice());
        let space = unsafe { libc::CMSG_SPACE(fd_len as u32) } as usize;
        let mut control = vec![0u8; space];
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr() as *mut libc::c_void,
            iov_len: bytes.len(),
        };
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space as _;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(fd_len as u32) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(cmsg), fd_len);
        }
        let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, 0) };
        if sent < 0 {
            return Err(std::io::Error::last_os_error()).context("sendmsg");
        }
        stream.write_all(&bytes[sent as usize..])?;

        // The answer is small; a Cloud Hypervisor that keeps writing is not read for more than a megabyte.
        let mut answer = Vec::new();
        let _ = stream.take(1 << 20).read_to_end(&mut answer);
        let text = String::from_utf8_lossy(&answer);
        let status = text.split_whitespace().nth(1).unwrap_or("?");
        if !status.starts_with('2') {
            let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
            anyhow::bail!("CH API {method} {path}: {status} {body}");
        }
        Ok(())
    })
    .await
    .context("api request with descriptors panicked")?
}

/// Connect to the guest agent via vsock and return ttrpc clients.
pub async fn connect_agent(
    vsock_socket: &Path,
) -> Result<(
    cloudhv_proto::AgentServiceClient,
    cloudhv_proto::HealthServiceClient,
)> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    check_socket(vsock_socket)?;
    let stream = UnixStream::connect(vsock_socket)
        .await
        .with_context(|| format!("connect vsock: {}", vsock_socket.display()))?;

    let (reader, mut writer) = stream.into_split();
    let connect_cmd = format!("CONNECT {}\n", cloudhv_common::AGENT_VSOCK_PORT);
    writer.write_all(connect_cmd.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(5), buf_reader.read_line(&mut response))
        .await
        .map_err(|_| anyhow::anyhow!("vsock CONNECT handshake timed out"))?
        .context("vsock read")?;

    if !response.starts_with("OK") {
        anyhow::bail!("vsock CONNECT failed: {}", response.trim());
    }

    let stream = buf_reader.into_inner().reunite(writer)?;
    let std_stream = stream.into_std()?;
    use std::os::unix::io::IntoRawFd;
    let fd = std_stream.into_raw_fd();

    let client = ttrpc::asynchronous::Client::new(fd);
    let agent = cloudhv_proto::AgentServiceClient::new(client.clone());
    let health = cloudhv_proto::HealthServiceClient::new(client);

    Ok((agent, health))
}
