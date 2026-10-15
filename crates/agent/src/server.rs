use std::sync::Arc;
use std::time::Duration;

use crate::proto::agent::*;
use crate::proto::agent_ttrpc::{self, AgentService, HealthService};
use anyhow::Result;
use async_trait::async_trait;
use log::{debug, info};
use tokio::signal::unix::{signal, SignalKind};
use ttrpc::r#async::TtrpcContext;

use cloudhv_common::AGENT_VSOCK_PORT;

use crate::container::ContainerManager;
use crate::exec::ExecManager;

// --- AgentService implementation ---

/// Convert proto VolumeMount list to VolumeInfo for the container manager.
fn proto_volumes_to_info(volumes: &[VolumeMount]) -> Vec<crate::container::VolumeInfo> {
    volumes
        .iter()
        .map(|v| crate::container::VolumeInfo {
            destination: v.destination.clone(),
            source: v.source.clone(),
            readonly: v.readonly,
            is_block: v.volume_type == crate::proto::agent::VolumeType::BLOCK.into(),
            fs_type: v.fs_type.clone(),
            options: v.options.clone(),
            serial: v.serial.clone(),
            inline_files: v
                .files
                .iter()
                .map(|f| crate::container::InlineFileInfo {
                    path: f.path.clone(),
                    content: f.content.clone(),
                    mode: f.mode,
                })
                .collect(),
        })
        .collect()
}

/// Create a container: reserve its ID under the manager's lock, make its root file system and volumes WITHOUT the lock (waiting for a
/// disk to appear must not stop the other calls), and register it under the lock again. A failure gives the ID back.
async fn create_flow(
    mgr: &Arc<tokio::sync::Mutex<ContainerManager>>,
    req: &CreateContainerRequest,
) -> anyhow::Result<()> {
    let volumes = proto_volumes_to_info(&req.volumes);
    let plan = mgr
        .lock()
        .await
        .begin_create(&req.container_id, &volumes)?;
    match crate::container::prepare(
        plan,
        &req.container_id,
        &volumes,
        &req.config_json,
        req.rootfs_preattached,
        req.erofs_layers,
        &req.layer_serials,
        req.terminal,
    )
    .await
    {
        Ok(prepared) => {
            mgr.lock().await.finish_create(prepared);
            Ok(())
        }
        Err(e) => {
            mgr.lock().await.abort_create(&req.container_id);
            Err(e)
        }
    }
}

struct AgentServiceHandler {
    container_manager: Arc<tokio::sync::Mutex<ContainerManager>>,
    exec_manager: Arc<ExecManager>,
}

/// An error answer of an exec call.
fn exec_err(what: &str, e: anyhow::Error) -> ttrpc::Error {
    ttrpc::Error::Others(format!("{what}: {e:#}"))
}

#[async_trait]
impl AgentService for AgentServiceHandler {
    async fn create_container(
        &self,
        _ctx: &TtrpcContext,
        req: CreateContainerRequest,
    ) -> ttrpc::Result<CreateContainerResponse> {
        info!("RPC: create_container id={}", req.container_id);
        create_flow(&self.container_manager, &req)
            .await
            .map_err(|e| ttrpc::Error::Others(format!("create_container failed: {e:#}")))?;
        // The container exists but its process does not yet: there is no PID to report (not the agent's own, which is 1).
        let mut resp = CreateContainerResponse::new();
        resp.pid = 0;
        Ok(resp)
    }

    async fn start_container(
        &self,
        _ctx: &TtrpcContext,
        req: StartContainerRequest,
    ) -> ttrpc::Result<StartContainerResponse> {
        info!("RPC: start_container id={}", req.container_id);
        let mut mgr = self.container_manager.lock().await;
        let pid = mgr
            .start(&req.container_id)
            .await
            .map_err(|e| ttrpc::Error::Others(format!("start_container failed: {e}")))?;
        let mut resp = StartContainerResponse::new();
        resp.pid = pid;
        Ok(resp)
    }

