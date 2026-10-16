//! Processes run in a container with `crun exec`: what `kubectl exec` and the exec probes of the kubelet need.
//!
//! A process is *created* (checked and cut down like a container's, see `spec::sanitize_exec_process`), *started*, written to and read
//! from, killed, waited for and *deleted*, by id, through the RPCs of `proto/agent.proto`. The host shim carries the pipes (or the
//! terminal) containerd gave it to and from these calls.
//!
//! How it runs:
//! - `crun exec --process <file> --pid-file <file> <container>` is a child of the agent, as `crun run` is for a container. Without a
//!   terminal its stdin, stdout and stderr are pipes of the agent's; the process inherits them. Its exit status is the exec's.
//! - With a terminal crun makes the pty and sends its master over a unix socket (`--console-socket`), which the agent receives with
//!   SCM_RIGHTS; reading and writing the master are the process's output and input, and a resize is `TIOCSWINSZ` on it.
//! - The process is signalled through a pidfd opened on the pid crun wrote to the pid file, so a pid that has been reused is never hit.
//! - Output goes into the same bounded buffers as a container's (`ContainerLogs`): 1 MiB a stream, the oldest bytes dropped and counted.
//!
//! Limits: 8 execs per container, a process definition of at most 1 MiB, 64 KiB per write. There is no limit on how long an exec runs
//! (the client decides).

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use log::{info, warn};
use serde_json::Value;
use tokio::io::unix::AsyncFd;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{watch, Mutex, Notify};

use crate::container::{valid_container_id, wait_for_exit_pidfd, ContainerLogs, ExitStatus};

/// Where the files of a starting exec are (process definition, pid file, console socket): root's, mode 0700. The file names are short
/// and made here, not from the ids (a unix socket path is at most 107 bytes).
const EXEC_DIR: &str = "/run/cloudhv-exec";

pub const MAX_EXECS_PER_CONTAINER: usize = 8;
pub const MAX_PROCESS_JSON: usize = 1024 * 1024;
pub const MAX_WRITE: usize = 64 * 1024;
/// The most bytes of one stream an ExecRead returns (the ttrpc message limit is 4 MiB).
pub const MAX_READ: usize = 1024 * 1024;
pub const DEFAULT_READ: usize = 256 * 1024;
/// How long crun has to start the process (write the pid file, or hand over the terminal).
pub(crate) const START_WAIT: Duration = Duration::from_secs(10);
/// How long a write to a process that does not read may take.
const WRITE_WAIT: Duration = Duration::from_secs(10);
/// How long the readers get to finish after the process has exited (a process it left behind may hold the pipes open).
pub(crate) const DRAIN_WAIT: Duration = Duration::from_secs(3);

/// The exec id under which the terminal of a container's own process (the one `crun run` started) is written to and resized: the
/// calls `ExecWrite`, `ExecCloseStdin` and `ExecResize` with this id go to the container (see `server.rs`). An exec process cannot be
/// made with it (`create` refuses it), and no other call (`ExecStart`, `ExecKill`, `ExecWait`, `ExecDelete`, `ExecRead`) means anything for it.
pub const INIT_EXEC_ID: &str = "init";

static NEXT_FILE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Created,
    Running,
    Exited,
}

/// The master side of a process's terminal.
pub(crate) struct Master {
    fd: AsyncFd<OwnedFd>,
}

impl std::fmt::Debug for Master {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Master")
    }
}

impl Master {
    pub(crate) fn raw_fd(&self) -> RawFd {
        self.fd.get_ref().as_raw_fd()
    }
}

enum Stdin {
    Pipe(tokio::process::ChildStdin),
    Pty(Arc<Master>),
}

/// How many reader tasks of a process are still running, so that the end of the output can be told.
#[derive(Default)]
struct Readers {
    running: AtomicUsize,
    done: Notify,
}

impl Readers {
    fn add(&self) {
        self.running.fetch_add(1, Ordering::SeqCst);
    }
    fn finish(&self) {
        if self.running.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.done.notify_waiters();
        }
    }
    async fn wait(&self) {
        // Registered before the count is read, so that a reader that finishes in between is not missed.
        let notified = self.done.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.running.load(Ordering::SeqCst) == 0 {
            return;
        }
        notified.await;
    }
}

struct Exec {
    terminal: bool,
    process: Value,
    state: State,
    /// The pid of crun (the agent's child) and of the process itself.
    crun_pid: u32,
    pid: u32,
    pidfd: Option<OwnedFd>,
    stdin: Option<Stdin>,
    master: Option<Arc<Master>>,
    /// The exit of the process. Made with the exec, so that ExecWait can be called before ExecStart (containerd's CRI waits first, then
    /// starts); the sender goes to the task that waits for crun when the exec starts.
    exit_tx: Option<watch::Sender<Option<ExitStatus>>>,
    exit_rx: watch::Receiver<Option<ExitStatus>>,
    logs: Arc<Mutex<ContainerLogs>>,
    /// Files made for the start, removed when it is over.
    files: Vec<PathBuf>,
}

