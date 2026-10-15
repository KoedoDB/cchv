//! Low-level VM lifecycle operations: boot, snapshot, restore, destroy.
//!
//! These functions interact with Cloud Hypervisor's API socket directly.
//! They are used by the pool manager and shadow VM logic.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use log::{info, warn};
use tokio::net::UnixStream;
use tokio::process::Command;

use crate::config::DaemonConfig;

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
    let ch_pid = spawn_ch(config, state_dir).await?;
    let api_socket = state_dir.join("api.sock");
    wait_ch_ready(&api_socket).await?;

    let vsock_socket = state_dir.join("vsock.sock");
    let console_log = state_dir.join("console.log");
    std::fs::File::create(&console_log)?;
    crate::rootless::give(&console_log, state_dir)?;
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
            "size": memory_mb * 1024 * 1024,
            "shared": shared,
        },
        "disks": disks,
        "net": net,
        "serial": { "mode": "File", "file": console_log.to_string_lossy() },
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
    api_request(&api_socket, "PUT", "/api/v1/vm.boot", None).await?;

    // Save PID for cleanup
    std::fs::write(state_dir.join("ch.pid"), ch_pid.to_string())?;

    info!(
        "VM {} booted (pid={}, cid={}, {} MiB, {} vCPUs)",
        vm_id, ch_pid, cid, memory_mb, vcpus
    );
    Ok(ch_pid)
}

/// Spawn a Cloud Hypervisor process. Returns PID.
pub async fn spawn_ch(config: &DaemonConfig, state_dir: &Path) -> Result<u32> {
    let api_socket = state_dir.join("api.sock");
    let mut cmd = Command::new(&config.cloud_hypervisor_binary);
    cmd.arg("--api-socket")
        .arg(&api_socket)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // Rootless: a user for this VM alone, who owns the state directory (and console log, if it is there already).
    let user = crate::rootless::alloc()?;
    if let Some((uid, gid)) = user {
        let setup = (|| -> Result<()> {
            crate::rootless::record(state_dir, uid)?;
            crate::rootless::give(state_dir, state_dir)?;
            let console = state_dir.join("console.log");
            if console.exists() {
                crate::rootless::give(&console, state_dir)?;
            }
            Ok(())
        })();
        if let Err(e) = setup {
            crate::rootless::release(state_dir);
            return Err(e);
        }
        // std clears the supplementary groups when it drops from root; the group is the one that may open /dev/kvm.
        cmd.uid(uid).gid(gid);
    }

    // Reset the signal mask before exec. The daemon's tokio runtime blocks
    // SIGTERM/SIGINT which CH inherits, preventing CH's signal handler from
    // receiving them. Clear the mask so CH can handle signals normally.
    let rootless = user.is_some();
    unsafe {
        cmd.pre_exec(move || {
            let mut set: libc::sigset_t = std::mem::zeroed();
            if libc::sigemptyset(&mut set) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if rootless && libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            crate::rootless::release(state_dir);
            return Err(e).context("spawn cloud-hypervisor");
        }
    };

    let pid = child.id().context("get CH pid")?;
    // Spawn a background task to reap the child when it exits, avoiding
    // zombie processes without blocking the caller.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    std::fs::write(state_dir.join("ch.pid"), pid.to_string())?;
    Ok(pid)
}

/// Wait for the CH API socket to become ready.
pub async fn wait_ch_ready(api_socket: &Path) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if api_socket.exists() && UnixStream::connect(api_socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    anyhow::bail!("CH API socket not ready: {}", api_socket.display())
}

/// Connect to the guest agent via vsock and verify it responds.
pub async fn wait_for_agent(vsock_socket: &Path) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        if vsock_socket.exists() {
            if let Ok(()) = try_agent_health(vsock_socket).await {
                return Ok(());
            }
        }
        tokio::task::yield_now().await;
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
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        buf_reader.read_line(&mut response),
    )
    .await??;

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

