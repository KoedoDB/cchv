//! Exec processes: `kubectl exec`, exec probes. One `ExecProc` is one process in a container of the VM.
//!
//! The process runs in the guest; this shim asks the daemon (which asks the guest agent) to create, start, signal and wait for it, and
//! carries its standard streams between the guest and the named pipes containerd made for it. The guest keeps the output until it
//! is read, so the shim polls (`ExecRead`, the way `pump_logs` polls `ReadLogs`): at once while data is flowing, then less often, to
//! at most every 100 ms. The input goes the other way, in `ExecWrite` calls.
//!
//! The order the shimkit task service keeps (create, start, exited, delete) is checked there; this is the part that talks to the daemon.

use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use containerd_shimkit::sandbox::sync::WaitableCell;
use containerd_shimkit::sandbox::{Error, ExecConfig};
use log::{info, warn};
use tokio::io::unix::AsyncFd;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::watch;

use crate::daemon_client::DaemonClient;

/// The exec id for the container's own terminal in the daemon's API (see the daemon's `exec.rs`).
pub const INIT_EXEC_ID: &str = "init";

/// At most this much of each stream per call, in either direction.
const CHUNK: usize = 64 * 1024;
/// A pipe nobody reads blocks a write once it is full; the stream is then dropped (as in `pump_logs`).
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
/// How long, after the process has exited, the rest of its output is waited for before the exit is reported.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(15);
/// How long `CloseIO` waits for the input already in the pipe to be forwarded.
const STDIN_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Consecutive failed calls after which a pump gives up (the VM is gone, or the daemon).
const MAX_FAILURES: u32 = 5;

pub struct ExecProc {
    pub cfg: ExecConfig,
    client: DaemonClient,
    vm_id: String,
    container_id: String,
    /// Set when the output pump has delivered everything (or given up).
    out_done: WaitableCell<()>,
    /// Set when the input pump has ended.
    in_done: WaitableCell<()>,
    /// `true` stops both pumps (the exec is deleted).
    cancel: watch::Sender<bool>,
    /// `true`: forward what is in the stdin pipe now, send "close", and end.
    close_stdin: watch::Sender<bool>,
    started: AtomicBool,
}