type Key = (String, String);

/// The execs of the VM.
#[derive(Default)]
pub struct ExecManager {
    execs: Mutex<HashMap<Key, Arc<Mutex<Exec>>>>,
}

/// An exec id: 1 to 64 characters of `[A-Za-z0-9_-]` (what the daemon accepts; containerd's are 64 hex digits).
pub fn valid_exec_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn key(container_id: &str, exec_id: &str) -> Result<Key> {
    if !valid_container_id(container_id) {
        anyhow::bail!("invalid container id");
    }
    if !valid_exec_id(exec_id) {
        anyhow::bail!("invalid exec id");
    }
    Ok((container_id.to_string(), exec_id.to_string()))
}

impl ExecManager {
    pub fn new() -> Self {
        Self::default()
    }

    async fn get(&self, container_id: &str, exec_id: &str) -> Result<Arc<Mutex<Exec>>> {
        let k = key(container_id, exec_id)?;
        self.execs
            .lock()
            .await
            .get(&k)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown exec {exec_id} of container {container_id}"))
    }

    /// Record a process to run. `container_process` is the process section of the (running) container, which bounds what the exec
    /// may have.
    pub async fn create(
        &self,
        container_id: &str,
        exec_id: &str,
        process_json: &[u8],
        terminal: bool,
        container_process: Option<&Value>,
    ) -> Result<()> {
        let k = key(container_id, exec_id)?;
        if exec_id == INIT_EXEC_ID {
            anyhow::bail!(
                "the exec id {INIT_EXEC_ID:?} is reserved for the container's own terminal"
            );
        }
        if process_json.len() > MAX_PROCESS_JSON {
            anyhow::bail!("the process definition is larger than {MAX_PROCESS_JSON} bytes");
        }
        let process: Value =
            serde_json::from_slice(process_json).context("the process definition is not JSON")?;
        let process = crate::spec::sanitize_exec_process(&process, container_process, terminal)?;

        let mut execs = self.execs.lock().await;
        if execs.contains_key(&k) {
            anyhow::bail!("exec {exec_id} of container {container_id} exists");
        }
        if execs.keys().filter(|(c, _)| c == container_id).count() >= MAX_EXECS_PER_CONTAINER {
            anyhow::bail!("container {container_id} already has {MAX_EXECS_PER_CONTAINER} execs");
        }
        let (exit_tx, exit_rx) = watch::channel::<Option<ExitStatus>>(None);
        execs.insert(
            k,
            Arc::new(Mutex::new(Exec {
                terminal,
                process,
                state: State::Created,
                crun_pid: 0,
                pid: 0,
                pidfd: None,
                stdin: None,
                master: None,
                exit_tx: Some(exit_tx),
                exit_rx,
                logs: Arc::new(Mutex::new(ContainerLogs::default())),
                files: Vec::new(),
            })),
        );
        Ok(())
    }

