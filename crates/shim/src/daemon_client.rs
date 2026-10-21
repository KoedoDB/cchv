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

/// How long a call that must not be lost (waiting for a container, reading its output) goes on trying while the daemon cannot be reached.
/// The daemon is replaced without stopping the pods' VMs; the new one is listening again after it has taken them over and filled its pool
/// (some tens of seconds). A pod's container runs on meanwhile, and its exit must not be taken for the daemon's absence.
const RECONNECT_GRACE: Duration = Duration::from_secs(300);
const RECONNECT_PAUSE: Duration = Duration::from_secs(1);
/// A wait that was under way this long had a connection to the daemon: the daemon was there until the connection broke, so what follows is a
/// new absence, with its own grace (not the end of an earlier one, which a pod that has seen the daemon replaced once would otherwise give up
/// at on the second).
const HEALTHY_AFTER: Duration = Duration::from_secs(5);

/// The daemon could not be spoken to (no connection, or the connection broke before an answer): not "the daemon answered with an error".
#[derive(Debug)]
struct Unreachable;

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the daemon cannot be reached")
    }
}

impl std::error::Error for Unreachable {}

/// Whether the error is that the daemon could not be spoken to (it may be restarting), as opposed to an error it answered with.
pub fn is_unreachable(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Unreachable>().is_some()
        || e.chain().any(|c| c.downcast_ref::<Unreachable>().is_some())
}

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
    /// Whether the container's process has a terminal (`tty: true`).
    pub terminal: bool,
}

/// Response from AcquireSandbox.
#[derive(Debug)]
pub struct AcquiredVm {
    pub vm_id: String,
    pub ch_pid: u32,
    /// The VM's cgroup directory in the daemon's hierarchy (all of its processes are in it), when the daemon uses cgroups.
    pub ch_cgroup: Option<String>,
    pub from_snapshot: bool,
    pub container_pid: u32,
    pub tap_name: String,
}

/// Async client for the sandbox daemon's Unix socket API.
#[derive(Clone)]
pub struct DaemonClient {
    socket_path: String,
    /// What an ordinary request may take, and what one that boots or hot-plugs a VM may take.
    rpc_timeout: Duration,
    acquire_timeout: Duration,
    /// See `RECONNECT_GRACE`.
    reconnect_grace: Duration,
    /// See `HEALTHY_AFTER`.
    healthy_after: Duration,
}