    async fn run_container(
        &self,
        _ctx: &TtrpcContext,
        req: CreateContainerRequest,
    ) -> ttrpc::Result<StartContainerResponse> {
        info!("RPC: run_container id={}", req.container_id);
        create_flow(&self.container_manager, &req)
            .await
            .map_err(|e| ttrpc::Error::Others(format!("run_container failed: {e:#}")))?;
        let pid = self
            .container_manager
            .lock()
            .await
            .start(&req.container_id)
            .await
            .map_err(|e| ttrpc::Error::Others(format!("run_container failed: {e:#}")))?;
        let mut resp = StartContainerResponse::new();
        resp.pid = pid;
        Ok(resp)
    }

    async fn kill_container(
        &self,
        _ctx: &TtrpcContext,
        req: KillContainerRequest,
    ) -> ttrpc::Result<KillContainerResponse> {
        info!(
            "RPC: kill_container id={} signal={}",
            req.container_id, req.signal
        );
        let mgr = self.container_manager.lock().await;
        mgr.kill(&req.container_id, req.signal)
            .await
            .map_err(|e| ttrpc::Error::Others(format!("kill_container failed: {e}")))?;
        Ok(KillContainerResponse::new())
    }

    async fn delete_container(
        &self,
        _ctx: &TtrpcContext,
        req: DeleteContainerRequest,
    ) -> ttrpc::Result<DeleteContainerResponse> {
        info!("RPC: delete_container id={}", req.container_id);
        // Its execs go first: their processes are in the container.
        self.exec_manager.remove_container(&req.container_id).await;
        let mut mgr = self.container_manager.lock().await;
        let (pid, exit_code) = mgr
            .delete(&req.container_id)
            .await
            .map_err(|e| ttrpc::Error::Others(format!("delete_container failed: {e}")))?;
        let mut resp = DeleteContainerResponse::new();
        resp.pid = pid;
        resp.exit_status = exit_code as u32;
        Ok(resp)
    }

    async fn wait_container(
        &self,
        _ctx: &TtrpcContext,
        req: WaitContainerRequest,
    ) -> ttrpc::Result<WaitContainerResponse> {
        info!("RPC: wait_container id={}", req.container_id);

        // Briefly lock to get the exit receiver, then drop the lock.
        let mut rx = {
            let mgr = self.container_manager.lock().await;
            mgr.get_exit_receiver(&req.container_id).ok_or_else(|| {
                ttrpc::Error::Others(format!(
                    "wait_container: unknown container {}",
                    req.container_id
                ))
            })?
        };

        // Clone data out of the Ref guard so we never hold a non-Send borrow across .await.
        let already_exited = { rx.borrow().clone() };

        // Wait for the container to exit, however long it runs, without holding the container_manager lock. (A container that has not
        // exited is never marked stopped: a timeout here used to do that, and then a kill of a live container was ignored. The caller has
        // its own deadline.)
        let (exit_code, exited_at) = if let Some(status) = already_exited {
            (status.code, status.exited_at)
        } else {
            loop {
                match rx.changed().await {
                    Ok(()) => {
                        let status = { rx.borrow().clone() };
                        if let Some(s) = status {
                            break (s.code, s.exited_at);
                        }
                    }
                    // The sender is gone: the task that watched the process ended without a status.
                    Err(_) => break (137, chrono::Utc::now().to_rfc3339()),
                }
            }
        };

        // The container has exited: record it.
        {
            let mut mgr = self.container_manager.lock().await;
            mgr.mark_stopped(&req.container_id, exit_code);
        }

        let mut resp = WaitContainerResponse::new();
        resp.exit_status = exit_code as u32;
        resp.exited_at = exited_at;
        Ok(resp)
    }

