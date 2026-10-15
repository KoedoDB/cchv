use std::collections::HashMap;
use std::fs::create_dir_all;
use std::ops::Not;
use std::path::Path;
use std::sync::Arc;

use containerd_shim::api::{
    CloseIORequest, ConnectRequest, ConnectResponse, CreateTaskRequest, CreateTaskResponse,
    DeleteRequest, Empty, ExecProcessRequest, KillRequest, ResizePtyRequest, ShutdownRequest,
    StartRequest, StartResponse, StateRequest, StateResponse, StatsRequest, StatsResponse,
    WaitRequest, WaitResponse,
};
use containerd_shim::error::Error as ShimError;
use containerd_shim::protos::events::task::{
    TaskCreate, TaskDelete, TaskExecAdded, TaskExecStarted, TaskExit, TaskIO, TaskStart,
};
use containerd_shim::protos::shim::shim_ttrpc::Task;
use containerd_shim::protos::types::task::Status;
use containerd_shim::util::IntoOption;
use containerd_shim::{DeleteResponse, TtrpcContext, TtrpcResult};
use futures::FutureExt as _;
use log::debug;
use oci_spec::runtime::Spec;
use tokio::sync::RwLock;
use crate::sandbox::async_utils::AmbientRuntime as _;
use crate::sandbox::instance::{ExecConfig, Instance, InstanceConfig};
use crate::sandbox::shim::events::{EventSender, RemoteEventSender, ToTimestamp};
use crate::sandbox::shim::exec::ExecState;
use crate::sandbox::shim::instance_data::InstanceData;
use crate::sandbox::sync::WaitableCell;
use crate::sandbox::{Error, Result};
use crate::sys::metrics::get_metrics;

#[cfg(test)]
mod tests;

/// The largest process spec accepted in an `Exec` (a command line and an environment).
const MAX_EXEC_SPEC_BYTES: usize = 1024 * 1024;

type LocalInstances<T> = RwLock<HashMap<String, Arc<InstanceData<T>>>>;

/// Local implements the Task service for a containerd shim.
/// It defers all task operations to the `Instance` implementation.
pub struct Local<T: Instance + Send + Sync, E: EventSender = RemoteEventSender> {
    pub(super) instances: LocalInstances<T>,
    events: E,
    exit: WaitableCell<()>,
    namespace: String,
    containerd_address: String,
}

impl<T: Instance + Send + Sync, E: EventSender> Local<T, E> {
    /// Creates a new local task service.
    pub fn new(
        events: E,
        exit: WaitableCell<()>,
        namespace: impl AsRef<str> + std::fmt::Debug,
        containerd_address: impl AsRef<str> + std::fmt::Debug,
    ) -> Self {
        let instances = RwLock::default();
        let namespace = namespace.as_ref().to_string();
        let containerd_address = containerd_address.as_ref().to_string();
        Self {
            instances,
            events,
            exit,
            namespace,
            containerd_address,
        }
    }

    pub(super) async fn get_instance(&self, id: &str) -> Result<Arc<InstanceData<T>>> {
        let instance = self.instances.read().await.get(id).cloned();
        instance.ok_or_else(|| Error::NotFound(id.to_string()))
    }

    async fn has_instance(&self, id: &str) -> bool {
        self.instances.read().await.contains_key(id)
    }

    async fn is_empty(&self) -> bool {
        self.instances.read().await.is_empty()
    }
}

