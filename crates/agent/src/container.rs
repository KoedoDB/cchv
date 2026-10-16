use std::collections::{HashMap, HashSet};
#[cfg(target_os = "linux")]
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::{error, info, warn};
use tokio::process::Command;
use tokio::sync::watch;

use crate::disks::{vd_name, Disks};
use crate::spec;
use crate::volumes::{self, VolumeFile};

/// Exit status reported through the watch channel when a container stops.
#[derive(Debug, Clone)]
pub struct ExitStatus {
    pub code: i32,
    pub exited_at: String,
}

/// Volume information passed from the shim to the agent.
pub struct VolumeInfo {
    pub destination: String,
    pub source: String,
    pub readonly: bool,
    pub is_block: bool,
    pub fs_type: String,
    /// Mount options the host asked for; only a few are honoured (see spec::volume_options).
    pub options: Vec<String>,
    /// For block volumes: the virtio-blk serial of the disk (empty: the one new disk that appears).
    pub serial: String,
    /// Inline file contents for filesystem volumes (ConfigMap, Secret).
    /// When non-empty, files are written to tmpfs instead of read from disk.
    pub inline_files: Vec<InlineFileInfo>,
}

/// A file delivered inline via the CreateContainer RPC.
pub struct InlineFileInfo {
    pub path: String,
    pub content: Vec<u8>,
    pub mode: u32,
}

/// The most layers one container may have. Each is a disk of the VM.
const MAX_EROFS_LAYERS: u32 = 120;

/// How long a container's disks and device nodes have, all together, to appear. A container whose disks do not come fails to start.
const DISK_WAIT: Duration = Duration::from_secs(20);

/// File systems a block volume may have.
const BLOCK_FS_TYPES: &[&str] = &["ext4", "xfs"];

/// The file system of a disk-backed emptyDir. Nothing else is mounted at the shared volume root: the host makes it (`mke2fs`).
const SHARED_BLOCK_FS: &str = "ext4";

/// The shared block volumes already mounted in this VM (by name). The lock is held while one is made, so that two containers of a pod
/// that start together mount the disk once.
fn mounted_block_volumes() -> &'static tokio::sync::Mutex<HashSet<String>> {
    static M: std::sync::OnceLock<tokio::sync::Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    M.get_or_init(|| tokio::sync::Mutex::new(HashSet::new()))
}

/// What a disk-backed volume needs of the request, or why it is refused: a serial (the disk is found by it, never by position) and
/// the one file system the host makes.
fn check_shared_block(serial: &str, fs_type: &str) -> Result<()> {
    if serial.is_empty() {
        anyhow::bail!("a disk-backed volume needs the serial of its disk");
    }
    if fs_type != SHARED_BLOCK_FS {
        anyhow::bail!("a disk-backed volume has file system {SHARED_BLOCK_FS}, not {fs_type:?}");
    }
    Ok(())
}

/// The directory `/run/cloudhv-vols/<name>` with the disk of serial `serial` mounted on it (made the first time; later calls only return
/// the directory). The mount is `nosuid,nodev`; the root of the file system is made world-writable like a kubelet emptyDir.
#[cfg(target_os = "linux")]
async fn shared_block_volume(
    disks: &Disks,
    name: &str,
    serial: &str,
    fs_type: &str,
    deadline: Instant,
) -> Result<PathBuf> {
    use nix::mount::{mount, MsFlags};
    use std::os::unix::fs::PermissionsExt;

    check_shared_block(serial, fs_type)?;
    let dir = Path::new(volumes::VOLUME_ROOT).join(name);
    let mut mounted = mounted_block_volumes().lock().await;
    if mounted.contains(name) {
        return Ok(dir);
    }
    let dev_name = disks.claim_by_serial(serial, deadline).await?;
    let dev = format!("/dev/{dev_name}");
    if !Path::new(&dev).exists() {
        wait_for_dev_inotify(&dev, deadline).await?;
    }
    std::fs::create_dir_all(volumes::VOLUME_ROOT)
        .with_context(|| format!("create {}", volumes::VOLUME_ROOT))?;
    // A directory that is already there is not ours to mount over (a file volume of the same name): the name is the host's hash.
    std::fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    let mount_deadline = Instant::now() + Duration::from_millis(2000);
    loop {
        match mount(
            Some(dev.as_str()),
            &dir,
            Some(SHARED_BLOCK_FS),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOATIME,
            None::<&str>,
        ) {
            Ok(()) => break,
            Err(_e) if Instant::now() < mount_deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
            Err(e) => anyhow::bail!("mount {dev} at {}: {e}", dir.display()),
        }
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777))
        .with_context(|| format!("chmod {}", dir.display()))?;
    mounted.insert(name.to_string());
    Ok(dir)
}

#[cfg(not(target_os = "linux"))]
async fn shared_block_volume(
    _: &Disks,
    _: &str,
    serial: &str,
    fs_type: &str,
    _: Instant,
) -> Result<PathBuf> {
    check_shared_block(serial, fs_type)?;
    anyhow::bail!("disk-backed volumes need Linux")
}

/// Tracks the state of a container managed by the agent.
#[derive(Debug)]
struct Container {
    pid: Option<u32>,
    exit_code: Option<i32>,
    state: ContainerState,
    /// The directories of the volumes it uses, so that deleting it can release them.
    volumes: Vec<VolumeUse>,
    /// Whether the container's process has a terminal, and its master once the container has started (`crun run --console-socket`).
    terminal: bool,
    master: Option<Arc<crate::exec::Master>>,
}

/// A volume directory a container uses.
#[derive(Debug, Clone)]
pub struct VolumeUse {
    root: PathBuf,
    name: String,
    /// It holds copies of the host's files (a ConfigMap, Secret or token) and is removed with its last user. An emptyDir is not.
    has_files: bool,
}