    async fn exec_process(
        &self,
        _ctx: &TtrpcContext,
        req: ExecProcessRequest,
    ) -> ttrpc::Result<ExecProcessResponse> {
        info!(
            "RPC: exec_process container={} exec_id={}",
            req.container_id, req.exec_id
        );
        Err(ttrpc::Error::RpcStatus(ttrpc::get_status(
            ttrpc::Code::UNIMPLEMENTED,
            "exec_process not yet implemented".to_string(),
        )))
    }

    // --- Exec. The calls log ids and sizes only: a process definition and what is written or read can hold secrets. ---

    async fn exec_create(
        &self,
        _ctx: &TtrpcContext,
        req: ExecCreateRequest,
    ) -> ttrpc::Result<ExecCreateResponse> {
        info!(
            "RPC: exec_create container={} exec_id={} terminal={} ({} bytes)",
            req.container_id,
            req.exec_id,
            req.terminal,
            req.process_json.len()
        );
        let target = self
            .container_manager
            .lock()
            .await
            .exec_target(&req.container_id)
            .map_err(|e| exec_err("exec_create", e))?;
        self.exec_manager
            .create(&req.container_id, &req.exec_id, &req.process_json, req.terminal, target.as_ref())
            .await
            .map_err(|e| exec_err("exec_create", e))?;
        Ok(ExecCreateResponse::new())
    }

    async fn exec_start(
        &self,
        _ctx: &TtrpcContext,
        req: ExecStartRequest,
    ) -> ttrpc::Result<ExecStartResponse> {
        info!("RPC: exec_start container={} exec_id={}", req.container_id, req.exec_id);
        let pid = self
            .exec_manager
            .start(&req.container_id, &req.exec_id)
            .await
            .map_err(|e| exec_err("exec_start", e))?;
        let mut resp = ExecStartResponse::new();
        resp.pid = pid;
        Ok(resp)
    }

    async fn exec_kill(
        &self,
        _ctx: &TtrpcContext,
        req: ExecKillRequest,
    ) -> ttrpc::Result<ExecKillResponse> {
        info!(
            "RPC: exec_kill container={} exec_id={} signal={}",
            req.container_id, req.exec_id, req.signal
        );
        self.exec_manager
            .kill(&req.container_id, &req.exec_id, req.signal)
            .await
            .map_err(|e| exec_err("exec_kill", e))?;
        Ok(ExecKillResponse::new())
    }

    async fn exec_wait(
        &self,
        _ctx: &TtrpcContext,
        req: ExecWaitRequest,
    ) -> ttrpc::Result<ExecWaitResponse> {
        info!("RPC: exec_wait container={} exec_id={}", req.container_id, req.exec_id);
        let status = self
            .exec_manager
            .wait(&req.container_id, &req.exec_id)
            .await
            .map_err(|e| exec_err("exec_wait", e))?;
        let mut resp = ExecWaitResponse::new();
        resp.exit_status = status.code as u32;
        resp.exited_at = status.exited_at;
        Ok(resp)
    }

    async fn exec_delete(
        &self,
        _ctx: &TtrpcContext,
        req: ExecDeleteRequest,
    ) -> ttrpc::Result<ExecDeleteResponse> {
        info!("RPC: exec_delete container={} exec_id={}", req.container_id, req.exec_id);
        self.exec_manager
            .delete(&req.container_id, &req.exec_id)
            .await
            .map_err(|e| exec_err("exec_delete", e))?;
        Ok(ExecDeleteResponse::new())
    }

    async fn exec_write(
        &self,
        _ctx: &TtrpcContext,
        req: ExecWriteRequest,
    ) -> ttrpc::Result<ExecWriteResponse> {
        debug!(
            "RPC: exec_write container={} exec_id={} ({} bytes)",
            req.container_id,
            req.exec_id,
            req.data.len()
        );
        let n = if req.exec_id == crate::exec::INIT_EXEC_ID {
            // The container's own terminal.
            let master = self
                .container_manager
                .lock()
                .await
                .terminal_master(&req.container_id)
                .map_err(|e| exec_err("exec_write", e))?;
            crate::exec::write_terminal(&master, &req.data)
                .await
                .map_err(|e| exec_err("exec_write", e))?
        } else {
            self.exec_manager
                .write(&req.container_id, &req.exec_id, &req.data)
                .await
                .map_err(|e| exec_err("exec_write", e))?
        };
        let mut resp = ExecWriteResponse::new();
        resp.written = n as u32;
        Ok(resp)
    }

