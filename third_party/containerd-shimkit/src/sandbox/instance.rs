//! Abstractions for running/managing a wasm/wasi instance.

use std::fs::File;
use std::future::Future;
use std::io::{Error as IoError, ErrorKind, Result as IoResult};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::error::Error;
use crate::sys::stdio::open;

/// Generic options builder for creating a wasm instance.
/// This is passed to the `Instance::new` method.
#[derive(Clone, Debug, Default)]
pub struct InstanceConfig {
    /// Optional stdin named pipe path.
    pub stdin: PathBuf,
    /// Optional stdout named pipe path.
    pub stdout: PathBuf,
    /// Optional stderr named pipe path.
    pub stderr: PathBuf,
    /// Path to the OCI bundle directory.
    pub bundle: PathBuf,
    /// Namespace for containerd
    pub namespace: String,
    /// GRPC address back to main containerd
    pub containerd_address: String,
    /// Whether the task's process has a terminal (`tty: true`). With one containerd gives a stdout pipe (the terminal's output) and,
    /// if the client has input, a stdin pipe; there is no stderr.
    pub terminal: bool,
}

/// What containerd asks for with `Exec`: an additional process in a running task.
#[derive(Clone, Debug, Default)]
pub struct ExecConfig {
    /// The id containerd gave the exec process (unique within the task).
    pub exec_id: String,
    /// The OCI `Process` of the exec, as JSON (the body of the `Any` in the request).
    pub spec: Vec<u8>,
    /// Whether the process has a terminal.
    pub terminal: bool,
    /// The named pipes containerd made for the process (empty when it has none).
    pub stdin: PathBuf,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
}

/// Represents a WASI module(s).
/// Instance is a trait that gets implemented by consumers of this library.
/// This trait requires that any type implementing it is `'static`, similar to `std::any::Any`.
/// This means that the type cannot contain a non-`'static` reference.
#[cfg_attr(not(doc), trait_variant::make(Send))]
pub trait Instance: 'static {
    /// Create a new instance
    async fn new(id: String, cfg: &InstanceConfig) -> Result<Self, Error>
    where
        Self: Sized;

    /// Start the instance
    /// The returned value should be a unique ID (such as a PID) for the instance.
    /// Nothing internally should be using this ID, but it is returned to containerd where a user may want to use it.
    async fn start(&self) -> Result<u32, Error>;

    /// Send a signal to the instance
    async fn kill(&self, signal: u32) -> Result<(), Error>;

    /// Delete any reference to the instance
    /// This is called after the instance has exited.
    async fn delete(&self) -> Result<(), Error>;

    /// Waits for the instance to finish and returns its exit code
    /// This is an async call.
    async fn wait(&self) -> (u32, DateTime<Utc>);

    // The terminal of the task's own process. Optional: an instance that has no terminal leaves them as they are.

    /// Change the size of the task's terminal (`ResizePty` without an exec id).
    fn resize_pty(&self, _width: u32, _height: u32) -> impl Future<Output = Result<(), Error>> + Send {
        std::future::ready(Err(Error::FailedPrecondition("the task has no terminal".to_string())))
    }

    /// Close the standard input of the task's process (`CloseIO` without an exec id).
    fn close_stdin(&self) -> impl Future<Output = Result<(), Error>> + Send {
        std::future::ready(Ok(()))
    }

    // Exec. Optional: an instance that does not run additional processes leaves these as they are, and `Exec` is answered with
    // "unimplemented". The task service keeps the state of each exec (created, started, exited, deleted) and publishes the events; an
    // instance only runs the process and reports what happened. Every call names the exec by its id.

    // (Written as functions that return a future, because `trait_variant` does not wrap the body of a provided `async fn`. An instance
    // implements them with `async fn` as usual.)

    /// Create the exec process (it is not running yet).
    fn exec_create(&self, _cfg: &ExecConfig) -> impl Future<Output = Result<(), Error>> + Send {
        std::future::ready(Err(exec_unimplemented()))
    }

    /// Start a created exec process and return its pid.
    fn exec_start(&self, _exec_id: &str) -> impl Future<Output = Result<u32, Error>> + Send {
        std::future::ready(Err(exec_unimplemented()))
    }

    /// Send a signal to a running exec process.
    fn exec_kill(&self, _exec_id: &str, _signal: u32) -> impl Future<Output = Result<(), Error>> + Send {
        std::future::ready(Err(exec_unimplemented()))
    }

    /// Wait for the exec process to exit and return its exit status.
    fn exec_wait(&self, _exec_id: &str) -> impl Future<Output = Result<(u32, DateTime<Utc>), Error>> + Send {
        std::future::ready(Err(exec_unimplemented()))
    }

    /// Delete any reference to the exec process (it has exited, or was never started).
    fn exec_delete(&self, _exec_id: &str) -> impl Future<Output = Result<(), Error>> + Send {
        std::future::ready(Err(exec_unimplemented()))
    }

    /// Change the size of the terminal of an exec process.
    fn exec_resize(
        &self,
        _exec_id: &str,
        _width: u32,
        _height: u32,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        std::future::ready(Err(exec_unimplemented()))
    }

    /// Close the standard input of an exec process.
    fn exec_close_stdin(&self, _exec_id: &str) -> impl Future<Output = Result<(), Error>> + Send {
        std::future::ready(Err(exec_unimplemented()))
    }
}

fn exec_unimplemented() -> Error {
    Error::Shim(containerd_shim::Error::Unimplemented(
        "exec is not supported".to_string(),
    ))
}

/// Open one of the named pipes containerd made (read-write, so that opening does not wait for the other end).
fn open_pipe(path: &Path) -> IoResult<File> {
    if path.as_os_str().is_empty() {
        return Err(IoError::new(ErrorKind::NotFound, "File not found"));
    }
    open(path)
}

impl InstanceConfig {
    pub fn open_stdin(&self) -> IoResult<File> {
        open_pipe(&self.stdin)
    }

    pub fn open_stdout(&self) -> IoResult<File> {
        open_pipe(&self.stdout)
    }

    pub fn open_stderr(&self) -> IoResult<File> {
        open_pipe(&self.stderr)
    }
}

impl ExecConfig {
    pub fn open_stdin(&self) -> IoResult<File> {
        open_pipe(&self.stdin)
    }

    pub fn open_stdout(&self) -> IoResult<File> {
        open_pipe(&self.stdout)
    }

    pub fn open_stderr(&self) -> IoResult<File> {
        open_pipe(&self.stderr)
    }
}