    /// Start a created process. Returns its pid.
    pub async fn start(&self, container_id: &str, exec_id: &str) -> Result<u32> {
        let exec = self.get(container_id, exec_id).await?;
        // Held while starting, so that two starts of one exec do not run together.
        let mut e = exec.lock().await;
        if e.state != State::Created {
            anyhow::bail!("exec {exec_id} was started already");
        }
        match self.spawn(container_id, &mut e).await {
            Ok(pid) => {
                e.state = State::Running;
                Ok(pid)
            }
            Err(err) => {
                // Whatever was started is stopped, and the exec is over: it can only be deleted. (Not if crun has already been waited for:
                // its pid may be someone else's by now.)
                if e.crun_pid != 0 && e.exit_rx.borrow().is_none() {
                    kill_pid(e.crun_pid, libc::SIGKILL);
                }
                e.state = State::Exited;
                // What crun said (it was not started: the program is missing, the container is gone) is the reason to show.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let said = {
                    let mut l = e.logs.lock().await;
                    l.eof = true;
                    let n = l.stderr.len().min(1024);
                    String::from_utf8_lossy(&l.stderr[..n]).trim().to_string()
                };
                remove_files(&mut e.files);
                if said.is_empty() {
                    Err(err)
                } else {
                    Err(anyhow::anyhow!("{err:#}: {said}"))
                }
            }
        }
    }

    async fn spawn(&self, container_id: &str, e: &mut Exec) -> Result<u32> {
        make_dir(Path::new(EXEC_DIR))?;
        let n = NEXT_FILE.fetch_add(1, Ordering::SeqCst);
        let process_file = Path::new(EXEC_DIR).join(format!("x{n}.json"));
        let pid_file = Path::new(EXEC_DIR).join(format!("x{n}.pid"));
        let sock_file = Path::new(EXEC_DIR).join(format!("x{n}.sock"));
        e.files = vec![process_file.clone(), pid_file.clone(), sock_file.clone()];
        write_private(&process_file, e.process.to_string().as_bytes())?;

        let listener = if e.terminal {
            Some(
                std::os::unix::net::UnixListener::bind(&sock_file)
                    .context("bind the console socket")?,
            )
        } else {
            None
        };

        let mut cmd = Command::new("/bin/crun");
        cmd.arg("exec")
            .arg("--process")
            .arg(&process_file)
            .arg("--pid-file")
            .arg(&pid_file);
        if e.terminal {
            cmd.arg("--console-socket").arg(&sock_file);
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
        } else {
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        }
        cmd.arg(container_id);
        // The agent's own descriptors must not reach the process (everything above stderr is closed on exec).
        unsafe {
            cmd.pre_exec(|| {
                const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
                libc::syscall(
                    libc::SYS_close_range,
                    3 as libc::c_uint,
                    !0 as libc::c_uint,
                    CLOSE_RANGE_CLOEXEC,
                );
                Ok(())
            });
        }
        let mut child = cmd.spawn().context("spawn crun exec")?;
        let crun_pid = child.id().unwrap_or(0);
        e.crun_pid = crun_pid;

        let readers = Arc::new(Readers::default());
        let tx = e
            .exit_tx
            .take()
            .ok_or_else(|| anyhow::anyhow!("the exit of the exec is taken already"))?;
        let rx = e.exit_rx.clone();

        // Output: the pipes (stdout and stderr on their own), or the terminal's master.
        if !e.terminal {
            if let Some(out) = child.stdout.take() {
                spawn_pipe_reader(out, e.logs.clone(), readers.clone(), false);
            }
            if let Some(err) = child.stderr.take() {
                spawn_pipe_reader(err, e.logs.clone(), readers.clone(), true);
            }
            e.stdin = child.stdin.take().map(Stdin::Pipe);
        }

        // The exit of crun is the exit of the exec: waited for with a pidfd (see wait_for_exit_pidfd) from here on.
        let logs = e.logs.clone();
        let readers_for_exit = readers.clone();
        tokio::spawn(async move {
            let code = wait_for_exit_pidfd(crun_pid, child).await;
            info!("exec exited: crun pid={crun_pid} code={code}");
            let _ = tx.send(Some(ExitStatus {
                code,
                exited_at: chrono::Utc::now().to_rfc3339(),
            }));
            // What the process wrote is read before the end of the output is reported.
            let _ = tokio::time::timeout(DRAIN_WAIT, readers_for_exit.wait()).await;
            logs.lock().await.eof = true;
        });

        if let Some(listener) = listener {
            let master = receive_master(listener, rx.clone()).await?;
            let master = Arc::new(master);
            readers.add();
            let (m, l, r) = (master.clone(), e.logs.clone(), readers.clone());
            tokio::spawn(async move {
                read_master(m, l).await;
                r.finish();
            });
            e.stdin = Some(Stdin::Pty(master.clone()));
            e.master = Some(master);
        }

        // The pid: crun writes it before it runs the process. If crun ends first, the exec could not be started.
        let pid = wait_for_pid(&pid_file, rx).await.with_context(|| {
            "crun exec did not start the process (is the container running? is the program there?)".to_string()
        })?;
        e.pid = pid;
        e.pidfd = pidfd_open(pid);
        // The process definition may hold secrets (its environment): it is not kept on disk once the process runs.
        let _ = std::fs::remove_file(&process_file);
        info!("exec started: container={container_id} pid={pid}");
        Ok(pid)
    }

    /// Send a signal to a started process.
    pub async fn kill(&self, container_id: &str, exec_id: &str, signal: u32) -> Result<()> {
        if signal == 0 || signal > 64 {
            anyhow::bail!("invalid signal");
        }
        let exec = self.get(container_id, exec_id).await?;
        let e = exec.lock().await;
        match e.state {
            State::Created => anyhow::bail!("exec {exec_id} was not started"),
            State::Exited => Ok(()),
            State::Running => {
                // Already gone, by the pidfd's account: nothing to do.
                match &e.pidfd {
                    Some(fd) => {
                        if !pidfd_send_signal(fd.as_raw_fd(), signal as i32) {
                            info!("exec {exec_id}: signal {signal}: the process is gone");
                        }
                    }
                    None => {
                        info!("exec {exec_id}: no pidfd (the process was gone when it started)")
                    }
                }
                Ok(())
            }
        }
    }

    /// Wait until the process has exited. No timeout: the caller has its own deadline.
    pub async fn wait(&self, container_id: &str, exec_id: &str) -> Result<ExitStatus> {
        let exec = self.get(container_id, exec_id).await?;
        // Not started yet is not an error: containerd waits first and starts after, so this blocks until the process has run and exited
        // (or the exec is deleted, which ends the wait with 137).
        let mut rx = exec.lock().await.exit_rx.clone();
        loop {
            let current = rx.borrow().clone();
            if let Some(s) = current {
                exec.lock().await.state = State::Exited;
                return Ok(s);
            }
            if rx.changed().await.is_err() {
                // The task that watched the process ended without a status.
                exec.lock().await.state = State::Exited;
                return Ok(ExitStatus {
                    code: 137,
                    exited_at: chrono::Utc::now().to_rfc3339(),
                });
            }
        }
    }

    /// Write to the process's stdin (or its terminal). Returns the bytes written.
    pub async fn write(&self, container_id: &str, exec_id: &str, data: &[u8]) -> Result<usize> {
        if data.len() > MAX_WRITE {
            anyhow::bail!("a write is larger than {MAX_WRITE} bytes");
        }
        let exec = self.get(container_id, exec_id).await?;
        let mut e = exec.lock().await;
        if e.state != State::Running {
            anyhow::bail!("exec {exec_id} is not running");
        }
        match e.stdin.as_mut() {
            None => anyhow::bail!("stdin of exec {exec_id} is closed"),
            Some(Stdin::Pipe(w)) => {
                tokio::time::timeout(WRITE_WAIT, async {
                    w.write_all(data).await?;
                    w.flush().await
                })
                .await
                .map_err(|_| anyhow::anyhow!("the process does not read its stdin"))?
                .context("write to stdin")?;
                Ok(data.len())
            }
            Some(Stdin::Pty(m)) => {
                let m = m.clone();
                drop(e);
                tokio::time::timeout(WRITE_WAIT, write_master(&m, data))
                    .await
                    .map_err(|_| anyhow::anyhow!("the terminal does not take input"))?
                    .context("write to the terminal")?;
                Ok(data.len())
            }
        }
    }

    /// End of input. A terminal has no end of input (the client sends ^D if it wants one).
    pub async fn close_stdin(&self, container_id: &str, exec_id: &str) -> Result<()> {
        let exec = self.get(container_id, exec_id).await?;
        let mut e = exec.lock().await;
        if matches!(e.stdin, Some(Stdin::Pipe(_))) {
            e.stdin = None; // dropped: the pipe closes
        }
        Ok(())
    }

    /// The output since the offsets (see `ContainerLogs::read`).
    pub async fn read(
        &self,
        container_id: &str,
        exec_id: &str,
        stdout_offset: u64,
        stderr_offset: u64,
        max: usize,
    ) -> Result<crate::container::LogRead> {
        let exec = self.get(container_id, exec_id).await?;
        let logs = exec.lock().await.logs.clone();
        let max = if max == 0 {
            DEFAULT_READ
        } else {
            max.min(MAX_READ)
        };
        let r = logs.lock().await.read(stdout_offset, stderr_offset, max);
        Ok(r)
    }

    /// Set the size of the terminal.
    pub async fn resize(
        &self,
        container_id: &str,
        exec_id: &str,
        width: u32,
        height: u32,
    ) -> Result<()> {
        if !valid_terminal_size(width, height) {
            anyhow::bail!("invalid terminal size");
        }
        let exec = self.get(container_id, exec_id).await?;
        let e = exec.lock().await;
        let master = e
            .master
            .clone()
            .ok_or_else(|| anyhow::anyhow!("exec {exec_id} has no terminal"))?;
        set_window_size(master.fd.get_ref().as_raw_fd(), width as u16, height as u16)
            .context("TIOCSWINSZ")
    }

    /// Kill the process if it still runs and forget the exec. Not an error if there is none.
    pub async fn delete(&self, container_id: &str, exec_id: &str) -> Result<()> {
        let k = key(container_id, exec_id)?;
        let Some(exec) = self.execs.lock().await.remove(&k) else {
            return Ok(());
        };
        finish_exec(exec).await;
        Ok(())
    }

    /// Delete every exec of a container (the container is going away).
    pub async fn remove_container(&self, container_id: &str) {
        let taken: Vec<Arc<Mutex<Exec>>> = {
            let mut execs = self.execs.lock().await;
            let keys: Vec<Key> = execs
                .keys()
                .filter(|(c, _)| c == container_id)
                .cloned()
                .collect();
            keys.iter().filter_map(|k| execs.remove(k)).collect()
        };
        for exec in taken {
            finish_exec(exec).await;
        }
    }

    /// The pids of the agent's children that run execs (and the processes), so that the zombie reaper leaves them to their waits.
    pub async fn tracked_pids(&self) -> std::collections::HashSet<u32> {
        let all: Vec<Arc<Mutex<Exec>>> = self.execs.lock().await.values().cloned().collect();
        let mut out = std::collections::HashSet::new();
        for e in all {
            if let Ok(e) = e.try_lock() {
                if e.crun_pid != 0 {
                    out.insert(e.crun_pid);
                }
                if e.pid != 0 {
                    out.insert(e.pid);
                }
            } else {
                // Being started or stopped just now: its pids are not known; a zombie must have been one at two scans.
            }
        }
        out
    }

    /// How many execs there are (for tests).
    #[cfg(test)]
    pub async fn count(&self) -> usize {
        self.execs.lock().await.len()
    }
}