    async fn exec_close_stdin(
        &self,
        _ctx: &TtrpcContext,
        req: ExecCloseStdinRequest,
    ) -> ttrpc::Result<ExecCloseStdinResponse> {
        info!("RPC: exec_close_stdin container={} exec_id={}", req.container_id, req.exec_id);
        if req.exec_id == crate::exec::INIT_EXEC_ID {
            // A terminal has no end of input (the client sends ^D if it wants one): accepted, nothing to do. A container with no
            // terminal has no input to close either.
            return Ok(ExecCloseStdinResponse::new());
        }
        self.exec_manager
            .close_stdin(&req.container_id, &req.exec_id)
            .await
            .map_err(|e| exec_err("exec_close_stdin", e))?;
        Ok(ExecCloseStdinResponse::new())
    }

    async fn exec_read(
        &self,
        _ctx: &TtrpcContext,
        req: ExecReadRequest,
    ) -> ttrpc::Result<ExecReadResponse> {
        let r = self
            .exec_manager
            .read(
                &req.container_id,
                &req.exec_id,
                req.stdout_offset,
                req.stderr_offset,
                req.max_bytes as usize,
            )
            .await
            .map_err(|e| exec_err("exec_read", e))?;
        let mut resp = ExecReadResponse::new();
        resp.stdout = r.stdout;
        resp.stderr = r.stderr;
        resp.stdout_offset = r.stdout_next;
        resp.stderr_offset = r.stderr_next;
        resp.eof = r.eof;
        resp.stdout_dropped = r.stdout_dropped;
        resp.stderr_dropped = r.stderr_dropped;
        Ok(resp)
    }

    async fn exec_resize(
        &self,
        _ctx: &TtrpcContext,
        req: ExecResizeRequest,
    ) -> ttrpc::Result<ExecResizeResponse> {
        debug!(
            "RPC: exec_resize container={} exec_id={} {}x{}",
            req.container_id, req.exec_id, req.width, req.height
        );
        if req.exec_id == crate::exec::INIT_EXEC_ID {
            if !crate::exec::valid_terminal_size(req.width, req.height) {
                return Err(exec_err("exec_resize", anyhow::anyhow!("invalid terminal size")));
            }
            let master = self
                .container_manager
                .lock()
                .await
                .terminal_master(&req.container_id)
                .map_err(|e| exec_err("exec_resize", e))?;
            crate::exec::set_window_size(master.raw_fd(), req.width as u16, req.height as u16)
                .map_err(|e| exec_err("exec_resize", anyhow::Error::new(e).context("TIOCSWINSZ")))?;
            return Ok(ExecResizeResponse::new());
        }
        self.exec_manager
            .resize(&req.container_id, &req.exec_id, req.width, req.height)
            .await
            .map_err(|e| exec_err("exec_resize", e))?;
        Ok(ExecResizeResponse::new())
    }

    async fn state_container(
        &self,
        _ctx: &TtrpcContext,
        req: StateContainerRequest,
    ) -> ttrpc::Result<StateContainerResponse> {
        debug!("RPC: state_container id={}", req.container_id);
        let mgr = self.container_manager.lock().await;
        mgr.state(&req.container_id)
            .await
            .map_err(|e| ttrpc::Error::Others(format!("state_container failed: {e}")))
    }