// These are the same functions as in Task, but without the TtrcpContext, which is useful for testing
impl<T: Instance + Send + Sync, E: EventSender> Local<T, E> {
    async fn task_create(&self, req: CreateTaskRequest) -> Result<CreateTaskResponse> {
        if !req.checkpoint().is_empty() || !req.parent_checkpoint().is_empty() {
            return Err(ShimError::Unimplemented("checkpoint is not supported".to_string()).into());
        }

        if self.has_instance(&req.id).await {
            return Err(Error::AlreadyExists(req.id));
        }

        let mut spec = Spec::load(Path::new(&req.bundle).join("config.json"))
            .map_err(|err| Error::InvalidArgument(format!("could not load runtime spec: {err}")))?;

        spec.canonicalize_rootfs(req.bundle()).map_err(|err| {
            ShimError::InvalidArgument(format!("could not canonicalize rootfs: {err}"))
        })?;

        let rootfs = spec
            .root()
            .as_ref()
            .ok_or_else(|| Error::InvalidArgument("rootfs is not set in runtime spec".to_string()))?
            .path();

        let _ = create_dir_all(rootfs);
        let rootfs_mounts = req.rootfs().to_vec();
        if !rootfs_mounts.is_empty() {
            for m in rootfs_mounts {
                let _mount_type = m.type_().none_if(|&x| x.is_empty());
                let _source = m.source.as_str().none_if(|&x| x.is_empty());

                containerd_shim::mount::mount_rootfs(
                    _mount_type,
                    _source,
                    &m.options.to_vec(),
                    rootfs,
                )?;
            }
        }

        let cfg = InstanceConfig {
            namespace: self.namespace.clone(),
            containerd_address: self.containerd_address.clone(),
            bundle: req.bundle.as_str().into(),
            stdout: req.stdout.as_str().into(),
            stderr: req.stderr.as_str().into(),
            stdin: req.stdin.as_str().into(),
            terminal: req.terminal,
        };

        // Check if this is a cri container
        let instance = InstanceData::new(req.id(), cfg).await?;

        self.instances
            .write()
            .await
            .insert(req.id().to_string(), Arc::new(instance));

        self.events.send(TaskCreate {
            container_id: req.id,
            bundle: req.bundle,
            rootfs: req.rootfs,
            io: Some(TaskIO {
                stdin: req.stdin,
                stdout: req.stdout,
                stderr: req.stderr,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        });

        debug!("create done");

        Ok(CreateTaskResponse {
            pid: std::process::id(),
            ..Default::default()
        })
    }

    async fn task_start(&self, req: StartRequest) -> Result<StartResponse> {
        if req.exec_id().is_empty().not() {
            return self.exec_start(req).await;
        }

        let i = self.get_instance(req.id()).await?;
        let pid = i.start().await?;

        self.events.send(TaskStart {
            container_id: req.id().into(),
            pid,
            ..Default::default()
        });

        let events = self.events.clone();

        let id = req.id().to_string();

        async move {
            let (exit_code, timestamp) = i.wait().await;
            events.send(TaskExit {
                container_id: id.clone(),
                exit_status: exit_code,
                exited_at: Some(timestamp.to_timestamp()).into(),
                pid,
                id,
                ..Default::default()
            });
        }
        .spawn();

        debug!("started: {:?}", req);

        Ok(StartResponse {
            pid,
            ..Default::default()
        })
    }

    async fn task_kill(&self, req: KillRequest) -> Result<Empty> {
        if !req.exec_id().is_empty() {
            let i = self.get_instance(req.id()).await?;
            let e = i.exec(req.exec_id()).await?;
            i.exec_kill(&e, req.signal()).await?;
            return Ok(Empty::new());
        }
        self.get_instance(req.id())
            .await?
            .kill(req.signal())
            .await?;
        Ok(Empty::new())
    }

    async fn task_delete(&self, req: DeleteRequest) -> Result<DeleteResponse> {
        if !req.exec_id().is_empty() {
            // containerd sends no TaskDelete event for an exec process (nor does runc's shim).
            let i = self.get_instance(req.id()).await?;
            let e = i.exec(req.exec_id()).await?;
            i.exec_delete(&e).await?;
            let (status, at) = e.exited_at().unzip();
            return Ok(DeleteResponse {
                pid: e.pid().unwrap_or_default(),
                exit_status: status.unwrap_or_default(),
                exited_at: at.map(ToTimestamp::to_timestamp).into(),
                ..Default::default()
            });
        }

        let i = self.get_instance(req.id()).await?;

        i.delete().await?;

        let pid = i.pid().unwrap_or_default();
        let (exit_code, timestamp) = i.wait().now_or_never().unzip();
        let timestamp = timestamp.map(ToTimestamp::to_timestamp);

        self.instances.write().await.remove(req.id());

        self.events.send(TaskDelete {
            container_id: req.id().into(),
            pid,
            exit_status: exit_code.unwrap_or_default(),
            exited_at: timestamp.clone().into(),
            ..Default::default()
        });

        Ok(DeleteResponse {
            pid,
            exit_status: exit_code.unwrap_or_default(),
            exited_at: timestamp.into(),
            ..Default::default()
        })
    }

    async fn task_wait(&self, req: WaitRequest) -> Result<WaitResponse> {
        if !req.exec_id().is_empty() {
            // containerd waits before it starts the process, so this returns when the process has been started and has exited.
            let i = self.get_instance(req.id()).await?;
            let e = i.exec(req.exec_id()).await?;
            let (status, at) = *e.exit.wait().await;
            return Ok(WaitResponse {
                exit_status: status,
                exited_at: Some(at.to_timestamp()).into(),
                ..Default::default()
            });
        }

        let i = self.get_instance(req.id()).await?;
        let (exit_code, timestamp) = i.wait().await;

        debug!("wait finishes");
        Ok(WaitResponse {
            exit_status: exit_code,
            exited_at: Some(timestamp.to_timestamp()).into(),
            ..Default::default()
        })
    }

    async fn task_state(&self, req: StateRequest) -> Result<StateResponse> {
        if !req.exec_id().is_empty() {
            let i = self.get_instance(req.id()).await?;
            let e = i.exec(req.exec_id()).await?;
            let (status, at) = e.exited_at().unzip();
            let state = *e.state.read().await;
            let status_code = match state {
                ExecState::Created => Status::CREATED,
                ExecState::Starting | ExecState::Started => Status::RUNNING,
                ExecState::Exited | ExecState::Deleting => Status::STOPPED,
            };
            return Ok(StateResponse {
                id: req.id().to_string(),
                exec_id: req.exec_id().to_string(),
                bundle: i.config.bundle.to_string_lossy().to_string(),
                stdin: e.config.stdin.to_string_lossy().to_string(),
                stdout: e.config.stdout.to_string_lossy().to_string(),
                stderr: e.config.stderr.to_string_lossy().to_string(),
                terminal: e.config.terminal,
                pid: e.pid().unwrap_or_default(),
                exit_status: status.unwrap_or_default(),
                exited_at: at.map(ToTimestamp::to_timestamp).into(),
                status: status_code.into(),
                ..Default::default()
            });
        }

        let i = self.get_instance(req.id()).await?;
        let pid = i.pid();
        let (exit_code, timestamp) = i.wait().now_or_never().unzip();
        let timestamp = timestamp.map(ToTimestamp::to_timestamp);

        let status = if pid.is_none() {
            Status::CREATED
        } else if exit_code.is_none() {
            Status::RUNNING
        } else {
            Status::STOPPED
        };

        Ok(StateResponse {
            terminal: i.config.terminal,
            bundle: i.config.bundle.to_string_lossy().to_string(),
            stdin: i.config.stdin.to_string_lossy().to_string(),
            stdout: i.config.stdout.to_string_lossy().to_string(),
            stderr: i.config.stderr.to_string_lossy().to_string(),
            pid: pid.unwrap_or_default(),
            exit_status: exit_code.unwrap_or_default(),
            exited_at: timestamp.into(),
            status: status.into(),
            ..Default::default()
        })
    }

    /// `Exec`: make an additional process in a running task. It is started by a later `Start` with its exec id.
    async fn task_exec(&self, req: ExecProcessRequest) -> Result<Empty> {
        let i = self.get_instance(req.id()).await?;
        let spec = req
            .spec
            .as_ref()
            .ok_or_else(|| Error::InvalidArgument("the exec request has no process spec".into()))?;
        // The body of the `Any` is the OCI `Process` as JSON. It is checked to be that before an instance sees it.
        if spec.value.len() > MAX_EXEC_SPEC_BYTES {
            return Err(Error::InvalidArgument("the exec process spec is too large".into()));
        }
        let process: serde_json::Value = serde_json::from_slice(&spec.value)
            .map_err(|e| Error::InvalidArgument(format!("the exec process spec is not JSON: {e}")))?;
        if !process.is_object() {
            return Err(Error::InvalidArgument("the exec process spec is not an object".into()));
        }
        let config = ExecConfig {
            exec_id: req.exec_id().to_string(),
            spec: spec.value.clone(),
            terminal: req.terminal,
            stdin: req.stdin.as_str().into(),
            stdout: req.stdout.as_str().into(),
            stderr: req.stderr.as_str().into(),
        };
        i.exec_create(config).await?;
        self.events.send(TaskExecAdded {
            container_id: req.id().to_string(),
            exec_id: req.exec_id().to_string(),
            ..Default::default()
        });
        Ok(Empty::new())
    }

    /// `Start` with an exec id.
    async fn exec_start(&self, req: StartRequest) -> Result<StartResponse> {
        let i = self.get_instance(req.id()).await?;
        let e = i.exec(req.exec_id()).await?;
        let pid = i.exec_start(&e).await?;

        self.events.send(TaskExecStarted {
            container_id: req.id().to_string(),
            exec_id: req.exec_id().to_string(),
            pid,
            ..Default::default()
        });

        let events = self.events.clone();
        let (id, exec_id) = (req.id().to_string(), req.exec_id().to_string());
        async move {
            let (status, at) = i.exec_watch(&e).await;
            events.send(TaskExit {
                container_id: id,
                id: exec_id,
                pid,
                exit_status: status,
                exited_at: Some(at.to_timestamp()).into(),
                ..Default::default()
            });
        }
        .spawn();

        Ok(StartResponse {
            pid,
            ..Default::default()
        })
    }

    async fn task_resize_pty(&self, req: ResizePtyRequest) -> Result<Empty> {
        let i = self.get_instance(req.id()).await?;
        if req.exec_id().is_empty() {
            i.resize_pty(req.width, req.height).await?;
            return Ok(Empty::new());
        }
        let e = i.exec(req.exec_id()).await?;
        i.exec_resize(&e, req.width, req.height).await?;
        Ok(Empty::new())
    }

    async fn task_close_io(&self, req: CloseIORequest) -> Result<Empty> {
        let i = self.get_instance(req.id()).await?;
        if req.exec_id().is_empty() {
            if req.stdin {
                i.close_stdin().await?;
            }
            return Ok(Empty::new());
        }
        let e = i.exec(req.exec_id()).await?;
        if req.stdin {
            i.exec_close_stdin(&e).await?;
        }
        Ok(Empty::new())
    }

    async fn task_stats(&self, req: StatsRequest) -> Result<StatsResponse> {
        let i = self.get_instance(req.id()).await?;
        let pid = i
            .pid()
            .ok_or_else(|| Error::InvalidArgument("task is not running".to_string()))?;

        let metrics = get_metrics(pid)?;

        Ok(StatsResponse {
            stats: Some(metrics).into(),
            ..Default::default()
        })
    }
}

impl<T: Instance + Sync + Send, E: EventSender> Task for Local<T, E> {
    fn create(
        &self,
        _ctx: &TtrpcContext,
        req: CreateTaskRequest,
    ) -> TtrpcResult<CreateTaskResponse> {
        debug!("create: {:?}", req);


        Ok(self.task_create(req).block_on()?)
    }

    fn start(&self, _ctx: &TtrpcContext, req: StartRequest) -> TtrpcResult<StartResponse> {
        debug!("start: {:?}", req);


        Ok(self.task_start(req).block_on()?)
    }

    fn kill(&self, _ctx: &TtrpcContext, req: KillRequest) -> TtrpcResult<Empty> {
        debug!("kill: {:?}", req);


        Ok(self.task_kill(req).block_on()?)
    }

    fn delete(&self, _ctx: &TtrpcContext, req: DeleteRequest) -> TtrpcResult<DeleteResponse> {
        debug!("delete: {:?}", req);


        Ok(self.task_delete(req).block_on()?)
    }

    fn wait(&self, _ctx: &TtrpcContext, req: WaitRequest) -> TtrpcResult<WaitResponse> {
        debug!("wait: {:?}", req);

        let res = self.task_wait(req).block_on()?;

        Ok(res)
    }

    fn exec(&self, _ctx: &TtrpcContext, req: ExecProcessRequest) -> TtrpcResult<Empty> {
        debug!("exec: {:?}", req);


        Ok(self.task_exec(req).block_on()?)
    }

    fn resize_pty(&self, _ctx: &TtrpcContext, req: ResizePtyRequest) -> TtrpcResult<Empty> {
        debug!("resize_pty: {:?}", req);


        Ok(self.task_resize_pty(req).block_on()?)
    }

    fn close_io(&self, _ctx: &TtrpcContext, req: CloseIORequest) -> TtrpcResult<Empty> {
        debug!("close_io: {:?}", req);


        Ok(self.task_close_io(req).block_on()?)
    }

    fn connect(&self, _ctx: &TtrpcContext, req: ConnectRequest) -> TtrpcResult<ConnectResponse> {
        debug!("connect: {:?}", req);


        let i = self.get_instance(req.id()).block_on()?;
        let shim_pid = std::process::id();
        let task_pid = i.pid().unwrap_or_default();
        Ok(ConnectResponse {
            shim_pid,
            task_pid,
            ..Default::default()
        })
    }

    fn state(&self, _ctx: &TtrpcContext, req: StateRequest) -> TtrpcResult<StateResponse> {
        debug!("state: {:?}", req);


        Ok(self.task_state(req).block_on()?)
    }

    fn shutdown(&self, _ctx: &TtrpcContext, _: ShutdownRequest) -> TtrpcResult<Empty> {
        debug!("shutdown");


        if self.is_empty().block_on() {
            let _ = self.exit.set(());
        }
        Ok(Empty::new())
    }

    fn stats(&self, _ctx: &TtrpcContext, req: StatsRequest) -> TtrpcResult<StatsResponse> {
        debug!("stats: {:?}", req);


        Ok(self.task_stats(req).block_on()?)
    }
}