/// Stop what an exec left: the process, the descriptors, the files.
async fn finish_exec(exec: Arc<Mutex<Exec>>) {
    let mut e = exec.lock().await;
    let alive = e.state == State::Running && e.exit_rx.borrow().is_none();
    if alive {
        // By the pidfd when there is one; otherwise by the pid, which is the process's while crun has not been waited for.
        match &e.pidfd {
            Some(fd) => {
                pidfd_send_signal(fd.as_raw_fd(), libc::SIGKILL);
            }
            None => kill_pid(e.pid, libc::SIGKILL),
        }
        // Give the exit a moment to be seen (the waiter task reaps crun).
        let mut rx = e.exit_rx.clone();
        drop(e);
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while rx.borrow().is_none() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
        e = exec.lock().await;
    }
    e.state = State::Exited;
    // An exec that never started has nobody to report an exit: whoever waits for it is told 137 (killed).
    if let Some(tx) = e.exit_tx.take() {
        let _ = tx.send(Some(ExitStatus {
            code: 137,
            exited_at: chrono::Utc::now().to_rfc3339(),
        }));
    }
    e.logs.lock().await.eof = true;
    e.stdin = None;
    e.master = None;
    e.pidfd = None;
    remove_files(&mut e.files);
}

fn remove_files(files: &mut Vec<PathBuf>) {
    for f in files.drain(..) {
        let _ = std::fs::remove_file(f);
    }
}