    async fn get_mem_info(
        &self,
        _ctx: &TtrpcContext,
        _req: GetMemInfoRequest,
    ) -> ttrpc::Result<GetMemInfoResponse> {
        debug!("RPC: get_mem_info");
        let content = std::fs::read_to_string("/proc/meminfo")
            .map_err(|e| ttrpc::Error::Others(format!("read /proc/meminfo: {e}")))?;

        let mut resp = GetMemInfoResponse::new();
        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }
            let val: u64 = parts[1].parse().unwrap_or(0);
            match parts[0] {
                "MemTotal:" => resp.mem_total_kb = val,
                "MemFree:" => resp.mem_free_kb = val,
                "MemAvailable:" => resp.mem_available_kb = val,
                "Buffers:" => resp.buffers_kb = val,
                "Cached:" => resp.cached_kb = val,
                "SwapTotal:" => resp.swap_total_kb = val,
                "SwapFree:" => resp.swap_free_kb = val,
                _ => {}
            }
        }
        Ok(resp)
    }

    async fn get_container_logs(
        &self,
        _ctx: &TtrpcContext,
        req: GetContainerLogsRequest,
    ) -> ttrpc::Result<GetContainerLogsResponse> {
        let id = &req.container_id;
        let offset = req.offset as usize;

        let log_buf = {
            let mgr = self.container_manager.lock().await;
            mgr.get_log_buffer(id)
        };

        let mut resp = GetContainerLogsResponse::new();
        if let Some(buf) = log_buf {
            let logs = buf.lock().await;
            // Return data from offset onwards
            if offset < logs.stdout.len() {
                resp.stdout = logs.stdout[offset..].to_vec();
            }
            if offset < logs.stderr.len() {
                resp.stderr = logs.stderr[offset..].to_vec();
            }
            resp.offset = std::cmp::max(logs.stdout.len(), logs.stderr.len()) as u64;
            resp.eof = logs.eof;
        } else {
            resp.eof = true;
        }
        Ok(resp)
    }

    async fn read_logs(
        &self,
        _ctx: &TtrpcContext,
        req: ReadLogsRequest,
    ) -> ttrpc::Result<ReadLogsResponse> {
        let max = if req.max_bytes == 0 {
            256 * 1024
        } else {
            req.max_bytes as usize
        };
        let log_buf = {
            let mgr = self.container_manager.lock().await;
            mgr.get_log_buffer(&req.container_id)
        };

        let mut resp = ReadLogsResponse::new();
        match log_buf {
            Some(buf) => {
                let read = buf
                    .lock()
                    .await
                    .read(req.stdout_offset, req.stderr_offset, max);
                resp.stdout = read.stdout;
                resp.stderr = read.stderr;
                resp.stdout_offset = read.stdout_next;
                resp.stderr_offset = read.stderr_next;
                resp.eof = read.eof;
                resp.stdout_dropped = read.stdout_dropped;
                resp.stderr_dropped = read.stderr_dropped;
            }
            // No such container (never started, or deleted): nothing more will come.
            None => resp.eof = true,
        }
        Ok(resp)
    }

    async fn configure_network(
        &self,
        _ctx: &TtrpcContext,
        req: ConfigureNetworkRequest,
    ) -> ttrpc::Result<ConfigureNetworkResponse> {
        let device = if req.device.is_empty() {
            "eth0"
        } else {
            &req.device
        };

        let mac = if req.mac.is_empty() {
            None
        } else {
            Some(req.mac.as_str())
        };

        info!(
            "RPC: configure_network dev={} mac={:?} ip={}/{} gw={}",
            device, mac, req.ip_address, req.prefix_len, req.gateway
        );

        // Only wait for sysfs device if not using MAC-based lookup.
        // MAC-based lookup retries internally via retry_find_by_mac.
        if mac.is_none() {
            wait_for_device(device)
                .await
                .map_err(|e| ttrpc::Error::Others(format!("device wait: {e}")))?;
        }

        let ip: std::net::Ipv4Addr = req
            .ip_address
            .parse()
            .map_err(|e| ttrpc::Error::Others(format!("invalid IP: {e}")))?;
        let gw: Option<std::net::Ipv4Addr> = if req.gateway.is_empty() {
            None
        } else {
            Some(
                req.gateway
                    .parse()
                    .map_err(|e| ttrpc::Error::Others(format!("invalid gateway: {e}")))?,
            )
        };

        let prefix_len: u8 = u8::try_from(req.prefix_len).map_err(|_| {
            ttrpc::Error::Others("invalid prefix_len: must be between 0 and 32".to_string())
        })?;
        if prefix_len > 32 {
            return Err(ttrpc::Error::Others(
                "invalid prefix_len: must be between 0 and 32".to_string(),
            ));
        }

        let device_owned = device.to_string();
        let mac_owned = mac.map(|m| m.to_string());
        tokio::task::spawn_blocking(move || {
            cloudhv_common::netlink::configure_interface(
                &device_owned,
                ip,
                prefix_len,
                gw,
                mac_owned.as_deref(),
            )
        })
        .await
        .map_err(|e| ttrpc::Error::Others(format!("spawn_blocking: {e}")))?
        .map_err(|e| ttrpc::Error::Others(format!("configure_interface: {e:#}")))?;

        info!(
            "configure_network: {} configured via netlink (mac={:?})",
            device, mac
        );
        Ok(ConfigureNetworkResponse::new())
    }

    async fn adopt_container(
        &self,
        _ctx: &TtrpcContext,
        req: AdoptContainerRequest,
    ) -> ttrpc::Result<AdoptContainerResponse> {
        info!(
            "RPC: adopt_container old={} new={}",
            req.old_container_id, req.new_container_id
        );
        let mut mgr = self.container_manager.lock().await;
        let pid = mgr
            .adopt(&req.old_container_id, &req.new_container_id)
            .map_err(|e| ttrpc::Error::Others(format!("adopt_container failed: {e:#}")))?;
        let mut resp = AdoptContainerResponse::new();
        resp.pid = pid;
        Ok(resp)
    }
}