impl VolumeUse {
    fn key(&self) -> String {
        self.root.join(&self.name).to_string_lossy().to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ContainerState {
    Created,
    Running,
    Stopped,
}

/// The most output the agent keeps for each stream of a container that nobody reads. The guest has little memory (128 MiB), and
/// a container that prints without end would otherwise fill it. When the buffer is full the oldest bytes go (and are counted).
pub const MAX_LOG_BUFFER: usize = 1024 * 1024;

/// The part of an OCI `linux.resources` that means the same inside the guest: the memory limit, the CPU shares, quota and period, and the
/// number of processes. A value that stands for "no limit" (huge) is left out. Returns None if nothing is left.
pub fn sanitize_resources(resources: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    const NO_LIMIT: i64 = 1 << 60;
    let r = resources?;
    let mut out = serde_json::Map::new();

    if let Some(limit) = r.pointer("/memory/limit").and_then(|v| v.as_i64()) {
        if limit > 0 && limit < NO_LIMIT {
            out.insert("memory".to_string(), serde_json::json!({ "limit": limit }));
        }
    }

    let mut cpu = serde_json::Map::new();
    if let Some(shares) = r.pointer("/cpu/shares").and_then(|v| v.as_u64()) {
        if shares > 0 {
            cpu.insert("shares".to_string(), serde_json::json!(shares));
        }
    }
    let quota = r
        .pointer("/cpu/quota")
        .and_then(|v| v.as_i64())
        .filter(|q| *q > 0);
    let period = r
        .pointer("/cpu/period")
        .and_then(|v| v.as_u64())
        .filter(|p| *p > 0);
    if let (Some(quota), Some(period)) = (quota, period) {
        cpu.insert("quota".to_string(), serde_json::json!(quota));
        cpu.insert("period".to_string(), serde_json::json!(period));
    }
    if !cpu.is_empty() {
        out.insert("cpu".to_string(), serde_json::Value::Object(cpu));
    }

    if let Some(pids) = r.pointer("/pids/limit").and_then(|v| v.as_i64()) {
        if pids > 0 && pids < NO_LIMIT {
            out.insert("pids".to_string(), serde_json::json!({ "limit": pids }));
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(out))
    }
}

/// Captured container output (buffered for streaming via RPC).
#[derive(Debug, Default)]
pub struct ContainerLogs {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub eof: bool,
    /// Absolute position in the stream of stdout[0] / stderr[0] (what has been taken or dropped before it).
    pub stdout_base: u64,
    pub stderr_base: u64,
    /// Bytes dropped because the buffer was full, in all.
    pub stdout_dropped: u64,
    pub stderr_dropped: u64,
}

impl ContainerLogs {
    /// Append output to stdout, dropping the oldest bytes if the buffer would pass MAX_LOG_BUFFER.
    pub fn push_stdout(&mut self, data: &[u8]) {
        Self::push(
            &mut self.stdout,
            &mut self.stdout_base,
            &mut self.stdout_dropped,
            data,
        );
    }

    /// Append output to stderr (see push_stdout).
    pub fn push_stderr(&mut self, data: &[u8]) {
        Self::push(
            &mut self.stderr,
            &mut self.stderr_base,
            &mut self.stderr_dropped,
            data,
        );
    }

    fn push(buf: &mut Vec<u8>, base: &mut u64, dropped: &mut u64, data: &[u8]) {
        buf.extend_from_slice(data);
        if buf.len() > MAX_LOG_BUFFER {
            let excess = buf.len() - MAX_LOG_BUFFER;
            buf.drain(..excess);
            *base += excess as u64;
            *dropped += excess as u64;
        }
    }

    /// Take from one stream: acknowledge (drop) what is before `offset`, return up to `max` bytes from `offset` on, and the position after them.
    /// An offset before the buffer's start (bytes were dropped) starts at the buffer's start.
    fn take(buf: &mut Vec<u8>, base: &mut u64, offset: u64, max: usize) -> (Vec<u8>, u64) {
        let end = *base + buf.len() as u64;
        let from = offset.clamp(*base, end);
        let ack = (from - *base) as usize;
        if ack > 0 {
            buf.drain(..ack);
            *base = from;
        }
        let n = buf.len().min(max);
        (buf[..n].to_vec(), *base + n as u64)
    }

    /// The new output of both streams since the given offsets, for the ReadLogs call (see proto/agent.proto).
    pub fn read(&mut self, stdout_offset: u64, stderr_offset: u64, max: usize) -> LogRead {
        let (stdout, stdout_next) =
            Self::take(&mut self.stdout, &mut self.stdout_base, stdout_offset, max);
        let (stderr, stderr_next) =
            Self::take(&mut self.stderr, &mut self.stderr_base, stderr_offset, max);
        let drained = self.stdout_base + self.stdout.len() as u64 <= stdout_next
            && self.stderr_base + self.stderr.len() as u64 <= stderr_next;
        LogRead {
            stdout,
            stderr,
            stdout_next,
            stderr_next,
            eof: self.eof && drained,
            stdout_dropped: self.stdout_dropped,
            stderr_dropped: self.stderr_dropped,
        }
    }
}

/// The result of ContainerLogs::read.
#[derive(Debug, PartialEq)]
pub struct LogRead {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_next: u64,
    pub stderr_next: u64,
    pub eof: bool,
    pub stdout_dropped: u64,
    pub stderr_dropped: u64,
}

/// What `prepare` needs from the manager, taken under its lock so that the preparing itself (which waits for disks) does not hold it.
pub struct CreatePlan {
    disks: Arc<Disks>,
    /// The volume directories a container is using now: their files are not rewritten.
    busy: HashSet<String>,
}

/// A container whose root file system and volumes are ready; `finish_create` registers it.
pub struct Prepared {
    id: String,
    volumes: Vec<VolumeUse>,
    terminal: bool,
}

/// Manages container lifecycle via crun.
pub struct ContainerManager {
    containers: HashMap<String, Container>,
    /// IDs being created (reserved, not yet registered), so that two creates of one ID do not run together.
    creating: HashSet<String>,
    /// The disks of the VM and which are claimed.
    disks: Arc<Disks>,
    /// Which containers use each volume directory.
    volume_users: HashMap<String, HashSet<String>>,
    /// Watch receivers for container exit notifications.
    exit_receivers: HashMap<String, watch::Receiver<Option<ExitStatus>>>,
    /// Buffered container logs (stdout/stderr captured from crun).
    logs: HashMap<String, std::sync::Arc<tokio::sync::Mutex<ContainerLogs>>>,
}

impl Default for ContainerManager {
    fn default() -> Self {
        Self::new()
    }
}

/// A container ID is used in paths: letters, digits, `.`, `_` and `-`, up to 128, not starting with a dot.
pub fn valid_container_id(id: &str) -> bool {
    volumes::safe_component(id)
}

fn bundle_of(id: &str) -> PathBuf {
    PathBuf::from("/run/containers").join(id)
}

impl ContainerManager {
    pub fn new() -> Self {
        Self {
            containers: HashMap::new(),
            creating: HashSet::new(),
            disks: Arc::new(Disks::new()),
            volume_users: HashMap::new(),
            exit_receivers: HashMap::new(),
            logs: HashMap::new(),
        }
    }

    /// First step of a create: check the ID, reserve it, and note which volumes are in use. The caller then runs `prepare` without the
    /// manager's lock, and `finish_create` (or `abort_create`) with it.
    pub fn begin_create(&mut self, id: &str, vols: &[VolumeInfo]) -> Result<CreatePlan> {
        if !valid_container_id(id) {
            anyhow::bail!("invalid container id");
        }
        if self.containers.contains_key(id) || self.creating.contains(id) {
            anyhow::bail!("container {id} already exists");
        }
        let bundle = bundle_of(id);
        let mut busy = HashSet::new();
        for v in vols.iter().filter(|v| !v.is_block) {
            if let Ok((root, name)) = volumes::location(&v.source, &bundle) {
                let key = root.join(name).to_string_lossy().to_string();
                if self.volume_users.get(&key).is_some_and(|u| !u.is_empty()) {
                    busy.insert(key);
                }
            }
        }
        self.creating.insert(id.to_string());
        Ok(CreatePlan {
            disks: self.disks.clone(),
            busy,
        })
    }

    /// A create that failed: give the ID back.
    pub fn abort_create(&mut self, id: &str) {
        self.creating.remove(id);
    }

    /// Last step of a create: the container exists, in the Created state (its process does not exist yet, so it has no PID).
    pub fn finish_create(&mut self, p: Prepared) {
        self.creating.remove(&p.id);
        for u in &p.volumes {
            self.volume_users
                .entry(u.key())
                .or_default()
                .insert(p.id.clone());
        }
        self.containers.insert(
            p.id.clone(),
            Container {
                pid: None,
                exit_code: None,
                state: ContainerState::Created,
                volumes: p.volumes,
                terminal: p.terminal,
                master: None,
            },
        );
        info!("container created: id={}", p.id);
    }

    /// The PIDs of the processes the manager started (the `crun run` of each container).
    /// For an exec: the container must be running. Returns the process section of its bundle's config.json (already cut down when the
    /// container was created), which bounds what an exec in it may have.
    pub fn exec_target(&self, container_id: &str) -> Result<Option<serde_json::Value>> {
        if !valid_container_id(container_id) {
            anyhow::bail!("invalid container id");
        }
        let c = self
            .containers
            .get(container_id)
            .ok_or_else(|| anyhow::anyhow!("container not found: {container_id}"))?;
        let exited = self
            .exit_receivers
            .get(container_id)
            .is_some_and(|rx| rx.borrow().is_some());
        if c.state != ContainerState::Running || exited {
            anyhow::bail!("container {container_id} is not running");
        }
        let text = std::fs::read_to_string(bundle_of(container_id).join("config.json")).ok();
        Ok(text
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v.get("process").cloned()))
    }

    pub fn tracked_pids(&self) -> HashSet<u32> {
        self.containers.values().filter_map(|c| c.pid).collect()
    }

    /// Start a previously created container using "crun run".
    pub async fn start(&mut self, container_id: &str) -> Result<u32> {
        info!("starting container: {}", container_id);

        let _container = self
            .containers
            .get(container_id)
            .ok_or_else(|| anyhow::anyhow!("container not found: {}", container_id))?;

        // Bundle path matches the mount point created in prepare().
        let bundle = bundle_of(container_id);

        let terminal = self
            .containers
            .get(container_id)
            .map(|c| c.terminal)
            .unwrap_or(false);
        // With a terminal crun makes the pty and sends its master over a unix socket (see exec.rs, which does the same for an exec).
        let console_sock = bundle.join("console.sock");
        let listener = if terminal {
            let _ = std::fs::remove_file(&console_sock);
            Some(
                std::os::unix::net::UnixListener::bind(&console_sock)
                    .context("bind the console socket")?,
            )
        } else {
            None
        };

        let mut cmd = Command::new("/bin/crun");
        cmd.arg("run").arg("--bundle").arg(&bundle);
        if terminal {
            cmd.arg("--console-socket").arg(&console_sock);
        }
        cmd.arg(container_id)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The agent's own descriptors (its ttrpc connections) must not reach a container: everything above stderr is closed on exec.
        // (Rust opens its descriptors close-on-exec, but a library that accepts connections need not.)
        unsafe {
            cmd.pre_exec(|| {
                const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
                // Not fatal if the kernel lacks close_range: the descriptors are close-on-exec anyway.
                libc::syscall(
                    libc::SYS_close_range,
                    3 as libc::c_uint,
                    !0 as libc::c_uint,
                    CLOSE_RANGE_CLOEXEC,
                );
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn crun run (bundle={})", bundle.display()))?;

        let pid = child.id().unwrap_or(0);

        if let Some(c) = self.containers.get_mut(container_id) {
            c.state = ContainerState::Running;
            c.pid = Some(pid);
        }
        info!("container started: id={}, pid={}", container_id, pid);

        // Create shared log buffer for streaming via GetContainerLogs RPC
        let log_buf = std::sync::Arc::new(tokio::sync::Mutex::new(ContainerLogs::default()));
        self.logs.insert(container_id.to_string(), log_buf.clone());

        // Create a watch channel for exit notification.
        let (tx, rx) = watch::channel::<Option<ExitStatus>>(None);

        // Take stdout/stderr handles from child for streaming capture
        let child_stdout = child.stdout.take();
        let child_stderr = child.stderr.take();

        // Spawn stdout reader
        let log_buf_out = log_buf.clone();
        if let Some(stdout) = child_stdout {
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut reader = stdout;
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            log_buf_out.lock().await.push_stdout(&buf[..n]);
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        // Spawn stderr reader
        let log_buf_err = log_buf.clone();
        if let Some(stderr) = child_stderr {
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut reader = stderr;
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            log_buf_err.lock().await.push_stderr(&buf[..n]);
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        // The terminal's output is read from the master; the end of the output is reported only after it has been read.
        let master_reader = Arc::new(tokio::sync::Notify::new());
        let master_read = Arc::new(std::sync::atomic::AtomicBool::new(!terminal));

        // Wait for exit in background using pidfd for reliable PID 1 behavior.
        // Tokio's SIGCHLD-based child.wait() is unreliable as PID 1 because
        // signals can be lost in the init context. pidfd provides a file
        // descriptor that becomes readable when the process exits — no signals.
        let container_id_owned = container_id.to_string();
        let child_pid = pid;
        let (reader_done, reader_flag) = (master_reader.clone(), master_read.clone());
        let log_for_exit = log_buf.clone();
        tokio::spawn(async move {
            let code = wait_for_exit_pidfd(child_pid, child).await;
            info!("container exited: id={} code={}", container_id_owned, code);

            // Notify exit BEFORE locking log buffer to avoid blocking
            // WaitContainer on log mutex contention.
            let _ = tx.send(Some(ExitStatus {
                code,
                exited_at: chrono::Utc::now().to_rfc3339(),
            }));

            // What the process wrote to its terminal is read before the end of the output is reported (a process it left behind may hold
            // the terminal open: a limit on the wait).
            if !reader_flag.load(std::sync::atomic::Ordering::SeqCst) {
                let notified = reader_done.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if !reader_flag.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = tokio::time::timeout(crate::exec::DRAIN_WAIT, notified).await;
                }
            }
            log_for_exit.lock().await.eof = true;
        });

        if let Some(listener) = listener {
            let received = crate::exec::receive_master(listener, rx.clone()).await;
            let _ = std::fs::remove_file(&console_sock);
            match received {
                Ok(master) => {
                    let master = Arc::new(master);
                    let (m, l) = (master.clone(), log_buf.clone());
                    let (done, flag) = (master_reader.clone(), master_read.clone());
                    tokio::spawn(async move {
                        crate::exec::read_master(m, l).await;
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        done.notify_waiters();
                    });
                    if let Some(c) = self.containers.get_mut(container_id) {
                        c.master = Some(master);
                    }
                }
                Err(e) => {
                    // The container cannot be used without its terminal: stop it, and say why.
                    warn!("container {container_id}: no terminal: {e:#}");
                    master_read.store(true, std::sync::atomic::Ordering::SeqCst);
                    master_reader.notify_waiters();
                    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                    self.exit_receivers.insert(container_id.to_string(), rx);
                    return Err(e.context("the container's terminal"));
                }
            }
        }

        self.exit_receivers.insert(container_id.to_string(), rx);

        Ok(pid)
    }

    /// The master of a running container's terminal, to write to or to resize.
    pub fn terminal_master(&self, container_id: &str) -> Result<Arc<crate::exec::Master>> {
        if !valid_container_id(container_id) {
            anyhow::bail!("invalid container id");
        }
        let c = self
            .containers
            .get(container_id)
            .ok_or_else(|| anyhow::anyhow!("container not found: {container_id}"))?;
        if !c.terminal {
            anyhow::bail!("container {container_id} has no terminal");
        }
        c.master
            .clone()
            .ok_or_else(|| anyhow::anyhow!("container {container_id} is not running"))
    }

    /// Send a signal to a container.
    pub async fn kill(&self, container_id: &str, signal: u32) -> Result<()> {
        info!("killing container: {} signal={}", container_id, signal);
        if !valid_container_id(container_id) {
            anyhow::bail!("invalid container id");
        }

        // Check if container has already exited
        if let Some(container) = self.containers.get(container_id) {
            if container.state == ContainerState::Stopped {
                info!("container {} already stopped, ignoring kill", container_id);
                return Ok(());
            }
        }

        let output = Command::new("/bin/crun")
            .arg("kill")
            .arg(container_id)
            .arg(signal.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("failed to execute crun kill")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // "No such file" means the container already exited — not an error
            if stderr.contains("No such file") || stderr.contains("not found") {
                info!(
                    "container {} already exited (crun kill: {})",
                    container_id,
                    stderr.trim()
                );
                return Ok(());
            }
            anyhow::bail!("crun kill failed: {}", stderr);
        }

        Ok(())
    }

    /// Signal all running containers. Used during agent shutdown to ensure
    /// the guest kernel can exit quickly without waiting for orphaned processes.
    pub async fn signal_all(&self, signal: u32) {
        let ids: Vec<String> = self
            .containers
            .iter()
            .filter(|(_, c)| c.state != ContainerState::Stopped)
            .map(|(id, _)| id.clone())
            .collect();

        for id in &ids {
            if let Err(e) = self.kill(id, signal).await {
                info!("signal_all: failed to signal {id}: {e:#}");
            }
        }
        if !ids.is_empty() {
            info!(
                "signal_all: sent signal {} to {} containers",
                signal,
                ids.len()
            );
        }
    }

    /// Delete a container: its process, then everything it had in the guest, in the reverse order it was made (the overlay and the erofs
    /// layers, the directory of its upper layer, which is memory, the copies of Secrets no other container uses, the log buffer).
    pub async fn delete(&mut self, container_id: &str) -> Result<(u32, i32)> {
        info!("deleting container: {}", container_id);
        if !valid_container_id(container_id) {
            anyhow::bail!("invalid container id");
        }

        let output = Command::new("/bin/crun")
            .arg("delete")
            .arg("--force")
            .arg(container_id)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("failed to execute crun delete")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("crun delete failed (may be ok): {}", stderr);
        }

        let removed = self.containers.remove(container_id);
        let (pid, exit_code) = match &removed {
            Some(c) => (c.pid.unwrap_or(0), c.exit_code.unwrap_or(0)),
            None => (0, 0),
        };

        // The root file system and the bundle.
        let bundle = bundle_of(container_id);
        let torn = tokio::task::spawn_blocking(move || teardown(&bundle))
            .await
            .unwrap_or(false);
        if !torn {
            warn!("container {container_id}: its mounts could not all be released");
        }

        // The volumes: a copy of a ConfigMap or Secret goes with its last user.
        for u in removed.map(|c| c.volumes).unwrap_or_default() {
            let key = u.key();
            let last = match self.volume_users.get_mut(&key) {
                Some(users) => {
                    users.remove(container_id);
                    users.is_empty()
                }
                None => false,
            };
            if last {
                self.volume_users.remove(&key);
                if u.has_files {
                    volumes::remove(&u.root, &u.name);
                }
            }
        }

        self.exit_receivers.remove(container_id);
        self.logs.remove(container_id);

        info!(
            "container deleted: id={}, pid={}, exit_code={}",
            container_id, pid, exit_code
        );
        Ok((pid, exit_code))
    }

    /// Re-register a container under a new ID (used after warm snapshot restore). Moves all bookkeeping — container state, exit
    /// receiver, and log buffer — from `old_id` to `new_id`. Returns the PID of the adopted container.
    ///
    /// A container belongs to the pod that started it, and a VM belongs to one pod: this exists only to rename the shadow container
    /// of a warm snapshot inside the VM restored from it. It refuses to replace a container that already has the new ID.
    pub fn adopt(&mut self, old_id: &str, new_id: &str) -> Result<u32> {
        if !valid_container_id(new_id) || old_id == new_id {
            anyhow::bail!("adopt: invalid new id");
        }
        if self.containers.contains_key(new_id)
            || self.creating.contains(new_id)
            || self.exit_receivers.contains_key(new_id)
            || self.logs.contains_key(new_id)
        {
            anyhow::bail!("adopt: container {new_id} already exists");
        }
        let container = self
            .containers
            .remove(old_id)
            .ok_or_else(|| anyhow::anyhow!("adopt: container {old_id} not found"))?;
        let pid = match container.pid {
            Some(pid) => pid,
            None => {
                let msg = format!("adopt: container {old_id} has no PID");
                self.containers.insert(old_id.to_string(), container);
                anyhow::bail!(msg);
            }
        };

        if let Some(rx) = self.exit_receivers.remove(old_id) {
            self.exit_receivers.insert(new_id.to_string(), rx);
        }
        if let Some(buf) = self.logs.remove(old_id) {
            self.logs.insert(new_id.to_string(), buf);
        }
        for users in self.volume_users.values_mut() {
            if users.remove(old_id) {
                users.insert(new_id.to_string());
            }
        }
        self.containers.insert(new_id.to_string(), container);
        info!("adopted container {old_id} -> {new_id} pid={pid}");
        Ok(pid)
    }

    pub fn get_exit_receiver(&self, id: &str) -> Option<watch::Receiver<Option<ExitStatus>>> {
        self.exit_receivers.get(id).cloned()
    }

    /// Mark a container as stopped with the given exit code.
    pub fn mark_stopped(&mut self, id: &str, code: i32) {
        if let Some(c) = self.containers.get_mut(id) {
            c.state = ContainerState::Stopped;
            c.exit_code = Some(code);
        }
    }

    /// Get the log buffer for a container (for streaming via GetContainerLogs RPC).
    pub fn get_log_buffer(
        &self,
        id: &str,
    ) -> Option<std::sync::Arc<tokio::sync::Mutex<ContainerLogs>>> {
        self.logs.get(id).cloned()
    }

    /// Get the state of a container as a proto response.
    pub async fn state(
        &self,
        container_id: &str,
    ) -> Result<crate::proto::agent::StateContainerResponse> {
        use crate::proto::agent::{ContainerState as ProtoState, StateContainerResponse};
        use ::protobuf::EnumOrUnknown;

        let mut resp = StateContainerResponse::new();
        resp.container_id = container_id.to_string();

        if let Some(container) = self.containers.get(container_id) {
            resp.pid = container.pid.unwrap_or(0);
            resp.status = match container.state {
                ContainerState::Created => EnumOrUnknown::new(ProtoState::CREATED),
                ContainerState::Running => EnumOrUnknown::new(ProtoState::RUNNING),
                ContainerState::Stopped => EnumOrUnknown::new(ProtoState::STOPPED),
            };
            resp.exit_status = container.exit_code.unwrap_or(0) as u32;
        } else {
            resp.status = EnumOrUnknown::new(ProtoState::UNKNOWN);
        }

        Ok(resp)
    }
}

/// What `prepare` needs to know about a volume once it is made ready.
enum Resolved {
    Bind {
        dest: String,
        source: String,
        readonly: bool,
        options: Vec<String>,
    },
    Block {
        dest: String,
        dev: String,
        fs_type: String,
        readonly: bool,
        options: Vec<String>,
    },
}

/// Make a container's root file system and volumes ready: mount each erofs layer, compose them with a tmpfs-backed upper layer by overlayfs,
/// write the volumes, and write the sanitised spec. If any step fails everything mounted so far is released again.
///
/// The erofs snapshotter creates layer.erofs blobs per image layer. The shim passes these as read-only virtio-blk devices to the VM.
/// This runs without the manager's lock: waiting for a disk to appear must not stop the other containers' calls.
#[allow(clippy::too_many_arguments)]
pub async fn prepare(
    plan: CreatePlan,
    container_id: &str,
    vols: &[VolumeInfo],
    config_json: &[u8],
    rootfs_preattached: bool,
    erofs_layers: u32,
    layer_serials: &[String],
    terminal: bool,
) -> Result<Prepared> {
    info!(
        "creating container: id={} erofs_layers={} preattached={} terminal={}",
        container_id, erofs_layers, rootfs_preattached, terminal
    );
    if erofs_layers == 0 {
        anyhow::bail!("erofs_layers must be > 0: the erofs snapshotter is required");
    }
    if erofs_layers > MAX_EROFS_LAYERS {
        anyhow::bail!("erofs_layers {erofs_layers} is more than the {MAX_EROFS_LAYERS} allowed");
    }
    let bundle = bundle_of(container_id);
    let result = prepare_inner(
        &plan,
        container_id,
        &bundle,
        vols,
        config_json,
        rootfs_preattached,
        erofs_layers,
        layer_serials,
        terminal,
    )
    .await;
    if result.is_err() {
        // Roll back: unmount what was mounted, remove what was made.
        let b = bundle.clone();
        let _ = tokio::task::spawn_blocking(move || teardown(&b)).await;
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn prepare_inner(
    plan: &CreatePlan,
    container_id: &str,
    mount_point: &Path,
    vols: &[VolumeInfo],
    config_json: &[u8],
    rootfs_preattached: bool,
    erofs_layers: u32,
    layer_serials: &[String],
    terminal: bool,
) -> Result<Prepared> {
    let deadline = Instant::now() + DISK_WAIT;
    std::fs::create_dir_all(mount_point)?;

    let rootfs_dir = mount_point.join("rootfs");
    std::fs::create_dir_all(&rootfs_dir)?;

    // Mount each erofs layer and compose with overlayfs
    #[cfg(target_os = "linux")]
    {
        use nix::mount::{mount, MsFlags};

        let mut lowerdirs: Vec<String> = Vec::new();

        for i in 0..erofs_layers as usize {
            // Which disk is this layer? Attached at boot: the names are fixed (vdb, vdc, ...). Hot-plugged: the disk with the serial the
            // host named, or else the next new disk that nobody has claimed (never a name worked out from the layer's number, which
            // is the first container's disk for the second container).
            let dev_name = if rootfs_preattached {
                let n = vd_name(i + 1);
                plan.disks.claim_named(&n);
                n
            } else if let Some(serial) = layer_serials.get(i).filter(|s| !s.is_empty()) {
                plan.disks.claim_by_serial(serial, deadline).await?
            } else {
                plan.disks.claim_next(deadline).await?
            };
            let dev = format!("/dev/{dev_name}");

            let layer_mount = mount_point.join(format!("layer{i}"));
            std::fs::create_dir_all(&layer_mount)?;

            // Wait for the device node to appear using inotify, then mount.
            // For pre-attached devices, /dev/vdX exists at boot — mount works
            // immediately. For hot-plugged devices, the kernel creates the node
            // after PCI enumeration (~20-50ms). inotify detects creation in <1ms
            // vs the old 100ms polling interval.
            if !std::path::Path::new(&dev).exists() {
                wait_for_dev_inotify(&dev, deadline).await?;
            }

            // Retry mount — the device node may exist before the block device
            // driver is fully initialized (ENXIO). Poll for up to 2s with
            // short sleeps between attempts.
            let mount_deadline = Instant::now() + Duration::from_millis(2000);
            loop {
                match mount(
                    Some(dev.as_str()),
                    &layer_mount,
                    Some("erofs"),
                    MsFlags::MS_RDONLY | MsFlags::MS_NOATIME,
                    None::<&str>,
                ) {
                    Ok(()) => {
                        info!("mounted erofs layer {} at {}", dev, layer_mount.display());
                        break;
                    }
                    Err(_e) if Instant::now() < mount_deadline => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(e) => {
                        anyhow::bail!("mount erofs {} at {}: {e}", dev, layer_mount.display());
                    }
                }
            }

            lowerdirs.push(layer_mount.to_string_lossy().to_string());
        }

        // Create writable upper + work dirs on tmpfs
        let upper_dir = mount_point.join("upper");
        let work_dir = mount_point.join("work");
        std::fs::create_dir_all(&upper_dir)?;
        std::fs::create_dir_all(&work_dir)?;

        // Compose overlayfs: erofs layers as lowerdirs, tmpfs as upper
        let lowerdir_opt = lowerdirs.join(":");
        let overlay_opts = format!(
            "lowerdir={},upperdir={},workdir={}",
            lowerdir_opt,
            upper_dir.display(),
            work_dir.display()
        );

        mount(
            Some("overlay"),
            rootfs_dir.as_path(),
            Some("overlay"),
            MsFlags::empty(),
            Some(overlay_opts.as_str()),
        )
        .with_context(|| format!("overlayfs mount with erofs layers: {overlay_opts}"))?;
        info!(
            "overlayfs rootfs: {} erofs layers + tmpfs upper",
            erofs_layers
        );
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (rootfs_preattached, erofs_layers, layer_serials, &rootfs_dir);
    }

    // The spec: the host's, which must be there, as one document.
    if config_json.is_empty() {
        anyhow::bail!("no config.json: the spec must come inline");
    }
    let spec_value: serde_json::Value =
        serde_json::from_slice(config_json).context("failed to parse config.json")?;

    // The volumes.
    let mut uses = Vec::new();
    let mut resolved = Vec::new();
    for vol in vols {
        if !spec::valid_destination(&vol.destination) {
            anyhow::bail!("volume destination {:?} is not allowed", vol.destination);
        }
        if vol.is_block {
            // A disk-backed emptyDir: a block volume whose source is a directory of the shared volume root. The disk is mounted there
            // once for the VM, and every container (and every destination) of the pod gets that directory bound in.
            if let Some(name) = volumes::shared_name(&vol.source) {
                let dir =
                    shared_block_volume(&plan.disks, name, &vol.serial, &vol.fs_type, deadline)
                        .await?;
                info!(
                    "block volume {name} -> {} (shared, {})",
                    vol.destination,
                    dir.display()
                );
                resolved.push(Resolved::Bind {
                    dest: vol.destination.clone(),
                    source: dir.to_string_lossy().to_string(),
                    readonly: vol.readonly,
                    options: vol.options.clone(),
                });
                continue;
            }
            if !BLOCK_FS_TYPES.contains(&vol.fs_type.as_str()) {
                anyhow::bail!("block volume file system {:?} is not allowed", vol.fs_type);
            }
            let name = if vol.serial.is_empty() {
                plan.disks.claim_next(deadline).await?
            } else {
                plan.disks.claim_by_serial(&vol.serial, deadline).await?
            };
            let dev = format!("/dev/{name}");
            if !Path::new(&dev).exists() {
                wait_for_dev_inotify(&dev, deadline).await?;
            }
            info!(
                "block volume {} -> {} (dev={}, fs={})",
                vol.source, vol.destination, dev, vol.fs_type
            );
            resolved.push(Resolved::Block {
                dest: vol.destination.clone(),
                dev,
                fs_type: vol.fs_type.clone(),
                readonly: vol.readonly,
                options: vol.options.clone(),
            });
            continue;
        }

        let (root, name) = volumes::location(&vol.source, mount_point)?;
        let key = root.join(&name).to_string_lossy().to_string();
        let files: Vec<VolumeFile<'_>> = vol
            .inline_files
            .iter()
            .map(|f| VolumeFile {
                path: &f.path,
                content: &f.content,
                mode: f.mode,
            })
            .collect();
        let source = volumes::materialize(
            &root,
            &name,
            &files,
            &vol.destination,
            plan.busy.contains(&key),
        )?;

        // The pod's hostname comes as the /etc/hostname the runtime bind-mounts; the VM is the pod, so it is the VM's hostname.
        if vol.destination == "/etc/hostname" && vol.inline_files.len() == 1 {
            set_hostname(String::from_utf8_lossy(&vol.inline_files[0].content).trim());
        }
        info!(
            "fs volume mount: {} -> {} (ro={}, {} files)",
            source.display(),
            vol.destination,
            vol.readonly,
            vol.inline_files.len()
        );
        uses.push(VolumeUse {
            root,
            name,
            has_files: !vol.inline_files.is_empty(),
        });
        resolved.push(Resolved::Bind {
            dest: vol.destination.clone(),
            source: source.to_string_lossy().to_string(),
            readonly: vol.readonly,
            options: vol.options.clone(),
        });
    }

    // The hostname of the spec, if there is one (the VM's UTS namespace is shared by the pod's containers).
    if let Some(name) = spec::hostname_of(&spec_value) {
        set_hostname(&name);
    }

    // The guest's own mounts and the volumes.
    let mut mounts = spec::base_mounts();
    for r in &resolved {
        mounts.push(match r {
            Resolved::Bind {
                dest,
                source,
                readonly,
                options,
            } => serde_json::json!({
                "destination": dest,
                "type": "bind",
                "source": source,
                "options": spec::volume_options(*readonly, options, &["rbind"]),
            }),
            Resolved::Block {
                dest,
                dev,
                fs_type,
                readonly,
                options,
            } => serde_json::json!({
                "destination": dest,
                "type": fs_type,
                "source": dev,
                "options": spec::volume_options(*readonly, options, &[]),
            }),
        });
    }

    // The spec crun gets: rebuilt from an allowlist (see spec.rs).
    let mut clean = spec::sanitize_spec(&spec_value, container_id, mounts)?;
    // Whether the process has a terminal is what the task was created with, not what the JSON says (as for an exec): crun needs
    // `--console-socket` exactly when it is true.
    if let Some(p) = clean.get_mut("process").and_then(|p| p.as_object_mut()) {
        p.insert("terminal".to_string(), serde_json::json!(terminal));
    }
    std::fs::write(
        mount_point.join("config.json"),
        serde_json::to_string_pretty(&clean)?,
    )
    .context("write config.json")?;

    Ok(Prepared {
        id: container_id.to_string(),
        volumes: uses,
        terminal,
    })
}

/// Set the VM's hostname if the name is one the guest accepts (see spec::valid_hostname).
fn set_hostname(name: &str) {
    if !spec::valid_hostname(name) {
        warn!("not setting the hostname {name:?}");
        return;
    }
    let rc = unsafe { libc::sethostname(name.as_ptr() as *const libc::c_char, name.len()) };
    if rc != 0 {
        warn!("sethostname({name}): {}", std::io::Error::last_os_error());
    }
}

/// The mount points below `prefix` in the text of /proc/self/mounts, deepest first (the order to unmount them in).
pub fn mounts_under(proc_mounts: &str, prefix: &str) -> Vec<String> {
    let below = format!("{}/", prefix.trim_end_matches('/'));
    let mut v: Vec<String> = proc_mounts
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .filter(|p| p.starts_with(&below))
        .map(|p| p.to_string())
        .collect();
    v.sort_by_key(|p| std::cmp::Reverse(p.len()));
    v.dedup();
    v
}

/// Release a container's bundle: unmount everything below it (deepest first, lazily if it is busy) and remove it. The directory is
/// removed only if nothing is left mounted in it, so that removing files can never reach into a mounted file system. Returns whether
/// everything went.
fn teardown(bundle: &Path) -> bool {
    let mut ok = true;
    #[cfg(target_os = "linux")]
    {
        use nix::mount::{umount2, MntFlags};
        let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
        for m in mounts_under(&mounts, &bundle.to_string_lossy()) {
            if umount2(m.as_str(), MntFlags::empty()).is_err()
                && umount2(m.as_str(), MntFlags::MNT_DETACH).is_err()
            {
                warn!("could not unmount {m}");
                ok = false;
            }
        }
        // A lazy unmount can leave the mount listed for a moment; look again before removing anything.
        let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
        if !mounts_under(&mounts, &bundle.to_string_lossy()).is_empty() {
            ok = false;
        }
    }
    if ok {
        let _ = std::fs::remove_dir_all(bundle);
    }
    ok
}

/// Wait for a device node (e.g. `/dev/vdb`) to appear using inotify.
/// Returns immediately if the device already exists.
/// Fails at `deadline`.
#[cfg(target_os = "linux")]
async fn wait_for_dev_inotify(dev_path: &str, deadline: std::time::Instant) -> Result<()> {
    use std::path::Path;

    if Path::new(dev_path).exists() {
        return Ok(());
    }

    let dev_name = Path::new(dev_path)
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid device path")?
        .to_string();

    let watch_dir = Path::new(dev_path)
        .parent()
        .unwrap_or(Path::new("/dev"))
        .to_path_buf();

    info!(
        "waiting for device {} via inotify on {}",
        dev_path,
        watch_dir.display()
    );

    // Run inotify in a blocking task (it uses blocking read)
    let result = tokio::task::spawn_blocking(move || -> Result<()> {
        struct FdGuard(i32);
        impl Drop for FdGuard {
            fn drop(&mut self) {
                unsafe {
                    libc::close(self.0);
                }
            }
        }

        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if fd < 0 {
            anyhow::bail!("inotify_init1: {}", std::io::Error::last_os_error());
        }
        let _fd_guard = FdGuard(fd);

        let dir_cstr = std::ffi::CString::new(watch_dir.to_string_lossy().as_bytes())?;
        let wd = unsafe {
            libc::inotify_add_watch(fd, dir_cstr.as_ptr(), libc::IN_CREATE | libc::IN_MOVED_TO)
        };
        if wd < 0 {
            anyhow::bail!(
                "inotify_add_watch({}): {}",
                watch_dir.display(),
                std::io::Error::last_os_error()
            );
        }

        // Check again after adding watch (device may have appeared between
        // our initial check and the watch registration)
        if watch_dir.join(&dev_name).exists() {
            return Ok(());
        }

        // Poll inotify fd with timeout
        let mut buf = [0u8; 4096];
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                anyhow::bail!("timeout waiting for device {}", dev_name);
            }

            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout_ms = remaining.as_millis().min(1000) as i32;
            let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                anyhow::bail!("poll: {}", err);
            }
            if ret == 0 {
                // Timeout on this poll — check if device appeared
                if watch_dir.join(&dev_name).exists() {
                    return Ok(());
                }
                continue;
            }

            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n < 0 {
                continue;
            }

            // Parse inotify events looking for our device name
            let mut offset = 0usize;
            while offset + 16 <= n as usize {
                let name_len =
                    u32::from_ne_bytes(buf[offset + 12..offset + 16].try_into().unwrap()) as usize;
                let event_end = offset + 16 + name_len;
                if event_end <= n as usize && name_len > 0 {
                    let name_bytes = &buf[offset + 16..event_end];
                    if let Ok(name) = std::ffi::CStr::from_bytes_until_nul(name_bytes) {
                        if name.to_string_lossy() == dev_name {
                            info!("inotify: device {} appeared", dev_name);
                            return Ok(());
                        }
                    }
                }
                offset = event_end;
            }
        }
    })
    .await
    .context("inotify task panicked")?;

    result
}

/// Wait for a process to exit using pidfd (Linux 5.3+).
/// Falls back to tokio child.wait() if pidfd is unavailable.
///
/// pidfd is immune to the SIGCHLD delivery issues that affect
/// tokio::process::Child::wait() when running as PID 1 (init).
pub(crate) async fn wait_for_exit_pidfd(pid: u32, mut child: tokio::process::Child) -> i32 {
    #[cfg(target_os = "linux")]
    {
        // Try pidfd_open
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as i32, 0) };
        if fd >= 0 {
            let fd = fd as std::os::unix::io::RawFd;
            // SAFETY: fd is a valid pidfd from pidfd_open
            let owned = unsafe { std::os::unix::io::OwnedFd::from_raw_fd(fd) };
            match tokio::io::unix::AsyncFd::new(owned) {
                Ok(async_fd) => {
                    // Wait for the fd to become readable (process exited)
                    match async_fd.readable().await {
                        Ok(_ready_guard) => {}
                        Err(e) => {
                            warn!(
                                "async_fd.readable() failed for pid {pid}: {e}, \
                                 falling back to child.wait()"
                            );
                            match child.wait().await {
                                Ok(status) => return status.code().unwrap_or(137),
                                Err(_) => return 137,
                            }
                        }
                    }
                    // Reap the child to get exit code
                    match child.try_wait() {
                        Ok(Some(status)) => return status.code().unwrap_or(137),
                        _ => {
                            // pidfd signaled but try_wait failed; do blocking wait
                            match child.wait().await {
                                Ok(status) => return status.code().unwrap_or(137),
                                Err(_) => return 137,
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("AsyncFd::new for pidfd failed: {e}, falling back to child.wait()");
                }
            }
        } else {
            warn!("pidfd_open failed for pid {pid}, falling back to child.wait()");
        }
    }

    // Fallback: tokio SIGCHLD-based wait
    match child.wait().await {
        Ok(status) => status.code().unwrap_or(137),
        Err(_) => 137,
    }
}

#[cfg(test)]
mod log_tests {
    use super::*;

    #[test]
    fn each_stream_has_its_own_offset_and_a_short_one_loses_nothing() {
        let mut logs = ContainerLogs::default();
        logs.push_stdout(&[b'o'; 100]);
        logs.push_stderr(b"err1");
        let a = logs.read(0, 0, 1 << 20);
        assert_eq!((a.stdout.len(), a.stderr.len()), (100, 4));
        assert_eq!((a.stdout_next, a.stderr_next), (100, 4));
        // More stderr arrives while stdout stays where it was: with one shared offset (the older call) this tail would be skipped.
        logs.push_stderr(b"err2");
        let b = logs.read(a.stdout_next, a.stderr_next, 1 << 20);
        assert_eq!(b.stdout, b"");
        assert_eq!(b.stderr, b"err2");
        assert_eq!(b.stderr_next, 8);
    }

    #[test]
    fn what_the_caller_took_is_dropped_and_asking_again_returns_the_rest_only() {
        let mut logs = ContainerLogs::default();
        logs.push_stdout(b"hello world");
        let a = logs.read(0, 0, 5);
        assert_eq!(a.stdout, b"hello");
        assert_eq!(a.stdout_next, 5);
        let b = logs.read(a.stdout_next, 0, 1 << 20);
        assert_eq!(b.stdout, b" world");
        // Everything before offset 5 was acknowledged and is gone from the buffer.
        assert_eq!(logs.stdout.len(), 6);
        assert_eq!(logs.stdout_base, 5);
        let c = logs.read(b.stdout_next, 0, 1 << 20);
        assert_eq!(c.stdout, b"");
        assert_eq!(logs.stdout.len(), 0);
    }

    #[test]
    fn the_buffer_is_bounded_and_the_loss_is_counted() {
        let mut logs = ContainerLogs::default();
        let chunk = vec![b'x'; 4096];
        for _ in 0..(MAX_LOG_BUFFER / 4096 + 10) {
            logs.push_stdout(&chunk);
        }
        assert_eq!(logs.stdout.len(), MAX_LOG_BUFFER);
        assert_eq!(logs.stdout_dropped, 10 * 4096);
        // A caller at 0 (too slow) is moved to the start of what is left, and told how much is gone.
        let r = logs.read(0, 0, 1 << 20);
        assert_eq!(r.stdout.len(), MAX_LOG_BUFFER);
        assert_eq!(r.stdout_dropped, 10 * 4096);
        assert_eq!(r.stdout_next, (MAX_LOG_BUFFER as u64) + 10 * 4096);
    }

    #[test]
    fn eof_is_reported_only_when_the_container_has_exited_and_everything_was_returned() {
        let mut logs = ContainerLogs::default();
        logs.push_stdout(b"abc");
        logs.eof = true;
        let a = logs.read(0, 0, 2);
        assert!(!a.eof, "one byte of stdout is still to come");
        let b = logs.read(a.stdout_next, a.stderr_next, 1 << 20);
        assert_eq!(b.stdout, b"c");
        assert!(b.eof);
        let c = ContainerLogs::default().read(0, 0, 10);
        assert!(
            !c.eof,
            "a running container with no output yet is not at its end"
        );
    }
}

#[cfg(test)]
mod resource_tests {
    use super::*;

    #[test]
    fn the_memory_cpu_and_pids_limits_are_kept_and_the_host_side_parts_are_dropped() {
        let host = serde_json::json!({
            "devices": [{"allow": false, "access": "rwm"}],
            "memory": {"limit": 104857600, "swap": 104857600, "disableOOMKiller": false},
            "cpu": {"shares": 102, "quota": 50000, "period": 100000, "cpus": "0-7", "mems": "0"},
            "pids": {"limit": 512},
            "hugepageLimits": [{"pageSize": "2MB", "limit": 0}],
            "blockIO": {"weight": 10}
        });
        let got = sanitize_resources(Some(&host)).expect("limits");
        assert_eq!(
            got,
            serde_json::json!({
                "memory": {"limit": 104857600},
                "cpu": {"shares": 102, "quota": 50000, "period": 100000},
                "pids": {"limit": 512}
            })
        );
    }

    #[test]
    fn no_limit_means_no_resources_so_the_container_gets_no_cgroup_of_its_own() {
        assert_eq!(sanitize_resources(None), None);
        assert_eq!(sanitize_resources(Some(&serde_json::json!({}))), None);
        // containerd leaves quota out and shares at its minimum for a container without a limit; a huge memory limit stands for none.
        let none =
            serde_json::json!({"memory": {"limit": 9223372036854771712i64}, "cpu": {"shares": 2}});
        assert_eq!(
            sanitize_resources(Some(&none)),
            Some(serde_json::json!({"cpu": {"shares": 2}}))
        );
    }

    #[test]
    fn a_quota_without_a_period_is_not_passed_on() {
        let half = serde_json::json!({"cpu": {"quota": 50000}});
        assert_eq!(sanitize_resources(Some(&half)), None);
    }
}

#[cfg(test)]
mod manager_tests {
    use super::*;
    use crate::volumes::VOLUME_ROOT;

    fn manager_with(id: &str) -> ContainerManager {
        let mut m = ContainerManager::new();
        m.begin_create(id, &[]).unwrap();
        m.finish_create(Prepared {
            id: id.to_string(),
            volumes: vec![],
            terminal: false,
        });
        m
    }

    #[test]
    fn container_ids_are_safe_path_components() {
        assert!(valid_container_id("a1b2c3"));
        assert!(valid_container_id("shadow-ctr-245df30f"));
        assert!(!valid_container_id(""));
        assert!(!valid_container_id("../x"));
        assert!(!valid_container_id("a/b"));
        assert!(!valid_container_id(".hidden"));
        assert!(!valid_container_id(&"a".repeat(129)));
    }

    #[test]
    fn a_second_create_of_one_id_is_refused_while_the_first_runs_or_exists() {
        let mut m = ContainerManager::new();
        m.begin_create("c1", &[]).unwrap();
        assert!(m.begin_create("c1", &[]).is_err(), "reserved");
        m.abort_create("c1");
        m.begin_create("c1", &[]).unwrap();
        m.finish_create(Prepared {
            id: "c1".into(),
            volumes: vec![],
            terminal: false,
        });
        assert!(m.begin_create("c1", &[]).is_err(), "exists");
        assert!(m.begin_create("../c2", &[]).is_err(), "unsafe id");
    }

    #[test]
    fn a_created_container_has_no_pid_yet() {
        let m = manager_with("c1");
        assert_eq!(m.containers["c1"].state, ContainerState::Created);
        assert_eq!(m.containers["c1"].pid, None);
        assert!(m.tracked_pids().is_empty());
    }

    #[test]
    fn adopt_moves_the_bookkeeping_and_refuses_to_replace_a_container() {
        let mut m = manager_with("old");
        m.containers.get_mut("old").unwrap().pid = Some(42);
        m.begin_create("other", &[]).unwrap();
        m.finish_create(Prepared {
            id: "other".into(),
            volumes: vec![],
            terminal: false,
        });
        assert!(
            m.adopt("old", "other").is_err(),
            "must not overwrite another container"
        );
        assert!(
            m.containers.contains_key("old"),
            "a refused adopt loses nothing"
        );
        assert!(m.adopt("old", "old").is_err());
        assert!(m.adopt("old", "../x").is_err());
        assert_eq!(m.adopt("old", "new").unwrap(), 42);
        assert!(!m.containers.contains_key("old") && m.containers.contains_key("new"));
        // a container that was never started has no PID to adopt, and stays where it is
        let mut m = manager_with("old");
        assert!(m.adopt("old", "new").is_err());
        assert!(m.containers.contains_key("old"));
    }

    #[test]
    fn a_volume_in_use_is_noted_when_the_next_container_is_planned() {
        let mut m = ContainerManager::new();
        let vol = VolumeInfo {
            destination: "/etc/cfg".into(),
            source: format!("{VOLUME_ROOT}/abc123"),
            readonly: true,
            is_block: false,
            fs_type: String::new(),
            options: vec![],
            serial: String::new(),
            inline_files: vec![],
        };
        let plan = m.begin_create("c1", std::slice::from_ref(&vol)).unwrap();
        assert!(plan.busy.is_empty());
        m.finish_create(Prepared {
            id: "c1".into(),
            volumes: vec![VolumeUse {
                root: PathBuf::from(VOLUME_ROOT),
                name: "abc123".into(),
                has_files: true,
            }],
            terminal: false,
        });
        let plan = m.begin_create("c2", std::slice::from_ref(&vol)).unwrap();
        assert_eq!(plan.busy.len(), 1);
    }

    #[test]
    fn mounts_below_a_bundle_are_listed_deepest_first_and_nothing_else() {
        let text = "overlay /run/containers/c1/rootfs overlay rw 0 0\n\
                    /dev/vdb /run/containers/c1/layer0 erofs ro 0 0\n\
                    /dev/vdc /run/containers/c10/layer0 erofs ro 0 0\n\
                    tmpfs /run tmpfs rw 0 0\n\
                    tmpfs /run/containers/c1/rootfs/dev tmpfs rw 0 0\n";
        assert_eq!(
            mounts_under(text, "/run/containers/c1"),
            [
                "/run/containers/c1/rootfs/dev",
                "/run/containers/c1/rootfs",
                "/run/containers/c1/layer0"
            ]
        );
        assert!(mounts_under(text, "/run/containers/c2").is_empty());
    }

    #[test]
    fn a_disk_backed_volume_needs_a_serial_and_the_hosts_file_system() {
        assert!(check_shared_block("ed0123456789abcdef", "ext4").is_ok());
        assert!(
            check_shared_block("", "ext4").is_err(),
            "no serial: the disk would be found by position"
        );
        assert!(check_shared_block("ed0123456789abcdef", "xfs").is_err());
        assert!(check_shared_block("ed0123456789abcdef", "").is_err());
    }
}
