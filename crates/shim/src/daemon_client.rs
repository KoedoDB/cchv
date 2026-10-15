//! Async client for the sandbox daemon.
//!
//! The shim communicates with the daemon over a Unix socket using a
//! JSON-line protocol. All methods are async and use tokio::net::UnixStream.

use std::time::Duration;

use anyhow::{Context, Result};
use log::info;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// The longest answer read from the daemon. ReadLogs answers (a container's output, base64) are the biggest, about 1.4 MB; a daemon,
/// or something in its place, that sends more is not read on until the shim's memory is gone.
const MAX_RESPONSE: u64 = 16 * 1024 * 1024;

/// Parameters for acquiring a sandbox VM from the daemon.
pub struct AcquireRequest<'a> {
    pub netns: &'a str,
    pub image_key: &'a str,
    pub erofs_path: &'a str,
    pub container_id: &'a str,
    pub config_json: &'a [u8],
    /// The VM the pod needs (0: the daemon's default). The daemon serves the default size from its pool and boots any other size.
    pub memory_mb: u64,
    pub vcpus: u32,
    /// What the pod's runtime bind-mounts into the container (see `volumes.rs`).
    pub volumes: &'a [serde_json::Value],
}

/// Response from AcquireSandbox.
#[derive(Debug)]
pub struct AcquiredVm {
    pub vm_id: String,
    pub ch_pid: u32,
    pub from_snapshot: bool,
    pub container_pid: u32,
    pub tap_name: String,
}

/// Async client for the sandbox daemon's Unix socket API.
pub struct DaemonClient {
    socket_path: String,
    /// What an ordinary request may take, and what one that boots or hot-plugs a VM may take.
    rpc_timeout: Duration,
    acquire_timeout: Duration,
}