/// Take a snapshot of a paused VM.
/// `shared`: the base snapshot (see `rootless::hand_over_snapshot`).
pub async fn snapshot_vm(api_socket: &Path, dest_dir: &Path, shared: bool) -> Result<()> {
    // Rootless: the VM that takes the snapshot writes it, so it owns the directory meanwhile; afterwards the group of all VMs reads it.
    if let Some(state_dir) = api_socket.parent() {
        crate::rootless::give(dest_dir, state_dir)?;
    }
    let body = serde_json::json!({
        "destination_url": format!("file://{}", dest_dir.display()),
    });
    let result = api_request(
        api_socket,
        "PUT",
        "/api/v1/vm.snapshot",
        Some(&body.to_string()),
    )
    .await;
    crate::rootless::hand_over_snapshot(dest_dir, shared)?;
    result
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
    api_request(
        api_socket,
        "PUT",
        "/api/v1/vm.restore",
        Some(&body.to_string()),
    )
    .await
}

/// Prepare a snapshot directory for a specific VM: patch CID, vsock socket,
/// serial console path. Hardlinks large files (memory-ranges) for CoW.
pub fn prepare_snapshot(
    base_dir: &Path,
    vm_state_dir: &Path,
    new_cid: u64,
    new_vsock_socket: &Path,
) -> Result<PathBuf> {
    let work_dir = vm_state_dir.join("snapshot");
    std::fs::create_dir_all(&work_dir)?;

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

    // Read, patch, and write config.json
    let config_str = std::fs::read_to_string(base_dir.join("config.json"))?;
    let mut config: serde_json::Value = serde_json::from_str(&config_str)?;

    // Patch vsock CID and socket
    if let Some(vsock) = config.pointer_mut("/vsock") {
        if let Some(obj) = vsock.as_object_mut() {
            obj.insert("cid".to_string(), serde_json::json!(new_cid));
            obj.insert(
                "socket".to_string(),
                serde_json::json!(new_vsock_socket.to_string_lossy()),
            );
        }
    }

    // Patch serial console path
    let console_path = vm_state_dir.join("console.log");
    std::fs::File::create(&console_path)?;
    if let Some(serial) = config.pointer_mut("/serial") {
        if let Some(obj) = serial.as_object_mut() {
            if obj.get("mode").and_then(|v| v.as_str()) == Some("File") {
                obj.insert(
                    "file".to_string(),
                    serde_json::json!(console_path.to_string_lossy()),
                );
            }
        }
    }

    std::fs::write(
        work_dir.join("config.json"),
        serde_json::to_string_pretty(&config)?,
    )?;

    Ok(work_dir)
}

/// Shutdown and destroy a VM.
pub async fn shutdown_and_destroy(_api_socket: &Path, ch_pid: u32, state_dir: &Path) {
    let t0 = std::time::Instant::now();

    // Send SIGTERM to CH. CH handles SIGTERM by shutting down the VM and
    // exiting cleanly (requires signal mask to be cleared in spawn_ch).
    unsafe {
        libc::kill(ch_pid as i32, libc::SIGTERM);
    }

    // Wait for CH to exit gracefully
    let proc_path = format!("/proc/{ch_pid}");
    for _ in 0..200 {
        if !std::path::Path::new(&proc_path).exists() {
            info!(
                "VM {} exited gracefully in {}ms",
                ch_pid,
                t0.elapsed().as_millis()
            );
            crate::rootless::release(state_dir);
            let _ = std::fs::remove_dir_all(state_dir);
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // Fallback: force kill if CH did not exit within 1s
    warn!(
        "VM {} did not exit after {}ms, force killing",
        ch_pid,
        t0.elapsed().as_millis()
    );
    unsafe {
        libc::kill(ch_pid as i32, libc::SIGKILL);
    }
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    crate::rootless::release(state_dir);
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
    use http_body_util::{BodyExt, Empty, Full};
    use hyper::body::Bytes;
    use hyper::Request;
    use hyper_util::rt::TokioIo;

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
/// opened (a TAP). The descriptors go with the first bytes of the request; the answer is read to its end.
pub async fn api_request_with_fds(
    socket_path: &Path,
    method: &str,
    path: &str,
    body: &str,
    fds: &[std::os::unix::io::RawFd],
) -> Result<()> {
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
        stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
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

        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer);
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

    let stream = UnixStream::connect(vsock_socket)
        .await
        .with_context(|| format!("connect vsock: {}", vsock_socket.display()))?;

    let (reader, mut writer) = stream.into_split();
    let connect_cmd = format!("CONNECT {}\n", cloudhv_common::AGENT_VSOCK_PORT);
    writer.write_all(connect_cmd.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut response = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        buf_reader.read_line(&mut response),
    )
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