impl DaemonClient {
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
            rpc_timeout: Duration::from_secs(30),
            acquire_timeout: Duration::from_secs(120),
            reconnect_grace: RECONNECT_GRACE,
            healthy_after: HEALTHY_AFTER,
        }
    }

    /// How long `wait_container` and `read_logs` keep trying while the daemon cannot be reached (the tests shorten it).
    #[cfg(test)]
    pub fn with_reconnect_grace(mut self, grace: Duration) -> Self {
        self.reconnect_grace = grace;
        self
    }

    /// How long a wait must have been under way for the failure that ends it to start a new absence (the tests shorten it).
    #[cfg(test)]
    pub fn with_healthy_after(mut self, after: Duration) -> Self {
        self.healthy_after = after;
        self
    }

    /// Send a request that is safe to repeat, again and again while the daemon cannot be reached, for up to the grace. An answer (of any
    /// kind) ends it. `call` makes one try.
    async fn rpc_patiently(&self, request: &serde_json::Value, long: bool) -> Result<serde_json::Value> {
        let mut down_since: Option<std::time::Instant> = None;
        let mut tries = 0u32;
        loop {
            let try_started = std::time::Instant::now();
            let tried = if long {
                self.rpc_no_timeout(request).await
            } else {
                self.rpc(request).await
            };
            match tried {
                Err(e) if is_unreachable(&e) => {
                    // A wait that had been under way for a while had its connection: this is a new absence, not the old one going on.
                    if long && try_started.elapsed() >= self.healthy_after {
                        down_since = None;
                        tries = 0;
                    }
                    let since = *down_since.get_or_insert_with(std::time::Instant::now);
                    if since.elapsed() >= self.reconnect_grace {
                        log::warn!("the daemon is still away after {:?}: giving up ({e:#})", since.elapsed());
                        return Err(e);
                    }
                    if tries == 0 {
                        log::info!("the daemon cannot be reached, trying again for up to {:?}: {e:#}", self.reconnect_grace);
                    }
                    tries += 1;
                    tokio::time::sleep(RECONNECT_PAUSE).await;
                }
                other => {
                    if tries > 0 {
                        log::info!("the daemon answered again after {tries} tries");
                    }
                    return other;
                }
            }
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
                "terminal": params.terminal,
            }))
            .await?;

        Self::check_error(&resp, "AcquireSandbox")?;

        Ok(AcquiredVm {
            vm_id: resp["vm_id"].as_str().context("missing vm_id")?.to_string(),
            ch_pid: resp["ch_pid"].as_u64().context("missing ch_pid")? as u32,
            ch_cgroup: resp["ch_cgroup"].as_str().map(String::from),
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
        terminal: bool,
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
                "terminal": terminal,
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
        // A daemon that is replaced closes this connection and, a little later, listens again, with the VM and the container still there:
        // ask again (the guest's agent answers a second wait as it does the first).
        let resp = self
            .rpc_patiently(
                &serde_json::json!({
                    "method": "WaitContainer",
                    "vm_id": vm_id,
                    "container_id": container_id,
                }),
                true,
            )
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
        // The offsets are the caller's, so a call that is repeated returns what the first would have.
        let resp = self
            .rpc_patiently(
                &serde_json::json!({
                    "method": "ReadLogs",
                    "vm_id": vm_id,
                    "container_id": container_id,
                    "stdout_offset": stdout_offset,
                    "stderr_offset": stderr_offset,
                }),
                false,
            )
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

    // ── exec processes (see the contract in docs/shim.md) ────────────────────

    /// Create an exec process in a container of the VM (not started yet). `process_json` is the OCI `Process`.
    pub async fn exec_create(
        &self,
        vm_id: &str,
        container_id: &str,
        exec_id: &str,
        process_json: &[u8],
        terminal: bool,
    ) -> Result<()> {
        use base64::Engine;
        let resp = self
            .rpc(&serde_json::json!({
                "method": "ExecCreate",
                "vm_id": vm_id,
                "container_id": container_id,
                "exec_id": exec_id,
                "process_json": base64::engine::general_purpose::STANDARD.encode(process_json),
                "terminal": terminal,
            }))
            .await?;
        Self::check_error(&resp, "ExecCreate")
    }

    /// Start the exec process; returns its pid in the guest.
    pub async fn exec_start(&self, vm_id: &str, container_id: &str, exec_id: &str) -> Result<u32> {
        let resp = self
            .rpc(&Self::exec_ids("ExecStart", vm_id, container_id, exec_id))
            .await?;
        Self::check_error(&resp, "ExecStart")?;
        Ok(resp["pid"].as_u64().unwrap_or(0) as u32)
    }

    pub async fn exec_kill(
        &self,
        vm_id: &str,
        container_id: &str,
        exec_id: &str,
        signal: u32,
    ) -> Result<()> {
        let mut req = Self::exec_ids("ExecKill", vm_id, container_id, exec_id);
        req["signal"] = signal.into();
        let resp = self.rpc(&req).await?;
        Self::check_error(&resp, "ExecKill")
    }

    /// Wait for the exec process to exit (no timeout: it may run for hours). Returns the exit status and when it exited.
    pub async fn exec_wait(
        &self,
        vm_id: &str,
        container_id: &str,
        exec_id: &str,
    ) -> Result<(u32, chrono::DateTime<chrono::Utc>)> {
        let resp = self
            .rpc_no_timeout(&Self::exec_ids("ExecWait", vm_id, container_id, exec_id))
            .await?;
        Self::check_error(&resp, "ExecWait")?;
        let status = resp["exit_status"].as_u64().unwrap_or(255) as u32;
        let at = resp["exited_at_unix_nanos"]
            .as_i64()
            .filter(|n| *n > 0)
            .map(chrono::DateTime::from_timestamp_nanos)
            .unwrap_or_else(chrono::Utc::now);
        Ok((status, at))
    }

    pub async fn exec_delete(&self, vm_id: &str, container_id: &str, exec_id: &str) -> Result<()> {
        let resp = self
            .rpc(&Self::exec_ids("ExecDelete", vm_id, container_id, exec_id))
            .await?;
        Self::check_error(&resp, "ExecDelete")
    }

    /// Write to the exec process's standard input; returns how many bytes were taken.
    pub async fn exec_write(
        &self,
        vm_id: &str,
        container_id: &str,
        exec_id: &str,
        data: &[u8],
    ) -> Result<usize> {
        use base64::Engine;
        let mut req = Self::exec_ids("ExecWrite", vm_id, container_id, exec_id);
        req["data"] = base64::engine::general_purpose::STANDARD
            .encode(data)
            .into();
        let resp = self.rpc(&req).await?;
        Self::check_error(&resp, "ExecWrite")?;
        Ok(resp["written"].as_u64().unwrap_or(data.len() as u64) as usize)
    }

    pub async fn exec_close_stdin(
        &self,
        vm_id: &str,
        container_id: &str,
        exec_id: &str,
    ) -> Result<()> {
        let resp = self
            .rpc(&Self::exec_ids(
                "ExecCloseStdin",
                vm_id,
                container_id,
                exec_id,
            ))
            .await?;
        Self::check_error(&resp, "ExecCloseStdin")
    }

    pub async fn exec_resize(
        &self,
        vm_id: &str,
        container_id: &str,
        exec_id: &str,
        width: u32,
        height: u32,
    ) -> Result<()> {
        let mut req = Self::exec_ids("ExecResize", vm_id, container_id, exec_id);
        req["width"] = width.into();
        req["height"] = height.into();
        let resp = self.rpc(&req).await?;
        Self::check_error(&resp, "ExecResize")
    }

    /// Read what the exec process has written since the offsets (cumulative byte positions, as for `ReadLogs`; the offsets
    /// acknowledge what the caller has taken). At most `max` bytes of each stream. With a terminal, everything is on stdout.
    pub async fn exec_read(
        &self,
        vm_id: &str,
        container_id: &str,
        exec_id: &str,
        stdout_offset: u64,
        stderr_offset: u64,
        max: usize,
    ) -> Result<ExecChunk> {
        use base64::Engine;
        let mut req = Self::exec_ids("ExecRead", vm_id, container_id, exec_id);
        req["stdout_offset"] = stdout_offset.into();
        req["stderr_offset"] = stderr_offset.into();
        req["max"] = max.into();
        let resp = self.rpc(&req).await?;
        Self::check_error(&resp, "ExecRead")?;
        let b64 = base64::engine::general_purpose::STANDARD;
        let decode = |key: &str| -> Result<Vec<u8>> {
            b64.decode(resp[key].as_str().unwrap_or(""))
                .with_context(|| format!("decode {key}"))
        };
        Ok(ExecChunk {
            stdout: decode("stdout")?,
            stderr: decode("stderr")?,
            stdout_offset: resp["stdout_offset"].as_u64().unwrap_or(stdout_offset),
            stderr_offset: resp["stderr_offset"].as_u64().unwrap_or(stderr_offset),
            eof: resp["eof"].as_bool().unwrap_or(false),
            dropped: resp["dropped"].as_u64().unwrap_or(0),
        })
    }

    fn exec_ids(method: &str, vm_id: &str, container_id: &str, exec_id: &str) -> serde_json::Value {
        serde_json::json!({
            "method": method,
            "vm_id": vm_id,
            "container_id": container_id,
            "exec_id": exec_id,
        })
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
            writer
                .write_all(msg.as_bytes())
                .await
                .context("write to the daemon")?;

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
        let done = match timeout {
            Some(t) => tokio::time::timeout(t, exchange)
                .await
                .map_err(|_| anyhow::anyhow!("daemon RPC timed out after {}s", t.as_secs()))
                .and_then(|r| r),
            None => exchange.await,
        };
        // Whatever went wrong here, the daemon gave no answer.
        done.map_err(|e| e.context(Unreachable))
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

/// New output of an exec process, from `DaemonClient::exec_read`.
#[derive(Debug)]
pub struct ExecChunk {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Where the next call starts.
    pub stdout_offset: u64,
    pub stderr_offset: u64,
    /// The process has exited and everything it wrote has been returned.
    pub eof: bool,
    /// Bytes the guest had to drop because nobody read them in time.
    pub dropped: u64,
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    use super::*;

    /// A daemon that answers one request with `answer` and returns the request it got.
    async fn once(
        answer: Value,
    ) -> (
        DaemonClient,
        tokio::task::JoinHandle<Value>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let h = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (rd, mut wr) = stream.into_split();
            let mut line = String::new();
            BufReader::new(rd).read_line(&mut line).await.unwrap();
            wr.write_all(format!("{answer}\n").as_bytes())
                .await
                .unwrap();
            serde_json::from_str(line.trim()).unwrap()
        });
        (DaemonClient::new(sock.to_str().unwrap()), h, dir)
    }

    /// A daemon that is not there for `down`, then answers every request with `answer`.
    async fn comes_back(
        down: Duration,
        answer: Value,
    ) -> (DaemonClient, tokio::task::JoinHandle<()>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let path = sock.clone();
        let h = tokio::spawn(async move {
            tokio::time::sleep(down).await;
            let listener = UnixListener::bind(&path).unwrap();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (rd, mut wr) = stream.into_split();
                let mut line = String::new();
                BufReader::new(rd).read_line(&mut line).await.unwrap();
                wr.write_all(format!("{answer}\n").as_bytes()).await.unwrap();
            }
        });
        (DaemonClient::new(sock.to_str().unwrap()), h, dir)
    }

    #[tokio::test]
    async fn waiting_for_a_container_outlasts_a_daemon_that_is_replaced() {
        let (c, h, _d) = comes_back(
            Duration::from_millis(1500),
            json!({"exit_status": 3, "exited_at": "t"}),
        )
        .await;
        let (code, at) = c.wait_container("v1", "c1").await.unwrap();
        assert_eq!((code, at.as_str()), (3, "t"));
        h.abort();
    }

    #[tokio::test]
    async fn waiting_goes_on_when_the_daemon_dies_with_the_request_in_hand() {
        // What a killed daemon does to a wait that is under way: the connection is closed with no answer. The next try is answered.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let h = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (rd, wr) = stream.into_split();
            let mut line = String::new();
            BufReader::new(rd).read_line(&mut line).await.unwrap();
            drop(wr); // closed with no answer
            let (stream, _) = listener.accept().await.unwrap();
            let (rd, mut wr) = stream.into_split();
            let mut line = String::new();
            BufReader::new(rd).read_line(&mut line).await.unwrap();
            wr.write_all(b"{\"exit_status\": 5, \"exited_at\": \"t\"}\n").await.unwrap();
        });
        let c = DaemonClient::new(sock.to_str().unwrap());
        let (code, _) = c.wait_container("v1", "c1").await.expect("the wait is repeated");
        assert_eq!(code, 5);
        h.await.unwrap();
    }

    #[tokio::test]
    async fn a_second_absence_of_the_daemon_gets_its_own_grace() {
        // The daemon is away for 0.5 s, comes back and the wait is under way for 0.6 s, then the daemon is killed again and is away for 0.7 s.
        // Each absence is shorter than the grace (1 s), together they are longer: the wait must go on.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let path = sock.clone();
        let h = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let listener = UnixListener::bind(&path).unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            let (rd, wr) = stream.into_split();
            let mut line = String::new();
            BufReader::new(rd).read_line(&mut line).await.unwrap();
            tokio::time::sleep(Duration::from_millis(600)).await;
            drop(wr); // killed again
            drop(listener);
            let _ = std::fs::remove_file(&path);
            tokio::time::sleep(Duration::from_millis(700)).await;
            let listener = UnixListener::bind(&path).unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            let (rd, mut wr) = stream.into_split();
            let mut line = String::new();
            BufReader::new(rd).read_line(&mut line).await.unwrap();
            wr.write_all(b"{\"exit_status\": 9, \"exited_at\": \"t\"}\n").await.unwrap();
        });
        let c = DaemonClient::new(sock.to_str().unwrap())
            .with_reconnect_grace(Duration::from_millis(1000))
            .with_healthy_after(Duration::from_millis(300));
        let (code, _) = c.wait_container("v1", "c1").await.expect("the second absence is short");
        assert_eq!(code, 9);
        h.await.unwrap();
    }

    #[tokio::test]
    async fn waiting_gives_up_when_the_daemon_stays_away_longer_than_the_grace() {
        let c = DaemonClient::new("/nonexistent/daemon.sock")
            .with_reconnect_grace(Duration::from_millis(1200));
        let t0 = std::time::Instant::now();
        let e = c.wait_container("v1", "c1").await.unwrap_err();
        assert!(is_unreachable(&e), "{e:#}");
        assert!(t0.elapsed() >= Duration::from_millis(1200));
    }

    #[tokio::test]
    async fn an_error_the_daemon_answers_with_is_not_retried() {
        let (c, h, _d) = once(json!({"exit_status": 137, "error": "no agent for VM v1"})).await;
        let e = c.wait_container("v1", "c1").await.unwrap_err();
        assert!(!is_unreachable(&e), "{e:#}");
        assert!(format!("{e:#}").contains("no agent for VM"));
        h.await.unwrap();
    }

    #[tokio::test]
    async fn acquire_sandbox_says_whether_the_container_has_a_terminal() {
        for terminal in [true, false] {
            let (c, h, _d) = once(json!({"vm_id": "v1", "ch_pid": 7, "from_snapshot": false, "container_pid": 3, "tap_name": "t"})).await;
            c.acquire_sandbox(&AcquireRequest {
                netns: "",
                image_key: "k",
                erofs_path: "/run/cloudhv/erofs-cache/a.erofs",
                container_id: "c1",
                config_json: b"{}",
                memory_mb: 0,
                vcpus: 0,
                volumes: &[],
                terminal,
            })
            .await
            .unwrap();
            let req = h.await.unwrap();
            assert_eq!(req["method"], "AcquireSandbox");
            assert_eq!(req["terminal"], json!(terminal));
        }
    }

    #[tokio::test]
    async fn add_container_says_whether_the_container_has_a_terminal() {
        for terminal in [true, false] {
            let (c, h, _d) = once(json!({"container_pid": 9})).await;
            let pid = c
                .add_container(
                    "v1",
                    "/run/cloudhv/erofs-cache/a.erofs",
                    "c2",
                    b"{}",
                    &[],
                    terminal,
                )
                .await
                .unwrap();
            assert_eq!(pid, 9);
            let req = h.await.unwrap();
            assert_eq!(req["method"], "AddContainer");
            assert_eq!(req["terminal"], json!(terminal));
        }
    }
}