impl DaemonClient {
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
            rpc_timeout: Duration::from_secs(30),
            acquire_timeout: Duration::from_secs(120),
        }
    }

    pub fn with_timeouts(mut self, rpc_secs: u64, acquire_secs: u64) -> Self {
        self.rpc_timeout = Duration::from_secs(rpc_secs.max(1));
        self.acquire_timeout = Duration::from_secs(acquire_secs.max(1));
        self
    }

    /// Acquire a pre-booted VM from the daemon.
    pub async fn acquire_sandbox(&self, params: &AcquireRequest<'_>) -> Result<AcquiredVm> {
        use base64::Engine;
        let config_b64 = base64::engine::general_purpose::STANDARD.encode(params.config_json);

        let resp = self
            .rpc_long(&serde_json::json!({
                "method": "AcquireSandbox",
                "netns": params.netns,
                "image_key": params.image_key,
                "erofs_path": params.erofs_path,
                "container_id": params.container_id,
                "config_json": config_b64,
                "memory_mb": params.memory_mb,
                "vcpus": params.vcpus,
                "volumes": params.volumes,
            }))
            .await?;

        Self::check_error(&resp, "AcquireSandbox")?;

        Ok(AcquiredVm {
            vm_id: resp["vm_id"].as_str().context("missing vm_id")?.to_string(),
            ch_pid: resp["ch_pid"].as_u64().context("missing ch_pid")? as u32,
            from_snapshot: resp["from_snapshot"].as_bool().unwrap_or(false),
            container_pid: resp["container_pid"].as_u64().unwrap_or(0) as u32,
            tap_name: resp["tap_name"].as_str().unwrap_or("").to_string(),
        })
    }

    /// Release a VM back to the daemon for destruction.
    pub async fn release_sandbox(&self, vm_id: &str) -> Result<()> {
        let resp = self
            .rpc(&serde_json::json!({
                "method": "ReleaseSandbox",
                "vm_id": vm_id,
            }))
            .await?;
        Self::check_error(&resp, "ReleaseSandbox")?;
        info!("released VM {} to daemon", vm_id);
        Ok(())
    }

    /// Add a container to an existing VM (multi-container pod).
    pub async fn add_container(
        &self,
        vm_id: &str,
        erofs_path: &str,
        container_id: &str,
        config_json: &[u8],
        volumes: &[serde_json::Value],
    ) -> Result<u32> {
        use base64::Engine;
        let config_b64 = base64::engine::general_purpose::STANDARD.encode(config_json);

        let resp = self
            .rpc_long(&serde_json::json!({
                "method": "AddContainer",
                "vm_id": vm_id,
                "erofs_path": erofs_path,
                "container_id": container_id,
                "config_json": config_b64,
                "volumes": volumes,
            }))
            .await?;
        Self::check_error(&resp, "AddContainer")?;
        Ok(resp["container_pid"].as_u64().unwrap_or(0) as u32)
    }

    /// Send a signal to a container inside a VM.
    pub async fn kill_container(&self, vm_id: &str, container_id: &str, signal: u32) -> Result<()> {
        let resp = self
            .rpc(&serde_json::json!({
                "method": "KillContainer",
                "vm_id": vm_id,
                "container_id": container_id,
                "signal": signal,
            }))
            .await?;
        Self::check_error(&resp, "KillContainer")?;
        Ok(())
    }

    /// Wait for a container to exit. Blocks until the container terminates.
    /// Returns (exit_code, exited_at).
    pub async fn wait_container(&self, vm_id: &str, container_id: &str) -> Result<(u32, String)> {
        // WaitContainer is long-lived — no timeout on the read since the
        // container may run for hours. The daemon holds the connection open
        // and responds only when the container exits.
        let resp = self
            .rpc_no_timeout(&serde_json::json!({
                "method": "WaitContainer",
                "vm_id": vm_id,
                "container_id": container_id,
            }))
            .await?;
        Self::check_error(&resp, "WaitContainer")?;
        let exit_code = resp["exit_status"].as_u64().unwrap_or(137) as u32;
        let exited_at = resp["exited_at"].as_str().unwrap_or("").to_string();
        Ok((exit_code, exited_at))
    }

    /// Read what a container has written since the given offsets (see `ReadLogs` in proto/agent.proto). The caller's offsets
    /// acknowledge what it has taken, so the guest drops it.
    pub async fn read_logs(
        &self,
        vm_id: &str,
        container_id: &str,
        stdout_offset: u64,
        stderr_offset: u64,
    ) -> Result<LogChunk> {
        use base64::Engine;
        let resp = self
            .rpc(&serde_json::json!({
                "method": "ReadLogs",
                "vm_id": vm_id,
                "container_id": container_id,
                "stdout_offset": stdout_offset,
                "stderr_offset": stderr_offset,
            }))
            .await?;
        Self::check_error(&resp, "ReadLogs")?;
        let b64 = base64::engine::general_purpose::STANDARD;
        let decode = |key: &str| -> Result<Vec<u8>> {
            b64.decode(resp[key].as_str().unwrap_or(""))
                .with_context(|| format!("decode {key}"))
        };
        Ok(LogChunk {
            stdout: decode("stdout")?,
            stderr: decode("stderr")?,
            stdout_offset: resp["stdout_offset"].as_u64().unwrap_or(stdout_offset),
            stderr_offset: resp["stderr_offset"].as_u64().unwrap_or(stderr_offset),
            eof: resp["eof"].as_bool().unwrap_or(false),
        })
    }

    /// Delete a stopped container inside a VM.
    pub async fn delete_container(&self, vm_id: &str, container_id: &str) -> Result<()> {
        let resp = self
            .rpc(&serde_json::json!({
                "method": "DeleteContainer",
                "vm_id": vm_id,
                "container_id": container_id,
            }))
            .await?;
        Self::check_error(&resp, "DeleteContainer")?;
        Ok(())
    }

    /// Send a JSON-line RPC; it must be answered within the ordinary timeout.
    async fn rpc(&self, request: &serde_json::Value) -> Result<serde_json::Value> {
        self.rpc_with(request, Some(self.rpc_timeout)).await
    }

    /// The same for a request that boots a VM or hot-plugs a disk.
    async fn rpc_long(&self, request: &serde_json::Value) -> Result<serde_json::Value> {
        self.rpc_with(request, Some(self.acquire_timeout)).await
    }

    /// Send a JSON-line RPC without a timeout (for long-lived RPCs). The connection ending is still an end.
    async fn rpc_no_timeout(&self, request: &serde_json::Value) -> Result<serde_json::Value> {
        self.rpc_with(request, None).await
    }

    /// Connecting, writing the request and reading the answer, all within `timeout`.
    async fn rpc_with(
        &self,
        request: &serde_json::Value,
        timeout: Option<Duration>,
    ) -> Result<serde_json::Value> {
        let exchange = async {
            let stream = UnixStream::connect(&self.socket_path)
                .await
                .with_context(|| format!("connect to daemon: {}", self.socket_path))?;

            let mut msg = serde_json::to_string(request)?;
            msg.push('\n');

            let (reader, mut writer) = stream.into_split();
            writer.write_all(msg.as_bytes()).await.context("write to the daemon")?;

            let mut buf_reader = BufReader::new(reader.take(MAX_RESPONSE));
            let mut line = String::new();
            let n = buf_reader
                .read_line(&mut line)
                .await
                .context("read daemon response")?;
            if n == 0 {
                anyhow::bail!("the daemon closed the connection without an answer");
            }
            if !line.ends_with('\n') && n as u64 >= MAX_RESPONSE {
                anyhow::bail!("the daemon's answer is longer than {MAX_RESPONSE} bytes");
            }
            serde_json::from_str(line.trim()).context("parse daemon response")
        };
        match timeout {
            Some(t) => tokio::time::timeout(t, exchange)
                .await
                .map_err(|_| anyhow::anyhow!("daemon RPC timed out after {}s", t.as_secs()))?,
            None => exchange.await,
        }
    }

    fn check_error(resp: &serde_json::Value, method: &str) -> Result<()> {
        if let Some(err) = resp.get("error").and_then(|e| e.as_str()) {
            anyhow::bail!("daemon {method}: {err}");
        }
        Ok(())
    }
}

/// New output of a container, from `DaemonClient::read_logs`.
#[derive(Debug)]
pub struct LogChunk {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Where the next call starts.
    pub stdout_offset: u64,
    pub stderr_offset: u64,
    /// The container has exited and everything it wrote has been returned.
    pub eof: bool,
}