fn kill_pid(pid: u32, sig: i32) {
    // Never pid 0 (our own group), 1 (the agent) or something that does not fit a pid.
    if pid > 1 && pid <= i32::MAX as u32 {
        unsafe { libc::kill(pid as i32, sig) };
    }
}

// ── files ───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────

pub(crate) fn make_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("create {}", dir.display())),
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// A new file, mode 0600 from the start, that must not exist.
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    f.write_all(data)?;
    Ok(())
}

/// The pid crun writes (as soon as it has made the process). Waits until it is there, or crun has ended without it, or the time is up.
async fn wait_for_pid(
    pid_file: &Path,
    mut exit: watch::Receiver<Option<ExitStatus>>,
) -> Result<u32> {
    let deadline = tokio::time::Instant::now() + START_WAIT;
    let mut delay = Duration::from_millis(2);
    loop {
        if let Some(pid) = read_pid(pid_file) {
            return Ok(pid);
        }
        if exit.borrow().is_some() {
            // crun is over; the file may have been written just before.
            return read_pid(pid_file)
                .ok_or_else(|| anyhow::anyhow!("crun exited before the process started"));
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "crun did not start the process within {} s",
                START_WAIT.as_secs()
            );
        }
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = exit.changed() => {}
        }
        delay = (delay * 2).min(Duration::from_millis(50));
    }
}

/// A pid file's content: a positive number that fits a pid.
pub fn parse_pid(text: &str) -> Option<u32> {
    let pid: u32 = text.trim().parse().ok()?;
    (pid > 1 && pid <= i32::MAX as u32).then_some(pid)
}

fn read_pid(path: &Path) -> Option<u32> {
    parse_pid(&std::fs::read_to_string(path).ok()?)
}

// ── pipes and the terminal ──────────────────────────────────────────────────────────────────────────────────────────────────────

fn spawn_pipe_reader<R>(
    mut reader: R,
    logs: Arc<Mutex<ContainerLogs>>,
    readers: Arc<Readers>,
    is_err: bool,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    readers.add();
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut l = logs.lock().await;
                    if is_err {
                        l.push_stderr(&buf[..n]);
                    } else {
                        l.push_stdout(&buf[..n]);
                    }
                }
            }
        }
        readers.finish();
    });
}