/// Wait for a network device to appear in /sys/class/net/.
/// Polls every 50ms for up to 5 seconds.
async fn wait_for_device(device: &str) -> anyhow::Result<()> {
    let path = format!("/sys/class/net/{}", device);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    while tokio::time::Instant::now() < deadline {
        if std::path::Path::new(&path).exists() {
            info!("network device {} appeared", device);
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    anyhow::bail!(
        "timed out waiting for device {} to appear at {}",
        device,
        path
    )
}

struct HealthServiceHandler;

#[async_trait]
impl HealthService for HealthServiceHandler {
    async fn check(&self, _ctx: &TtrpcContext, _req: CheckRequest) -> ttrpc::Result<CheckResponse> {
        let mut resp = CheckResponse::new();
        resp.ready = true;
        resp.version = env!("CARGO_PKG_VERSION").to_string();
        Ok(resp)
    }
}

// --- Server ---

/// ttrpc server that listens on vsock and handles container lifecycle RPCs.
pub struct AgentServer {
    container_manager: Arc<tokio::sync::Mutex<ContainerManager>>,
    exec_manager: Arc<ExecManager>,
}

impl Default for AgentServer {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentServer {
    pub fn new() -> Self {
        Self {
            container_manager: Arc::new(tokio::sync::Mutex::new(ContainerManager::new())),
            exec_manager: Arc::new(ExecManager::new()),
        }
    }

    /// Start the ttrpc server and listen on vsock.
    pub async fn run(&self) -> Result<()> {
        info!(
            "starting agent ttrpc server on vsock port {}",
            AGENT_VSOCK_PORT
        );

        let vsock_fd = create_vsock_listener(AGENT_VSOCK_PORT)?;
        info!("vsock listener created on port {}", AGENT_VSOCK_PORT);

        let agent_service = Arc::new(AgentServiceHandler {
            container_manager: self.container_manager.clone(),
            exec_manager: self.exec_manager.clone(),
        });
        let health_service = Arc::new(HealthServiceHandler);

        let agent_svc = agent_ttrpc::create_agent_service(agent_service);
        let health_svc = agent_ttrpc::create_health_service(health_service);

        #[cfg(target_os = "linux")]
        let mut server = {
            unsafe {
                ttrpc::asynchronous::Server::new()
                    .add_vsock_listener(vsock_fd)
                    .expect("failed to add vsock listener")
            }
            .register_service(agent_svc)
            .register_service(health_svc)
        };

        #[cfg(not(target_os = "linux"))]
        let mut server = {
            let _ = vsock_fd;
            ttrpc::asynchronous::Server::new()
                .register_service(agent_svc)
                .register_service(health_svc)
        };

        server
            .start()
            .await
            .map_err(|e| anyhow::anyhow!("failed to start ttrpc server: {e}"))?;

        info!("ttrpc server started, accepting connections");

        // Children that nobody waits for (processes a container's runtime left behind) are reaped; see reaper.rs.
        let reaper_mgr = self.container_manager.clone();
        let reaper_execs = self.exec_manager.clone();
        tokio::spawn(async move {
            let mut seen = std::collections::HashSet::new();
            loop {
                tokio::time::sleep(crate::reaper::SCAN_INTERVAL).await;
                let mut tracked = reaper_mgr.lock().await.tracked_pids();
                tracked.extend(reaper_execs.tracked_pids().await);
                seen = crate::reaper::reap_stale_zombies(&seen, &tracked);
            }
        });

        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;

        tokio::select! {
            _ = sigterm.recv() => info!("received SIGTERM, shutting down"),
            _ = sigint.recv() => info!("received SIGINT, shutting down"),
        }

        // Signal all running containers to exit cleanly before stopping
        // the server. This ensures the guest kernel can shut down quickly
        // without waiting for orphaned processes.
        self.container_manager.lock().await.signal_all(15).await;

        server
            .shutdown()
            .await
            .map_err(|e| anyhow::anyhow!("failed to shutdown ttrpc server: {e}"))?;
        info!("agent server stopped");
        Ok(())
    }
}

/// Create a vsock listener socket bound to the given port.
/// Only available on Linux (AF_VSOCK).
#[cfg(target_os = "linux")]
fn create_vsock_listener(port: u32) -> Result<i32> {
    use libc::{bind, listen, sockaddr_vm, socket, AF_VSOCK, SOCK_STREAM, VMADDR_CID_ANY};
    use std::mem;

    unsafe {
        let fd = socket(AF_VSOCK, SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            anyhow::bail!(
                "failed to create vsock socket: {}",
                std::io::Error::last_os_error()
            );
        }

        let mut addr: sockaddr_vm = mem::zeroed();
        addr.svm_family = AF_VSOCK as libc::sa_family_t;
        addr.svm_cid = VMADDR_CID_ANY;
        addr.svm_port = port;

        let addr_ptr = &addr as *const sockaddr_vm as *const libc::sockaddr;
        let addr_len = mem::size_of::<sockaddr_vm>() as libc::socklen_t;

        if bind(fd, addr_ptr, addr_len) < 0 {
            libc::close(fd);
            anyhow::bail!(
                "failed to bind vsock port {}: {}",
                port,
                std::io::Error::last_os_error()
            );
        }

        if listen(fd, 128) < 0 {
            libc::close(fd);
            anyhow::bail!(
                "failed to listen on vsock port {}: {}",
                port,
                std::io::Error::last_os_error()
            );
        }

        debug!("vsock listener ready: fd={}, port={}", fd, port);
        Ok(fd)
    }
}

#[cfg(not(target_os = "linux"))]
fn create_vsock_listener(_port: u32) -> Result<i32> {
    anyhow::bail!("vsock is only supported on Linux (AF_VSOCK)")
}
