//! The exec processes of a task.
//!
//! containerd's `Exec` makes an additional process in a running task (`kubectl exec`, an exec probe); `Start`, `Kill`, `Wait`,
//! `Delete`, `State`, `ResizePty` and `CloseIO` then name it by its exec id. The task service keeps one `ExecData` for each, with a
//! small state machine, so that the order of calls is checked here and an `Instance` only has to run the process.

use chrono::{DateTime, Utc};
use tokio::sync::{OnceCell, RwLock};

use crate::sandbox::Error::FailedPrecondition;
use crate::sandbox::instance::ExecConfig;
use crate::sandbox::sync::WaitableCell;
use crate::sandbox::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExecState {
    Created,
    Starting,
    Started,
    Exited,
    Deleting,
}

impl ExecState {
    /// `Start` of an exec that was created.
    pub fn start(&mut self) -> Result<()> {
        match self {
            Self::Created => {
                *self = Self::Starting;
                Ok(())
            }
            _ => transition_error(*self, Self::Starting),
        }
    }

    /// The instance started the process.
    pub fn started(&mut self) -> Result<()> {
        match self {
            Self::Starting => {
                *self = Self::Started;
                Ok(())
            }
            _ => transition_error(*self, Self::Started),
        }
    }

    /// The process ended: it ran and exited, or it could not be started.
    pub fn exited(&mut self) {
        if matches!(self, Self::Starting | Self::Started | Self::Created) {
            *self = Self::Exited;
        }
    }

    /// A signal can only go to a running process. One to a process that has exited is accepted and does nothing: containerd's CRI
    /// plugin kills an exec after its timeout without knowing whether it has just ended.
    pub fn kill(&self) -> Result<()> {
        match self {
            Self::Started | Self::Exited => Ok(()),
            _ => transition_error(*self, "Killing"),
        }
    }

    /// An exec is deleted when it has exited, or was never started. Deleting again is accepted (a retry).
    pub fn delete(&mut self) -> Result<()> {
        match self {
            Self::Created | Self::Exited | Self::Deleting => {
                *self = Self::Deleting;
                Ok(())
            }
            _ => transition_error(*self, Self::Deleting),
        }
    }

    /// A delete that failed in the instance can be tried again.
    pub fn delete_failed(&mut self) {
        if matches!(self, Self::Deleting) {
            *self = Self::Exited;
        }
    }

    /// Whether the process is running (what `ResizePty` and `CloseIO` need).
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Started)
    }
}

fn transition_error<T>(from: impl std::fmt::Debug, to: impl std::fmt::Debug) -> Result<T> {
    Err(FailedPrecondition(format!(
        "invalid state transition of the exec process: {from:?} => {to:?}"
    )))
}

/// One exec process of a task.
pub(super) struct ExecData {
    pub config: ExecConfig,
    pid: OnceCell<u32>,
    pub(super) state: RwLock<ExecState>,
    /// Set once, when the process has exited: its exit status and the time.
    pub exit: WaitableCell<(u32, DateTime<Utc>)>,
}

impl ExecData {
    pub fn new(config: ExecConfig) -> Self {
        Self {
            config,
            pid: OnceCell::default(),
            state: RwLock::new(ExecState::Created),
            exit: WaitableCell::new(),
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid.get().copied()
    }

    pub fn set_pid(&self, pid: u32) {
        let _ = self.pid.set(pid);
    }

    /// The exit status if the process has exited.
    pub fn exited_at(&self) -> Option<(u32, DateTime<Utc>)> {
        use futures::FutureExt as _;
        self.exit.wait().now_or_never().copied()
    }

    /// Record that the process ended (once; the first value stays).
    pub async fn finish(&self, status: u32, at: DateTime<Utc>) {
        let _ = self.exit.set((status, at));
        self.state.write().await.exited();
    }
}

/// Check an exec id: it names a process in this task only, and ends up in logs and in calls to other components.
pub(super) fn check_exec_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidArgument(format!(
            "the exec id {id:?} is empty, longer than 128 bytes, or has characters other than letters, digits and _-."
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_normal_life_of_an_exec() {
        let mut s = ExecState::Created;
        s.start().unwrap();
        assert_eq!(s, ExecState::Starting);
        s.started().unwrap();
        assert_eq!(s, ExecState::Started);
        assert!(s.is_running());
        s.kill().unwrap();
        s.exited();
        assert_eq!(s, ExecState::Exited);
        s.kill().unwrap(); // after the exit: accepted, does nothing
        s.delete().unwrap();
        assert_eq!(s, ExecState::Deleting);
        s.delete().unwrap(); // a retry
    }

    #[test]
    fn calls_in_the_wrong_order_are_refused() {
        let mut s = ExecState::Created;
        assert!(s.kill().is_err(), "kill before start");
        assert!(s.started().is_err(), "started before start");
        s.start().unwrap();
        assert!(s.start().is_err(), "start twice");
        assert!(s.delete().is_err(), "delete while starting");
        s.started().unwrap();
        assert!(s.delete().is_err(), "delete while running");
        assert!(s.start().is_err());
    }

    #[test]
    fn an_exec_that_was_never_started_can_be_deleted() {
        let mut s = ExecState::Created;
        s.delete().unwrap();
    }

    #[test]
    fn a_start_that_failed_leaves_an_exec_that_can_be_deleted() {
        let mut s = ExecState::Created;
        s.start().unwrap();
        s.exited(); // the instance could not start it
        assert_eq!(s, ExecState::Exited);
        s.delete().unwrap();
    }

    #[test]
    fn a_failed_delete_can_be_retried() {
        let mut s = ExecState::Exited;
        s.delete().unwrap();
        s.delete_failed();
        assert_eq!(s, ExecState::Exited);
        s.delete().unwrap();
    }

    #[test]
    fn exec_ids() {
        assert!(check_exec_id("3e5e497c128db644d3b824ca51f7200a35d56ebbbcca7425392d608436fd3065").is_ok());
        assert!(check_exec_id("exec-1_a.b").is_ok());
        for bad in ["", "a/b", "..x/", "a b", "a\n", &"x".repeat(129)] {
            assert!(check_exec_id(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn finish_records_the_first_status_only() {
        let e = ExecData::new(ExecConfig::default());
        assert!(e.exited_at().is_none());
        let t = Utc::now();
        e.finish(7, t).await;
        e.finish(9, Utc::now()).await;
        assert_eq!(e.exited_at().unwrap().0, 7);
        assert_eq!(*e.state.read().await, ExecState::Exited);
    }
}