pub(crate) async fn read_master(master: Arc<Master>, logs: Arc<Mutex<ContainerLogs>>) {
    let mut buf = [0u8; 4096];
    loop {
        let mut guard = match master.fd.readable().await {
            Ok(g) => g,
            Err(_) => break,
        };
        let r = guard.try_io(|inner| {
            let n = unsafe {
                libc::read(
                    inner.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        });
        match r {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => logs.lock().await.push_stdout(&buf[..n]),
            // EIO: every slave side is closed (the process and its children are gone).
            Ok(Err(_)) => break,
            Err(_would_block) => continue,
        }
    }
}

pub(crate) async fn write_master(master: &Master, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        let mut guard = master.fd.writable().await?;
        let r = guard.try_io(|inner| {
            let n = unsafe {
                libc::write(
                    inner.as_raw_fd(),
                    data.as_ptr() as *const libc::c_void,
                    data.len(),
                )
            };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        });
        match r {
            Ok(Ok(n)) => data = &data[n..],
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => continue,
        }
    }
    Ok(())
}

/// Write to a terminal's master, giving up if it does not take the data (the process is not reading its terminal).
pub(crate) async fn write_terminal(master: &Master, data: &[u8]) -> Result<usize> {
    if data.len() > MAX_WRITE {
        anyhow::bail!("a write is larger than {MAX_WRITE} bytes");
    }
    tokio::time::timeout(WRITE_WAIT, write_master(master, data))
        .await
        .map_err(|_| anyhow::anyhow!("the terminal does not take input"))?
        .context("write to the terminal")?;
    Ok(data.len())
}

/// The size limit of a terminal, checked before it is set.
pub(crate) fn valid_terminal_size(width: u32, height: u32) -> bool {
    width != 0 && height != 0 && width <= 10_000 && height <= 10_000
}

/// Set the size of a terminal by its master.
pub fn set_window_size(fd: RawFd, cols: u16, rows: u16) -> io::Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Receive the terminal's master from crun on the console socket. Gives up when crun has ended without sending it, or after
/// START_WAIT.
pub(crate) async fn receive_master(
    listener: std::os::unix::net::UnixListener,
    exit: watch::Receiver<Option<ExitStatus>>,
) -> Result<Master> {
    let fd = tokio::task::spawn_blocking(move || -> Result<OwnedFd> {
        listener.set_nonblocking(true)?;
        let deadline = std::time::Instant::now() + START_WAIT;
        let conn = loop {
            match listener.accept() {
                Ok((c, _)) => break c,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if exit.borrow().is_some() {
                        anyhow::bail!("crun exited without a terminal");
                    }
                    if std::time::Instant::now() >= deadline {
                        anyhow::bail!(
                            "crun did not send a terminal within {} s",
                            START_WAIT.as_secs()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(e).context("accept on the console socket"),
            }
        };
        conn.set_nonblocking(false)?;
        conn.set_read_timeout(Some(START_WAIT))?;
        recv_fd(conn.as_raw_fd())
    })
    .await
    .context("receiving the terminal panicked")??;
    set_nonblocking(fd.as_raw_fd())?;
    Ok(Master {
        // SAFETY: `fd` is the open terminal master, and the AsyncFd owns it until it is dropped.
        fd: unsafe { AsyncFd::register(fd) }
            .map_err(std::io::Error::from)
            .context("register the terminal")?,
    })
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Receive one file descriptor (SCM_RIGHTS) on a connected unix socket. Anything else received is closed.
pub fn recv_fd(sock: RawFd) -> Result<OwnedFd> {
    let mut data = [0u8; 4096];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    // Room for a few descriptors, so that an unexpected extra one is received (and closed) rather than lost in the kernel.
    let space = unsafe { libc::CMSG_SPACE((4 * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;
    let n = unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error()).context("recvmsg");
    }
    let mut got: Vec<OwnedFd> = Vec::new();
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let len = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let p = libc::CMSG_DATA(c) as *const RawFd;
                for i in 0..len / std::mem::size_of::<RawFd>() {
                    got.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(p.add(i))));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    let mut it = got.into_iter();
    it.next()
        .ok_or_else(|| anyhow::anyhow!("no descriptor was sent"))
}

// ── pidfd ───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────

fn pidfd_open(pid: u32) -> Option<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as i32, 0) };
    if fd < 0 {
        warn!("pidfd_open({pid}): {}", io::Error::last_os_error());
        return None;
    }
    Some(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Signal the process behind a pidfd. False if it is gone.
fn pidfd_send_signal(fd: RawFd, sig: i32) -> bool {
    let r = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd,
            sig,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    r == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids_are_plain() {
        assert!(valid_exec_id("abc-123_XYZ"));
        assert!(!valid_exec_id(""));
        assert!(!valid_exec_id("../x"));
        assert!(!valid_exec_id("a/b"));
        assert!(!valid_exec_id("a.b"), "no dots in an exec id");
        assert!(!valid_exec_id(".hidden"));
        assert!(!valid_exec_id(&"a".repeat(65)));
        assert!(key("c1", "e1").is_ok());
        assert!(key("c/1", "e1").is_err());
        assert!(key("c1", "e 1").is_err());
    }

    #[test]
    fn a_pid_file_holds_a_pid_that_can_be_signalled() {
        assert_eq!(parse_pid("1234\n"), Some(1234));
        assert_eq!(parse_pid(" 7 "), Some(7));
        for bad in [
            "",
            "0",
            "1",
            "-1",
            "abc",
            "4294967295",
            "2147483648",
            "12 34",
        ] {
            assert_eq!(parse_pid(bad), None, "{bad:?}");
        }
    }

    fn proc_json() -> Vec<u8> {
        json!({"args": ["sh", "-c", "true"], "env": ["A=b"], "cwd": "/"})
            .to_string()
            .into_bytes()
    }

    #[tokio::test]
    async fn create_checks_the_definition_and_limits_the_execs_of_a_container() {
        let m = ExecManager::new();
        assert!(m
            .create("c1", "e1", &proc_json(), false, None)
            .await
            .is_ok());
        // The same id twice, a bad id, a bad definition, a definition too large.
        assert!(m
            .create("c1", "e1", &proc_json(), false, None)
            .await
            .is_err());
        assert!(m
            .create("c1", "../e", &proc_json(), false, None)
            .await
            .is_err());
        assert!(m
            .create("c1", "e2", b"not json", false, None)
            .await
            .is_err());
        assert!(m
            .create("c1", "e2", b"{\"args\": []}", false, None)
            .await
            .is_err());
        let big = vec![b' '; MAX_PROCESS_JSON + 1];
        assert!(m.create("c1", "e2", &big, false, None).await.is_err());
        // Up to the limit per container; another container is not affected.
        for i in 2..=MAX_EXECS_PER_CONTAINER {
            m.create("c1", &format!("e{i}"), &proc_json(), false, None)
                .await
                .unwrap();
        }
        assert!(m
            .create("c1", "one-more", &proc_json(), false, None)
            .await
            .is_err());
        assert!(m
            .create("c2", "e1", &proc_json(), false, None)
            .await
            .is_ok());
        assert_eq!(m.count().await, MAX_EXECS_PER_CONTAINER + 1);
        // Deleting frees a place, and deleting what is not there is not an error.
        m.delete("c1", "e1").await.unwrap();
        m.delete("c1", "e1").await.unwrap();
        assert!(m
            .create("c1", "one-more", &proc_json(), false, None)
            .await
            .is_ok());
        m.remove_container("c1").await;
        assert_eq!(m.count().await, 1);
    }

    #[tokio::test]
    async fn an_exec_that_was_not_started_cannot_be_killed_written_to_or_waited_for() {
        let m = ExecManager::new();
        m.create("c1", "e1", &proc_json(), false, None)
            .await
            .unwrap();
        assert!(m.kill("c1", "e1", 15).await.is_err());
        assert!(m.kill("c1", "e1", 0).await.is_err(), "signal 0 is refused");
        assert!(m.kill("c1", "e1", 65).await.is_err());
        assert!(m.write("c1", "e1", b"x").await.is_err());
        // Waiting for an exec that was not started blocks (containerd waits before it starts); deleting it ends the wait with 137.
        let m = Arc::new(m);
        let waiter = {
            let m = m.clone();
            tokio::spawn(async move { m.wait("c1", "e1").await })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!waiter.is_finished(), "the wait blocks");
        m.delete("c1", "e1").await.unwrap();
        let st = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("the wait ended")
            .unwrap()
            .unwrap();
        assert_eq!(st.code, 137);
        m.create("c1", "e1", &proc_json(), false, None)
            .await
            .unwrap();
        assert!(m.resize("c1", "e1", 80, 24).await.is_err(), "no terminal");
        assert!(m.resize("c1", "e1", 0, 24).await.is_err());
        assert!(m.kill("c1", "unknown", 15).await.is_err());
        // Reading shows nothing, and no end of output yet.
        let r = m.read("c1", "e1", 0, 0, 0).await.unwrap();
        assert!(r.stdout.is_empty() && r.stderr.is_empty() && !r.eof);
        // A write over the limit is refused before anything else.
        let big = vec![0u8; MAX_WRITE + 1];
        assert!(m.write("c1", "e1", &big).await.is_err());
    }

    #[test]
    fn the_window_size_of_a_real_terminal_is_set() {
        let (mut master, mut slave) = (-1, -1);
        let r = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if r != 0 {
            return; // no ptys here (a sandboxed test run)
        }
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        set_window_size(master.as_raw_fd(), 132, 43).unwrap();
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) },
            0
        );
        assert_eq!((ws.ws_col, ws.ws_row), (132, 43));
    }

    #[test]
    fn a_descriptor_sent_over_a_unix_socket_is_received_and_extra_ones_are_closed() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;
        let (a, b) = UnixStream::pair().unwrap();
        // A pipe: send its read end, keep the write end.
        let mut fds = [0 as RawFd; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (rd, wr) = (fds[0], fds[1]);
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
        let mut control = vec![0u8; space];
        let data = b"/dev/pts/0";
        let mut iov = libc::iovec {
            iov_base: data.as_ptr() as *mut libc::c_void,
            iov_len: data.len(),
        };
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space as _;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c) as *mut RawFd, rd);
            assert!(libc::sendmsg(a.as_raw_fd(), &msg, 0) > 0);
        }
        let got = recv_fd(b.as_raw_fd()).unwrap();
        // It is the same pipe: what is written to the write end is read from the received descriptor.
        unsafe { libc::write(wr, b"hi".as_ptr() as *const libc::c_void, 2) };
        let mut f = std::fs::File::from(got);
        let mut buf = [0u8; 2];
        f.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hi");
        // Nothing sent: an error, not a hang or a bad descriptor.
        let (mut c, d) = UnixStream::pair().unwrap();
        c.write_all(b"x").unwrap();
        assert!(recv_fd(d.as_raw_fd()).is_err());
        unsafe {
            libc::close(rd);
            libc::close(wr);
        }
    }

    #[tokio::test]
    async fn readers_count_down_to_the_end_of_the_output() {
        let r = Arc::new(Readers::default());
        r.wait().await; // none: returns at once
        r.add();
        r.add();
        let r2 = r.clone();
        let t = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            r2.finish();
            r2.finish();
        });
        tokio::time::timeout(Duration::from_secs(2), r.wait())
            .await
            .expect("waited for both readers");
        t.await.unwrap();
    }

    #[test]
    fn files_are_private_and_not_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("cloudhv-exec-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        make_dir(&d).unwrap();
        assert_eq!(
            std::fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let f = d.join("x1.json");
        write_private(&f, b"{}").unwrap();
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            write_private(&f, b"{}").is_err(),
            "an existing file is not written"
        );
        std::os::unix::fs::symlink("/etc/passwd", d.join("x2.json")).unwrap();
        assert!(
            write_private(&d.join("x2.json"), b"{}").is_err(),
            "a link is not followed"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A pty pair as a `Master` (nonblocking, registered) and the slave side.
    fn pty() -> Option<(Arc<Master>, OwnedFd)> {
        let (mut master, mut slave) = (-1, -1);
        let r = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if r != 0 {
            return None; // no ptys here (a sandboxed test run)
        }
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        set_nonblocking(master.as_raw_fd()).unwrap();
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        Some((
            Arc::new(Master {
                // SAFETY: `master` is an open terminal master, and the AsyncFd owns it until it is dropped.
                fd: unsafe { AsyncFd::register(master) }
                    .map_err(std::io::Error::from)
                    .unwrap(),
            }),
            slave,
        ))
    }

    #[tokio::test]
    async fn the_terminal_of_a_container_takes_input_gives_output_and_is_resized() {
        let Some((master, slave)) = pty() else { return };
        // Output: what the process writes to its terminal reaches the container's log (stdout stream).
        let logs = Arc::new(Mutex::new(ContainerLogs::default()));
        let reader = tokio::spawn(read_master(master.clone(), logs.clone()));
        unsafe {
            libc::write(
                slave.as_raw_fd(),
                b"hello\n".as_ptr() as *const libc::c_void,
                6,
            )
        };
        for _ in 0..100 {
            if logs.lock().await.stdout.len() >= 7 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let got = logs.lock().await.read(0, 0, 4096);
        assert_eq!(
            got.stdout, b"hello\r\n",
            "the line discipline turns a newline into CR LF"
        );
        assert!(got.stderr.is_empty(), "a terminal has no stderr");

        // Input: written to the master, read from the slave (raw mode, so that the line discipline does not hold it back).
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        unsafe {
            libc::tcgetattr(slave.as_raw_fd(), &mut t);
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &t);
        }
        assert_eq!(write_terminal(&master, b"abc").await.unwrap(), 3);
        let mut buf = [0u8; 3];
        let mut n = 0;
        for _ in 0..100 {
            let r = unsafe {
                libc::read(
                    slave.as_raw_fd(),
                    buf.as_mut_ptr().add(n) as *mut libc::c_void,
                    3 - n,
                )
            };
            if r > 0 {
                n += r as usize;
            }
            if n == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(&buf, b"abc");
        // A write over the limit is refused.
        assert!(write_terminal(&master, &vec![0u8; MAX_WRITE + 1])
            .await
            .is_err());

        // Size.
        assert!(valid_terminal_size(80, 24));
        assert!(
            !valid_terminal_size(0, 24)
                && !valid_terminal_size(80, 0)
                && !valid_terminal_size(10_001, 24)
        );
        set_window_size(master.raw_fd(), 100, 30).unwrap();
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) },
            0
        );
        assert_eq!((ws.ws_col, ws.ws_row), (100, 30));

        // The reader ends when the slave side is gone (the process and its children have exited).
        drop(slave);
        tokio::time::timeout(Duration::from_secs(3), reader)
            .await
            .expect("the reader ended")
            .unwrap();
    }

    #[tokio::test]
    async fn the_init_exec_id_is_not_an_exec_process() {
        let m = ExecManager::new();
        assert!(valid_exec_id(INIT_EXEC_ID), "it is an id the host can send");
        let err = m
            .create("c1", INIT_EXEC_ID, br#"{"args":["ls"]}"#, false, None)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("reserved"), "{err:#}");
        assert_eq!(m.count().await, 0);
        // Nothing but write, close and resize means anything for it; the exec manager does not know it.
        assert!(m.start("c1", INIT_EXEC_ID).await.is_err());
        assert!(m.kill("c1", INIT_EXEC_ID, 9).await.is_err());
        assert!(m.read("c1", INIT_EXEC_ID, 0, 0, 0).await.is_err());
    }
}
