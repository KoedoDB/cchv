use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::{OnceCell, RwLock};

use crate::sandbox::shim::exec::{ExecData, ExecState, check_exec_id};
use crate::sandbox::shim::task_state::TaskState;
use crate::sandbox::{Error, ExecConfig, Instance, InstanceConfig, Result};

/// The most exec processes a task may have at once (created or running): a limit on what one task can ask of the instance.
const MAX_EXECS: usize = 64;

pub(super) struct InstanceData<T: Instance> {
    pub instance: T,
    pub config: InstanceConfig,
    pid: OnceCell<u32>,
    state: RwLock<TaskState>,
    execs: RwLock<HashMap<String, Arc<ExecData>>>,
}

impl<T: Instance> InstanceData<T> {
    pub async fn new(
        id: impl AsRef<str> + std::fmt::Debug,
        config: InstanceConfig,
    ) -> Result<Self> {
        let id = id.as_ref().to_string();
        let instance = T::new(id, &config).await?;
        Ok(Self {
            instance,
            config,
            pid: OnceCell::default(),
            state: RwLock::new(TaskState::Created),
            execs: RwLock::default(),
        })
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid.get().copied()
    }

    pub async fn start(&self) -> Result<u32> {
        let mut s = self.state.write().await;
        s.start()?;

        let res = self.instance.start().await;

        // These state transitions are always `Ok(())` because
        // we hold the lock since `s.start()`
        let _ = match res {
            Ok(pid) => {
                let _ = self.pid.set(pid);
                s.started()
            }
            Err(_) => s.stop(),
        };

        res
    }

    pub async fn kill(&self, signal: u32) -> Result<()> {
        let mut s = self.state.write().await;
        s.kill()?;

        self.instance.kill(signal).await
    }

    pub async fn delete(&self) -> Result<()> {
        let mut s = self.state.write().await;
        s.delete()?;

        let res = self.instance.delete().await;

        if res.is_err() {
            // Always `Ok(())` because we hold the lock since `s.delete()`
            let _ = s.stop();
        }

        res
    }

    pub async fn wait(&self) -> (u32, DateTime<Utc>) {
        let res = self.instance.wait().await;
        let mut s = self.state.write().await;
        *s = TaskState::Exited;
        res
    }

    /// `ResizePty` of the task's own terminal: it must have one and be running.
    pub async fn resize_pty(&self, width: u32, height: u32) -> Result<()> {
        if !self.config.terminal {
            return Err(Error::FailedPrecondition("the task has no terminal".into()));
        }
        if !matches!(*self.state.read().await, TaskState::Started) {
            return Err(Error::FailedPrecondition("the task is not running".into()));
        }
        self.instance.resize_pty(width, height).await
    }

    /// `CloseIO` (stdin) of the task's own process: nothing to close once it has exited.
    pub async fn close_stdin(&self) -> Result<()> {
        if !matches!(*self.state.read().await, TaskState::Started) {
            return Ok(());
        }
        self.instance.close_stdin().await
    }

    // ── exec processes ──────────────────────────────────────────────────────

    /// The exec process with this id.
    pub async fn exec(&self, exec_id: &str) -> Result<Arc<ExecData>> {
        let e = self.execs.read().await.get(exec_id).cloned();
        e.ok_or_else(|| Error::NotFound(format!("exec process {exec_id}")))
    }

    /// `Exec`: ask the instance to create the process, and keep it by its id. The task must be running.
    pub async fn exec_create(&self, config: ExecConfig) -> Result<()> {
        check_exec_id(&config.exec_id)?;
        if self.pid().is_none() || matches!(*self.state.read().await, TaskState::Exited | TaskState::Deleting) {
            return Err(Error::FailedPrecondition(
                "the task is not running, so it cannot have an exec process".into(),
            ));
        }
        // The lock is held through the instance's call, so two `Exec` with one id cannot both pass.
        let mut execs = self.execs.write().await;
        if execs.contains_key(&config.exec_id) {
            return Err(Error::AlreadyExists(format!("exec process {}", config.exec_id)));
        }
        if execs.len() >= MAX_EXECS {
            return Err(Error::FailedPrecondition(format!(
                "the task has {MAX_EXECS} exec processes already"
            )));
        }
        self.instance.exec_create(&config).await?;
        execs.insert(config.exec_id.clone(), Arc::new(ExecData::new(config)));
        Ok(())
    }

    /// `Start` of an exec process: returns its pid. If the instance cannot start it, the exec is exited (status 255) and can be deleted.
    pub async fn exec_start(&self, exec: &ExecData) -> Result<u32> {
        let mut s = exec.state.write().await;
        s.start()?;
        match self.instance.exec_start(&exec.config.exec_id).await {
            Ok(pid) => {
                exec.set_pid(pid);
                s.started()?;
                Ok(pid)
            }
            Err(err) => {
                s.exited();
                drop(s);
                let _ = exec.exit.set((255, Utc::now()));
                Err(err)
            }
        }
    }

    /// Wait for the instance to report that the exec process exited, and record it. Run once, from `Start`.
    pub async fn exec_watch(&self, exec: &ExecData) -> (u32, DateTime<Utc>) {
        let (status, at) = match self.instance.exec_wait(&exec.config.exec_id).await {
            Ok(v) => v,
            // The instance cannot tell how it ended (its connection to the process is gone): that is a failure, not a success.
            Err(err) => {
                log::warn!("waiting for exec process {}: {err}", exec.config.exec_id);
                (255, Utc::now())
            }
        };
        exec.finish(status, at).await;
        exec.exited_at().unwrap_or((status, at))
    }

    pub async fn exec_kill(&self, exec: &ExecData, signal: u32) -> Result<()> {
        let s = exec.state.read().await;
        s.kill()?;
        if matches!(*s, ExecState::Exited) {
            return Ok(());
        }
        self.instance.exec_kill(&exec.config.exec_id, signal).await
    }

    /// `Delete` of an exec process.
    pub async fn exec_delete(&self, exec: &ExecData) -> Result<()> {
        {
            let mut s = exec.state.write().await;
            s.delete()?;
        }
        if let Err(err) = self.instance.exec_delete(&exec.config.exec_id).await {
            exec.state.write().await.delete_failed();
            return Err(err);
        }
        self.execs.write().await.remove(&exec.config.exec_id);
        Ok(())
    }

    pub async fn exec_resize(&self, exec: &ExecData, width: u32, height: u32) -> Result<()> {
        if !exec.config.terminal {
            return Err(Error::FailedPrecondition(
                "the exec process has no terminal".into(),
            ));
        }
        if !exec.state.read().await.is_running() {
            return Err(Error::FailedPrecondition("the exec process is not running".into()));
        }
        self.instance
            .exec_resize(&exec.config.exec_id, width, height)
            .await
    }

    /// `CloseIO` with stdin: a process that has exited has nothing to close.
    pub async fn exec_close_stdin(&self, exec: &ExecData) -> Result<()> {
        if !exec.state.read().await.is_running() {
            return Ok(());
        }
        self.instance.exec_close_stdin(&exec.config.exec_id).await
    }
}