impl ExecProc {
    pub fn new(
        cfg: ExecConfig,
        client: DaemonClient,
        vm_id: String,
        container_id: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            client,
            vm_id,
            container_id,
            out_done: WaitableCell::new(),
            in_done: WaitableCell::new(),
            cancel: watch::channel(false).0,
            close_stdin: watch::channel(false).0,
            started: AtomicBool::new(false),
        })
    }

    fn id(&self) -> &str {
        &self.cfg.exec_id
    }

    fn err(e: anyhow::Error) -> Error {
        Error::Any(e)
    }

    /// The input of the container's own process, for a container with a terminal: what is written to its stdin pipe is forwarded to the
    /// terminal (the exec id `init`). Its output is not handled here (`pump_logs` does that), and nothing else is asked of the daemon.
    pub fn for_init(
        container_id: &str,
        stdin: std::path::PathBuf,
        client: DaemonClient,
        vm_id: String,
    ) -> Arc<Self> {
        Self::new(
            ExecConfig {
                exec_id: INIT_EXEC_ID.to_string(),
                terminal: true,
                stdin,
                ..Default::default()
            },
            client,
            vm_id,
            container_id.to_string(),
        )
    }

    /// Start forwarding the stdin pipe (see `for_init`).
    pub fn start_input_only(self: &Arc<Self>) {
        self.started.store(true, Ordering::SeqCst);
        if self.cfg.stdin.as_os_str().is_empty() {
            let _ = self.in_done.set(());
            return;
        }
        let me = self.clone();
        tokio::spawn(async move { me.pump_input().await });
    }

    /// Stop the pumps; nothing is asked of the daemon.
    pub fn stop(&self) {
        let _ = self.cancel.send(true);
    }

    pub async fn create(&self) -> Result<(), Error> {
        self.client
            .exec_create(
                &self.vm_id,
                &self.container_id,
                self.id(),
                &self.cfg.spec,
                self.cfg.terminal,
            )
            .await
            .map_err(Self::err)
    }

    /// Start the process and the pumps for its streams. Returns the pid in the guest.
    pub async fn start(self: &Arc<Self>) -> Result<u32, Error> {
        let pid = self
            .client
            .exec_start(&self.vm_id, &self.container_id, self.id())
            .await
            .map_err(Self::err)?;
        self.started.store(true, Ordering::SeqCst);
        info!(
            "exec {} of {} started in the guest (pid {pid})",
            self.id(),
            self.container_id
        );

        let me = self.clone();
        tokio::spawn(async move { me.pump_output().await });
        if self.cfg.stdin.as_os_str().is_empty() {
            let _ = self.in_done.set(());
        } else {
            let me = self.clone();
            tokio::spawn(async move { me.pump_input().await });
        }
        Ok(pid)
    }

    /// Wait for the process to exit, then for its output to have been written to the pipes: containerd takes the exit as the end of the
    /// streams, so the exit is reported last.
    pub async fn wait(&self) -> Result<(u32, DateTime<Utc>), Error> {
        let exit = self
            .client
            .exec_wait(&self.vm_id, &self.container_id, self.id())
            .await
            .map_err(Self::err)?;
        if tokio::time::timeout(DRAIN_TIMEOUT, self.out_done.wait())
            .await
            .is_err()
        {
            warn!(
                "exec {}: its output was not all delivered {} s after it exited",
                self.id(),
                DRAIN_TIMEOUT.as_secs()
            );
        }
        Ok(exit)
    }

    pub async fn kill(&self, signal: u32) -> Result<(), Error> {
        self.client
            .exec_kill(&self.vm_id, &self.container_id, self.id(), signal)
            .await
            .map_err(Self::err)
    }

    pub async fn resize(&self, width: u32, height: u32) -> Result<(), Error> {
        self.client
            .exec_resize(&self.vm_id, &self.container_id, self.id(), width, height)
            .await
            .map_err(Self::err)
    }

    /// `CloseIO` for stdin: what is in the pipe is forwarded first, then the guest's input is closed.
    pub async fn close_stdin(&self) -> Result<(), Error> {
        if self.cfg.stdin.as_os_str().is_empty() || !self.started.load(Ordering::SeqCst) {
            return self
                .client
                .exec_close_stdin(&self.vm_id, &self.container_id, self.id())
                .await
                .map_err(Self::err);
        }
        let _ = self.close_stdin.send(true);
        if tokio::time::timeout(STDIN_CLOSE_TIMEOUT, self.in_done.wait())
            .await
            .is_err()
        {
            warn!("exec {}: the input pump did not finish closing", self.id());
        }
        Ok(())
    }

    /// The exec is over: stop the pumps and let the daemon drop it. A daemon that does not know it (the VM is gone) is not an error.
    pub async fn delete(&self) -> Result<(), Error> {
        let _ = self.cancel.send(true);
        if let Err(e) = self
            .client
            .exec_delete(&self.vm_id, &self.container_id, self.id())
            .await
        {
            warn!("deleting exec {} in the daemon: {e:#}", self.id());
        }
        Ok(())
    }

    // ── streams ─────────────────────────────────────────────────────────────

    /// Copy the process's output to the stdout and stderr pipes until it has ended and everything has been written.
    async fn pump_output(self: Arc<Self>) {
        let open = |cfg: ExecConfig, stderr: bool| async move {
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
        let mut out = open(self.cfg.clone(), false).await;
        // With a terminal everything is on stdout and there is no stderr pipe.
        let mut err = if self.cfg.terminal {
            None
        } else {
            open(self.cfg.clone(), true).await
        };

        let mut cancel = self.cancel.subscribe();
        let (mut so, mut se) = (0u64, 0u64);
        let (mut failures, mut wait) = (0u32, Duration::from_millis(20));
        loop {
            if *cancel.borrow() {
                break;
            }
            match self
                .client
                .exec_read(&self.vm_id, &self.container_id, self.id(), so, se, CHUNK)
                .await
            {
                Ok(chunk) => {
                    failures = 0;
                    if chunk.dropped > 0 {
                        warn!(
                            "exec {}: the guest dropped {} bytes of output nobody read in time",
                            self.id(),
                            chunk.dropped
                        );
                    }
                    let got = !chunk.stdout.is_empty() || !chunk.stderr.is_empty();
                    write_to(&mut out, &chunk.stdout).await;
                    write_to(&mut err, &chunk.stderr).await;
                    so = chunk.stdout_offset;
                    se = chunk.stderr_offset;
                    if chunk.eof {
                        break;
                    }
                    // With no pipe to write to, the output is still read (and so dropped in the guest), so that the guest does not
                    // hold it.
                    if got {
                        wait = Duration::from_millis(20);
                    } else {
                        wait = (wait * 2).min(Duration::from_millis(100));
                        tokio::select! {
                            _ = tokio::time::sleep(wait) => {}
                            _ = cancel.changed() => {}
                        }
                    }
                }
                Err(e) => {
                    failures += 1;
                    if failures >= MAX_FAILURES {
                        warn!("the output pump of exec {} stops: {e:#}", self.id());
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
        let _ = self.out_done.set(());
    }

    /// Forward what containerd's client writes to the stdin pipe, and the end of it (`close_stdin`).
    ///
    /// The pipe is opened read-write (opening does not wait for a writer, and the pipe never reports end-of-file by itself): the end
    /// comes from `CloseIO`. It is read without blocking, through `AsyncFd`, so that stopping the pump does not leave a thread stuck in
    /// a read.
    async fn pump_input(self: Arc<Self>) {
        let _done = self.in_done.set_guard_with(|| ());
        let open = {
            let path = self.cfg.stdin.clone();
            tokio::task::spawn_blocking(move || {
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(path)
            })
        };
        let file = match tokio::time::timeout(OPEN_TIMEOUT, open).await {
            Ok(Ok(Ok(f))) => f,
            other => {
                warn!(
                    "exec {}: cannot open its stdin pipe: {:?}",
                    self.id(),
                    other.map(|r| r.map(|r| r.map(|_| ())))
                );
                return;
            }
        };
        // SAFETY: `file` owns its descriptor, which is open, and nothing else uses it.
        let afd = match unsafe { AsyncFd::register(file) } {
            Ok(a) => a,
            Err(e) => {
                warn!("exec {}: stdin pipe: {e}", self.id());
                return;
            }
        };
        let mut cancel = self.cancel.subscribe();
        let mut close = self.close_stdin.subscribe();
        let mut buf = vec![0u8; CHUNK];
        let mut failures = 0u32;
        let mut closing = false;
        loop {
            // Read what is there. When closing, read until the pipe is empty, then close.
            let read = if closing {
                match read_file(afd.get_ref(), &mut buf) {
                    Ok(n) => Some(Ok(n)),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
                    Err(e) => Some(Err(e)),
                }
            } else {
                tokio::select! {
                    _ = cancel.changed() => return,
                    _ = close.changed() => { closing = true; continue; }
                    guard = afd.readable() => match guard {
                        Ok(mut g) => match g.try_io(|inner| read_file(inner.get_ref(), &mut buf)) {
                            Ok(r) => Some(r),
                            Err(_would_block) => continue,
                        },
                        Err(e) => Some(Err(e)),
                    },
                }
            };
            match read {
                Some(Ok(0)) => {
                    // End of file (every writer closed and ours too: not expected with a read-write open): treat as the end of input.
                    closing = true;
                }
                Some(Ok(n)) => {
                    if self
                        .client
                        .exec_write(&self.vm_id, &self.container_id, self.id(), &buf[..n])
                        .await
                        .is_err()
                    {
                        failures += 1;
                        if failures >= MAX_FAILURES {
                            return;
                        }
                    } else {
                        failures = 0;
                    }
                }
                Some(Err(e)) => {
                    warn!("exec {}: reading its stdin pipe: {e}", self.id());
                    closing = true;
                }
                None => {
                    // Nothing left: close.
                    if let Err(e) = self
                        .client
                        .exec_close_stdin(&self.vm_id, &self.container_id, self.id())
                        .await
                    {
                        warn!("exec {}: closing its input: {e:#}", self.id());
                    }
                    return;
                }
            }
            if closing && *cancel.borrow() {
                return;
            }
        }
    }
}

/// `read` on a shared reference to a file (`Read` is implemented for `&File`, which needs a mutable binding).
fn read_file(f: &std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut r = f;
    r.read(buf)
}

/// Write `data` to a pipe. A pipe whose reader has gone, or that does not take the data in time, is dropped.
async fn write_to(pipe: &mut Option<tokio::fs::File>, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    if let Some(f) = pipe.as_mut() {
        let wrote = tokio::time::timeout(WRITE_TIMEOUT, async {
            f.write_all(data).await?;
            f.flush().await
        })
        .await;
        if !matches!(wrote, Ok(Ok(()))) {
            *pipe = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use base64::Engine as _;
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tokio::sync::Notify;

    use super::*;

    const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

    /// A daemon that answers the Exec* calls, and keeps the requests it got.
    struct FakeDaemon {
        socket: PathBuf,
        calls: Arc<Mutex<Vec<Value>>>,
        /// The answers to `ExecRead`, in order; when they are used up the answer is "end of output".
        reads: Arc<Mutex<VecDeque<Value>>>,
        /// `ExecWait` answers when this is notified.
        exit_gate: Arc<Notify>,
        /// When set, `ExecRead` keeps answering "nothing yet" (a process that goes on).
        endless: Arc<AtomicBool>,
        _dir: tempfile::TempDir,
    }

    impl FakeDaemon {
        async fn start() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("d.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let calls: Arc<Mutex<Vec<Value>>> = Arc::default();
            let reads: Arc<Mutex<VecDeque<Value>>> = Arc::default();
            let exit_gate = Arc::new(Notify::new());
            let endless = Arc::new(AtomicBool::new(false));
            let (c, r, g, e) = (
                calls.clone(),
                reads.clone(),
                exit_gate.clone(),
                endless.clone(),
            );
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let (c, r, g, e) = (c.clone(), r.clone(), g.clone(), e.clone());
                    tokio::spawn(async move {
                        let (rd, mut wr) = stream.into_split();
                        let mut line = String::new();
                        if BufReader::new(rd).read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let req: Value = serde_json::from_str(line.trim()).unwrap();
                        c.lock().unwrap().push(req.clone());
                        let resp = match req["method"].as_str().unwrap() {
                            "ExecStart" => json!({"pid": 77}),
                            "ExecWrite" => {
                                let n = B64.decode(req["data"].as_str().unwrap()).unwrap().len();
                                json!({"written": n})
                            }
                            "ExecWait" => {
                                g.notified().await;
                                json!({"exit_status": 3, "exited_at_unix_nanos": 1_700_000_000_000_000_000u64})
                            }
                            "ExecRead" => {
                                let next = r.lock().unwrap().pop_front();
                                match next {
                                    Some(v) => v,
                                    None if e.load(Ordering::SeqCst) => json!({
                                        "stdout": "", "stderr": "",
                                        "stdout_offset": req["stdout_offset"], "stderr_offset": req["stderr_offset"],
                                        "eof": false, "dropped": 0
                                    }),
                                    None => json!({
                                        "stdout": "", "stderr": "",
                                        "stdout_offset": req["stdout_offset"], "stderr_offset": req["stderr_offset"],
                                        "eof": true, "dropped": 0
                                    }),
                                }
                            }
                            _ => json!({}),
                        };
                        let _ = wr.write_all(format!("{resp}\n").as_bytes()).await;
                    });
                }
            });
            FakeDaemon {
                socket,
                calls,
                reads,
                exit_gate,
                endless,
                _dir: dir,
            }
        }

        fn client(&self) -> DaemonClient {
            DaemonClient::new(self.socket.to_str().unwrap())
        }

        fn methods(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|c| c["method"].as_str().unwrap().to_string())
                .collect()
        }

        fn calls_of(&self, method: &str) -> Vec<Value> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c["method"] == method)
                .cloned()
                .collect()
        }

        async fn wait_for(&self, method: &str, n: usize) {
            for _ in 0..200 {
                if self.calls_of(method).len() >= n {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("never saw {n} x {method}; saw {:?}", self.methods());
        }
    }

    fn chunk(stdout: &str, stderr: &str, so: u64, se: u64, eof: bool) -> Value {
        json!({"stdout": B64.encode(stdout), "stderr": B64.encode(stderr),
               "stdout_offset": so, "stderr_offset": se, "eof": eof, "dropped": 0})
    }

    fn fifo(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        nix::unistd::mkfifo(&p, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        p
    }

    /// The test's own end of a pipe (read-write, so that opening never waits).
    fn open_rw(p: &Path) -> std::fs::File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(p)
            .unwrap()
    }

    fn read_until(f: &mut std::fs::File, want: usize) -> Vec<u8> {
        let mut got = Vec::new();
        let mut buf = [0u8; 1024];
        for _ in 0..250 {
            match f.read(&mut buf) {
                Ok(n) if n > 0 => got.extend_from_slice(&buf[..n]),
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
            if got.len() >= want {
                break;
            }
        }
        got
    }

    fn cfg(id: &str, stdin: &Path, stdout: &Path, stderr: &Path, terminal: bool) -> ExecConfig {
        ExecConfig {
            exec_id: id.into(),
            spec: br#"{"args":["ls"]}"#.to_vec(),
            terminal,
            stdin: stdin.to_path_buf(),
            stdout: stdout.to_path_buf(),
            stderr: stderr.to_path_buf(),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn output_goes_to_the_pipes_and_the_exit_is_reported_after_it() {
        let d = FakeDaemon::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let (so, se) = (fifo(tmp.path(), "out"), fifo(tmp.path(), "err"));
        let (mut rso, mut rse) = (open_rw(&so), open_rw(&se));
        d.reads.lock().unwrap().extend([
            chunk("hello\n", "", 6, 0, false),
            chunk("", "oops\n", 6, 5, false),
            chunk("bye\n", "", 10, 5, true),
        ]);
        let p = ExecProc::new(
            cfg("e1", Path::new(""), &so, &se, false),
            d.client(),
            "vm1".into(),
            "c1".into(),
        );
        p.create().await.unwrap();
        assert_eq!(p.start().await.unwrap(), 77);
        d.exit_gate.notify_one();
        // `notify_one` before the call is waiting is kept for the next waiter
        let (status, at) = tokio::time::timeout(Duration::from_secs(10), p.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status, 3);
        assert_eq!(at.timestamp(), 1_700_000_000);
        // everything was in the pipes by the time wait returned
        assert_eq!(read_until(&mut rso, 10), b"hello\nbye\n");
        assert_eq!(read_until(&mut rse, 5), b"oops\n");
        // the reads acknowledged what had been taken
        let reads = d.calls_of("ExecRead");
        assert_eq!(
            (
                reads[0]["stdout_offset"].as_u64(),
                reads[0]["stderr_offset"].as_u64()
            ),
            (Some(0), Some(0))
        );
        assert_eq!(reads[1]["stdout_offset"].as_u64(), Some(6));
        assert_eq!(reads[2]["stderr_offset"].as_u64(), Some(5));
        assert_eq!(reads[0]["max"].as_u64(), Some(CHUNK as u64));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn requests_carry_the_ids_and_arguments_of_the_contract() {
        let d = FakeDaemon::start().await;
        let p = ExecProc::new(
            cfg("abc123", Path::new(""), Path::new(""), Path::new(""), true),
            d.client(),
            "vm9".into(),
            "ctr7".into(),
        );
        p.create().await.unwrap();
        p.start().await.unwrap();
        p.resize(120, 40).await.unwrap();
        p.kill(15).await.unwrap();
        p.close_stdin().await.unwrap(); // no stdin pipe: the daemon is asked straight away
        d.exit_gate.notify_one();
        p.wait().await.unwrap();
        p.delete().await.unwrap();

        let create = &d.calls_of("ExecCreate")[0];
        assert_eq!(create["vm_id"], "vm9");
        assert_eq!(create["container_id"], "ctr7");
        assert_eq!(create["exec_id"], "abc123");
        assert_eq!(create["terminal"], true);
        assert_eq!(
            B64.decode(create["process_json"].as_str().unwrap())
                .unwrap(),
            br#"{"args":["ls"]}"#
        );
        let resize = &d.calls_of("ExecResize")[0];
        assert_eq!(
            (resize["width"].as_u64(), resize["height"].as_u64()),
            (Some(120), Some(40))
        );
        assert_eq!(d.calls_of("ExecKill")[0]["signal"].as_u64(), Some(15));
        for m in ["ExecStart", "ExecCloseStdin", "ExecWait", "ExecDelete"] {
            let c = &d.calls_of(m)[0];
            assert_eq!(
                (
                    c["vm_id"].as_str(),
                    c["container_id"].as_str(),
                    c["exec_id"].as_str()
                ),
                (Some("vm9"), Some("ctr7"), Some("abc123")),
                "{m}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn input_is_forwarded_and_close_comes_after_it() {
        let d = FakeDaemon::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let si = fifo(tmp.path(), "in");
        d.endless.store(true, Ordering::SeqCst);
        let p = ExecProc::new(
            cfg("e2", &si, Path::new(""), Path::new(""), false),
            d.client(),
            "vm".into(),
            "c".into(),
        );
        p.create().await.unwrap();
        p.start().await.unwrap();

        let mut w = open_rw(&si);
        w.write_all(b"abc").unwrap();
        d.wait_for("ExecWrite", 1).await;
        // written just before the close: still forwarded first
        w.write_all(b"xyz").unwrap();
        p.close_stdin().await.unwrap();

        let writes: Vec<Vec<u8>> = d
            .calls_of("ExecWrite")
            .iter()
            .map(|c| B64.decode(c["data"].as_str().unwrap()).unwrap())
            .collect();
        assert_eq!(writes.concat(), b"abcxyz");
        let methods = d.methods();
        let last_write = methods.iter().rposition(|m| m == "ExecWrite").unwrap();
        let close = methods.iter().position(|m| m == "ExecCloseStdin").unwrap();
        assert!(close > last_write, "close after the input: {methods:?}");
        assert_eq!(d.calls_of("ExecCloseStdin").len(), 1);
        p.delete().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_exec_with_no_pipes_still_drains_and_exits() {
        let d = FakeDaemon::start().await;
        d.reads.lock().unwrap().extend([
            chunk("discarded", "", 9, 0, false),
            chunk("", "", 9, 0, true),
        ]);
        let p = ExecProc::new(
            cfg("e3", Path::new(""), Path::new(""), Path::new(""), false),
            d.client(),
            "vm".into(),
            "c".into(),
        );
        p.create().await.unwrap();
        p.start().await.unwrap();
        d.exit_gate.notify_one();
        let (status, _) = tokio::time::timeout(Duration::from_secs(10), p.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status, 3);
        assert_eq!(
            d.calls_of("ExecRead").len(),
            2,
            "the output was read (and dropped), not left in the guest"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn delete_stops_the_pumps() {
        let d = FakeDaemon::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let (si, so) = (fifo(tmp.path(), "in"), fifo(tmp.path(), "out"));
        d.endless.store(true, Ordering::SeqCst);
        let p = ExecProc::new(
            cfg("e4", &si, &so, Path::new(""), false),
            d.client(),
            "vm".into(),
            "c".into(),
        );
        p.create().await.unwrap();
        p.start().await.unwrap();
        d.wait_for("ExecRead", 3).await;
        p.delete().await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), p.out_done.wait())
            .await
            .expect("output pump ended");
        tokio::time::timeout(Duration::from_secs(3), p.in_done.wait())
            .await
            .expect("input pump ended");
        let n = d.calls_of("ExecRead").len();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            d.calls_of("ExecRead").len() <= n + 1,
            "no more polling after delete"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_daemon_that_is_gone_is_an_error_not_a_hang() {
        let client = DaemonClient::new("/nonexistent/daemon.sock");
        let p = ExecProc::new(
            cfg("e5", Path::new(""), Path::new(""), Path::new(""), false),
            client,
            "vm".into(),
            "c".into(),
        );
        assert!(p.create().await.is_err());
        assert!(p.start().await.is_err());
        assert!(p.wait().await.is_err());
        // deleting is cleanup: it succeeds
        p.delete().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_stdin_of_a_container_with_a_terminal_goes_to_the_init_terminal_and_nothing_else_is_asked(
    ) {
        let d = FakeDaemon::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let si = fifo(tmp.path(), "in");
        let p = ExecProc::for_init("ctr9", si.clone(), d.client(), "vm9".into());
        p.start_input_only();

        let mut w = open_rw(&si);
        w.write_all(b"ls\n").unwrap();
        d.wait_for("ExecWrite", 1).await;
        w.write_all(b"exit\n").unwrap();
        p.close_stdin().await.unwrap();

        let writes = d.calls_of("ExecWrite");
        let data: Vec<u8> = writes
            .iter()
            .flat_map(|c| B64.decode(c["data"].as_str().unwrap()).unwrap())
            .collect();
        assert_eq!(data, b"ls\nexit\n");
        for c in &writes {
            assert_eq!(c["exec_id"], "init", "the container's own terminal");
            assert_eq!(
                (c["vm_id"].as_str(), c["container_id"].as_str()),
                (Some("vm9"), Some("ctr9"))
            );
        }
        assert_eq!(d.calls_of("ExecCloseStdin").len(), 1);
        assert_eq!(d.calls_of("ExecCloseStdin")[0]["exec_id"], "init");
        // Only the input: no create, start, read or wait for a process that is not an exec.
        let methods = d.methods();
        for m in [
            "ExecCreate",
            "ExecStart",
            "ExecRead",
            "ExecWait",
            "ExecDelete",
            "ExecKill",
        ] {
            assert!(
                !methods.iter().any(|x| x == m),
                "{m} must not be called: {methods:?}"
            );
        }
        p.stop();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_init_input_with_no_stdin_pipe_is_a_no_op() {
        let d = FakeDaemon::start().await;
        let p = ExecProc::for_init("c", std::path::PathBuf::new(), d.client(), "vm".into());
        p.start_input_only();
        tokio::time::timeout(Duration::from_secs(2), p.in_done.wait())
            .await
            .expect("nothing to wait for");
        p.stop();
        assert!(d.methods().is_empty());
    }
}
