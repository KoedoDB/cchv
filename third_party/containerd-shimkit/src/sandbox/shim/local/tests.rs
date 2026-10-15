use std::fs::{File, create_dir};
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use containerd_shim::api::Status;
use containerd_shim::event::Event;
use protobuf::MessageDyn;
use serde_json as json;
use tempfile::tempdir;
use tokio::sync::mpsc::{UnboundedSender as Sender, unbounded_channel as channel};
use tokio_async_drop::tokio_async_drop;

use super::*;
use crate::sandbox::shim::events::EventSender;
use crate::sandbox::sync::WaitableCell;

/// This is used for the tests and is a no-op instance implementation.
pub struct InstanceStub {
    /// Since we are faking the container, we need to keep track of the "exit" code/time
    /// We'll just mark it as exited when kill is called.
    exit_code: WaitableCell<(u32, DateTime<Utc>)>,
}

impl Instance for InstanceStub {
    async fn new(_id: String, _cfg: &InstanceConfig) -> Result<Self, Error> {
        Ok(InstanceStub {
            exit_code: WaitableCell::new(),
        })
    }
    async fn start(&self) -> Result<u32, Error> {
        Ok(std::process::id())
    }
    async fn kill(&self, _signal: u32) -> Result<(), Error> {
        let _ = self.exit_code.set((1, Utc::now()));
        Ok(())
    }
    async fn delete(&self) -> Result<(), Error> {
        Ok(())
    }
    async fn wait(&self) -> (u32, DateTime<Utc>) {
        *self.exit_code.wait().await
    }
}

struct LocalWithDestructor<T: Instance + Send + Sync, E: EventSender> {
    local: Arc<Local<T, E>>,
}

impl<T: Instance + Send + Sync, E: EventSender> LocalWithDestructor<T, E> {
    fn new(local: Arc<Local<T, E>>) -> Self {
        Self { local }
    }
}

impl EventSender for Sender<(String, Box<dyn MessageDyn>)> {
    fn send(&self, event: impl Event) {
        let _ = self.send((event.topic(), Box::new(event)));
    }
}

impl<T: Instance + Send + Sync, E: EventSender> Drop for LocalWithDestructor<T, E> {
    fn drop(&mut self) {
        tokio_async_drop!({
            let instances = self.local.instances.write().await;
            for (_, instance) in instances.iter() {
                let _ = instance.kill(9).await;
                let _ = instance.delete().await;
            }
        })
    }
}

fn with_cri_sandbox(spec: Option<Spec>, id: String) -> Spec {
    let mut s = spec.unwrap_or_default();
    let mut annotations = HashMap::new();
    s.annotations().as_ref().map(|a| {
        a.iter().map(|(k, v)| {
            annotations.insert(k.to_string(), v.to_string());
        })
    });
    annotations.insert("io.kubernetes.cri.sandbox-id".to_string(), id);

    s.set_annotations(Some(annotations));
    s
}

fn create_bundle(dir: &std::path::Path, spec: Option<Spec>) -> Result<()> {
    create_dir(dir.join("rootfs"))?;

    let s = spec.unwrap_or_default();

    json::to_writer(File::create(dir.join("config.json"))?, &s)
        .context("could not write config.json")?;
    Ok(())
}

// Use a multi threaded runtime because LocalWithDestructor needs
// it to run its async drop.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_delete_after_create() -> anyhow::Result<()> {
    let dir = tempdir().unwrap();
    let id = "test-delete-after-create";
    create_bundle(dir.path(), None).unwrap();

    let (tx, _rx) = channel();
    let local = Arc::new(Local::<InstanceStub, _>::new(
        tx,
        WaitableCell::new(),
        "test_namespace",
        "/test/address",
    ));
    let mut _wrapped = LocalWithDestructor::new(local.clone());

    local
        .task_create(CreateTaskRequest {
            id: id.to_string(),
            bundle: dir.path().to_str().unwrap().to_string(),
            ..Default::default()
        })
        .await?;

    local
        .task_delete(DeleteRequest {
            id: id.to_string(),
            ..Default::default()
        })
        .await?;

    Ok(())
}

// Use a multi threaded runtime because LocalWithDestructor needs
// it to run its async drop.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_cri_task() -> Result<()> {
    // Currently the relationship between the "base" container and the "instances" are pretty weak.
    // When a cri sandbox is specified we just assume it's the sandbox container and treat it as such by not actually running the code (which is going to be wasm).
    let (etx, _erx) = channel();
    let exit_signal = WaitableCell::new();
    let local = Arc::new(Local::<InstanceStub, _>::new(
        etx,
        exit_signal,
        "test_namespace",
        "/test/address",
    ));

    let mut _wrapped = LocalWithDestructor::new(local.clone());

    let temp = tempdir().unwrap();
    let dir = temp.path();
    let sandbox_id = "test-cri-task".to_string();
    create_bundle(dir, Some(with_cri_sandbox(None, sandbox_id.clone())))?;

    local
        .task_create(CreateTaskRequest {
            id: "testbase".to_string(),
            bundle: dir.to_str().unwrap().to_string(),
            ..Default::default()
        })
        .await?;

    let state = local
        .task_state(StateRequest {
            id: "testbase".to_string(),
            ..Default::default()
        })
        .await?;
    assert_eq!(state.status(), Status::CREATED);

    // make sure that the instance exists
    let _i = local.get_instance("testbase").await?;

    local
        .task_start(StartRequest {
            id: "testbase".to_string(),
            ..Default::default()
        })
        .await?;

    let state = local
        .task_state(StateRequest {
            id: "testbase".to_string(),
            ..Default::default()
        })
        .await?;
    assert_eq!(state.status(), Status::RUNNING);

    let ll = local.clone();
    let (base_tx, mut base_rx) = channel();
    tokio::spawn(async move {
        let resp = ll
            .task_wait(WaitRequest {
                id: "testbase".to_string(),
                ..Default::default()
            })
            .await;
        base_tx.send(resp).unwrap();
    });
    base_rx.try_recv().unwrap_err();

    let temp2 = tempdir().unwrap();
    let dir2 = temp2.path();
    create_bundle(dir2, Some(with_cri_sandbox(None, sandbox_id)))?;

    local
        .task_create(CreateTaskRequest {
            id: "testinstance".to_string(),
            bundle: dir2.to_str().unwrap().to_string(),
            ..Default::default()
        })
        .await?;

    let state = local
        .task_state(StateRequest {
            id: "testinstance".to_string(),
            ..Default::default()
        })
        .await?;
    assert_eq!(state.status(), Status::CREATED);

    // make sure that the instance exists
    let _i = local.get_instance("testinstance").await?;

    local
        .task_start(StartRequest {
            id: "testinstance".to_string(),
            ..Default::default()
        })
        .await?;

    let state = local
        .task_state(StateRequest {
            id: "testinstance".to_string(),
            ..Default::default()
        })
        .await?;
    assert_eq!(state.status(), Status::RUNNING);

    let stats = local
        .task_stats(StatsRequest {
            id: "testinstance".to_string(),
            ..Default::default()
        })
        .await?;
    assert!(stats.has_stats());

    let ll = local.clone();
    let (instance_tx, mut instance_rx) = channel();
    tokio::spawn(async move {
        let resp = ll
            .task_wait(WaitRequest {
                id: "testinstance".to_string(),
                ..Default::default()
            })
            .await;
        instance_tx.send(resp).unwrap();
    });
    instance_rx.try_recv().unwrap_err();

    local
        .task_kill(KillRequest {
            id: "testinstance".to_string(),
            signal: 9,
            ..Default::default()
        })
        .await?;

    instance_rx
        .recv()
        .with_timeout(Duration::from_secs(50))
        .await
        .flatten()
        .unwrap()?;

    let state = local
        .task_state(StateRequest {
            id: "testinstance".to_string(),
            ..Default::default()
        })
        .await?;
    assert_eq!(state.status(), Status::STOPPED);
    local
        .task_delete(DeleteRequest {
            id: "testinstance".to_string(),
            ..Default::default()
        })
        .await?;

    match local
        .task_state(StateRequest {
            id: "testinstance".to_string(),
            ..Default::default()
        })
        .await
        .unwrap_err()
    {
        Error::NotFound(_) => {}
        e => return Err(e),
    }

    base_rx.try_recv().unwrap_err();
    let state = local
        .task_state(StateRequest {
            id: "testbase".to_string(),
            ..Default::default()
        })
        .await?;
    assert_eq!(state.status(), Status::RUNNING);

    local
        .task_kill(KillRequest {
            id: "testbase".to_string(),
            signal: 9,
            ..Default::default()
        })
        .await?;

    base_rx
        .recv()
        .with_timeout(Duration::from_secs(5))
        .await
        .flatten()
        .unwrap()?;
    let state = local
        .task_state(StateRequest {
            id: "testbase".to_string(),
            ..Default::default()
        })
        .await?;
    assert_eq!(state.status(), Status::STOPPED);

    local
        .task_delete(DeleteRequest {
            id: "testbase".to_string(),
            ..Default::default()
        })
        .await?;
    match local
        .task_state(StateRequest {
            id: "testbase".to_string(),
            ..Default::default()
        })
        .await
        .unwrap_err()
    {
        Error::NotFound(_) => {}
        e => return Err(e),
    }

    Ok(())
}

// Use a multi threaded runtime because LocalWithDestructor needs
// it to run its async drop.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_task_lifecycle() -> Result<()> {
    let (etx, _erx) = channel(); // TODO: check events
    let exit_signal = WaitableCell::new();
    let local = Arc::new(Local::<InstanceStub, _>::new(
        etx,
        exit_signal,
        "test_namespace",
        "/test/address",
    ));

    let mut _wrapped = LocalWithDestructor::new(local.clone());

    let temp = tempdir().unwrap();
    let dir = temp.path();
    create_bundle(dir, None)?;

    match local
        .task_state(StateRequest {
            id: "test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap_err()
    {
        Error::NotFound(_) => {}
        e => return Err(e),
    }

    local
        .task_create(CreateTaskRequest {
            id: "test".to_string(),
            bundle: dir.to_str().unwrap().to_string(),
            ..Default::default()
        })
        .await?;

    match local
        .task_create(CreateTaskRequest {
            id: "test".to_string(),
            bundle: dir.to_str().unwrap().to_string(),
            ..Default::default()
        })
        .await
        .unwrap_err()
    {
        Error::AlreadyExists(_) => {}
        e => return Err(e),
    }

    let state = local
        .task_state(StateRequest {
            id: "test".to_string(),
            ..Default::default()
        })
        .await?;

    assert_eq!(state.status(), Status::CREATED);

    local
        .task_start(StartRequest {
            id: "test".to_string(),
            ..Default::default()
        })
        .await?;

    let state = local
        .task_state(StateRequest {
            id: "test".to_string(),
            ..Default::default()
        })
        .await?;

    assert_eq!(state.status(), Status::RUNNING);

    let (tx, mut rx) = channel();
    let ll = local.clone();
    tokio::spawn(async move {
        let resp = ll
            .task_wait(WaitRequest {
                id: "test".to_string(),
                ..Default::default()
            })
            .await;
        tx.send(resp).unwrap();
    });

    rx.try_recv().unwrap_err();

    let res = local
        .task_stats(StatsRequest {
            id: "test".to_string(),
            ..Default::default()
        })
        .await?;
    assert!(res.has_stats());

    local
        .task_kill(KillRequest {
            id: "test".to_string(),
            signal: 9,
            ..Default::default()
        })
        .await?;

    rx.recv()
        .with_timeout(Duration::from_secs(5))
        .await
        .flatten()
        .unwrap()?;

    let state = local
        .task_state(StateRequest {
            id: "test".to_string(),
            ..Default::default()
        })
        .await?;
    assert_eq!(state.status(), Status::STOPPED);

    local
        .task_delete(DeleteRequest {
            id: "test".to_string(),
            ..Default::default()
        })
        .await?;

    match local
        .task_state(StateRequest {
            id: "test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap_err()
    {
        Error::NotFound(_) => {}
        e => return Err(e),
    }

    Ok(())
}

// ── exec ────────────────────────────────────────────────────────────────────

use std::collections::HashMap as ExecMap;
use std::sync::Mutex;

use containerd_shim::api::{CloseIORequest, ExecProcessRequest, KillRequest, ResizePtyRequest};
use protobuf::well_known_types::any::Any;

/// An instance that runs exec processes (pretend ones): an exec ends when it is killed.
struct ExecStub {
    base_exit: WaitableCell<(u32, DateTime<Utc>)>,
    execs: Mutex<ExecMap<String, WaitableCell<(u32, DateTime<Utc>)>>>,
    calls: Mutex<Vec<String>>,
}

impl Instance for ExecStub {
    async fn new(_id: String, _cfg: &InstanceConfig) -> Result<Self, Error> {
        Ok(ExecStub {
            base_exit: WaitableCell::new(),
            execs: Mutex::default(),
            calls: Mutex::default(),
        })
    }
    async fn start(&self) -> Result<u32, Error> {
        Ok(100)
    }
    async fn kill(&self, _signal: u32) -> Result<(), Error> {
        let _ = self.base_exit.set((1, Utc::now()));
        Ok(())
    }
    async fn delete(&self) -> Result<(), Error> {
        Ok(())
    }
    async fn wait(&self) -> (u32, DateTime<Utc>) {
        *self.base_exit.wait().await
    }
    async fn resize_pty(&self, width: u32, height: u32) -> Result<(), Error> {
        self.calls.lock().unwrap().push(format!("task resize {width}x{height}"));
        Ok(())
    }
    async fn close_stdin(&self) -> Result<(), Error> {
        self.calls.lock().unwrap().push("task close_stdin".to_string());
        Ok(())
    }
    async fn exec_create(&self, cfg: &ExecConfig) -> Result<(), Error> {
        self.calls.lock().unwrap().push(format!("create {} terminal={}", cfg.exec_id, cfg.terminal));
        self.execs.lock().unwrap().insert(cfg.exec_id.clone(), WaitableCell::new());
        Ok(())
    }
    async fn exec_start(&self, exec_id: &str) -> Result<u32, Error> {
        if exec_id == "unstartable" {
            return Err(Error::Others("cannot start".into()));
        }
        self.calls.lock().unwrap().push(format!("start {exec_id}"));
        Ok(4242)
    }
    async fn exec_kill(&self, exec_id: &str, signal: u32) -> Result<(), Error> {
        self.calls.lock().unwrap().push(format!("kill {exec_id} {signal}"));
        let cell = self.execs.lock().unwrap().get(exec_id).cloned();
        if let Some(c) = cell {
            let _ = c.set((128 + signal, Utc::now()));
        }
        Ok(())
    }
    async fn exec_wait(&self, exec_id: &str) -> Result<(u32, DateTime<Utc>), Error> {
        let cell = self.execs.lock().unwrap().get(exec_id).cloned();
        match cell {
            Some(c) => Ok(*c.wait().await),
            None => Err(Error::NotFound(exec_id.into())),
        }
    }
    async fn exec_delete(&self, exec_id: &str) -> Result<(), Error> {
        self.calls.lock().unwrap().push(format!("delete {exec_id}"));
        self.execs.lock().unwrap().remove(exec_id);
        Ok(())
    }
    async fn exec_resize(&self, exec_id: &str, width: u32, height: u32) -> Result<(), Error> {
        self.calls.lock().unwrap().push(format!("resize {exec_id} {width}x{height}"));
        Ok(())
    }
    async fn exec_close_stdin(&self, exec_id: &str) -> Result<(), Error> {
        self.calls.lock().unwrap().push(format!("close_stdin {exec_id}"));
        Ok(())
    }
}

fn exec_request(id: &str, exec_id: &str, terminal: bool, spec: &[u8]) -> ExecProcessRequest {
    ExecProcessRequest {
        id: id.to_string(),
        exec_id: exec_id.to_string(),
        terminal,
        spec: Some(Any {
            type_url: "types.containerd.io/opencontainers/runtime-spec/1/Process".into(),
            value: spec.to_vec(),
            ..Default::default()
        })
        .into(),
        ..Default::default()
    }
}

async fn running_task<I: Instance + Send + Sync>(
    id: &str,
) -> anyhow::Result<(
    Arc<Local<I, Sender<(String, Box<dyn MessageDyn>)>>>,
    tokio::sync::mpsc::UnboundedReceiver<(String, Box<dyn MessageDyn>)>,
    tempfile::TempDir,
)> {
    let (tx, rx) = channel();
    let local = Arc::new(Local::<I, _>::new(tx, WaitableCell::new(), "ns", "/addr"));
    let dir = tempdir()?;
    create_bundle(dir.path(), None)?;
    local
        .task_create(CreateTaskRequest {
            id: id.to_string(),
            bundle: dir.path().to_str().unwrap().to_string(),
            ..Default::default()
        })
        .await?;
    local
        .task_start(StartRequest { id: id.to_string(), ..Default::default() })
        .await?;
    Ok((local, rx, dir))
}

fn topics(rx: &mut tokio::sync::mpsc::UnboundedReceiver<(String, Box<dyn MessageDyn>)>) -> Vec<String> {
    let mut v = Vec::new();
    while let Ok((t, _)) = rx.try_recv() {
        v.push(t);
    }
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_runs_from_create_to_delete() -> anyhow::Result<()> {
    let (local, mut rx, _dir) = running_task::<ExecStub>("t1").await?;
    let _ = topics(&mut rx);

    local
        .task_exec(exec_request("t1", "e1", false, br#"{"args":["sh","-c","true"]}"#))
        .await?;
    assert_eq!(topics(&mut rx), vec!["/tasks/exec-added".to_string()]);
    let st = local
        .task_state(StateRequest { id: "t1".into(), exec_id: "e1".into(), ..Default::default() })
        .await?;
    assert_eq!(st.status(), Status::CREATED);
    assert_eq!(st.exec_id, "e1");

    // containerd waits before it starts the process
    let l2 = local.clone();
    let waiter = tokio::spawn(async move {
        l2.task_wait(WaitRequest { id: "t1".into(), exec_id: "e1".into(), ..Default::default() })
            .await
    });

    let pid = local
        .task_start(StartRequest { id: "t1".into(), exec_id: "e1".into(), ..Default::default() })
        .await?
        .pid;
    assert_eq!(pid, 4242);
    assert_eq!(topics(&mut rx), vec!["/tasks/exec-started".to_string()]);
    let st = local
        .task_state(StateRequest { id: "t1".into(), exec_id: "e1".into(), ..Default::default() })
        .await?;
    assert_eq!(st.status(), Status::RUNNING);
    assert_eq!(st.pid, 4242);

    // the process is running: it cannot be deleted, and starting it again is refused
    assert!(
        local
            .task_delete(DeleteRequest { id: "t1".into(), exec_id: "e1".into(), ..Default::default() })
            .await
            .is_err()
    );
    assert!(
        local
            .task_start(StartRequest { id: "t1".into(), exec_id: "e1".into(), ..Default::default() })
            .await
            .is_err()
    );

    local
        .task_kill(KillRequest { id: "t1".into(), exec_id: "e1".into(), signal: 9, ..Default::default() })
        .await?;
    let w = tokio::time::timeout(Duration::from_secs(5), waiter).await???;
    assert_eq!(w.exit_status, 137);

    // the exit event names the exec process
    let mut exit_seen = false;
    for _ in 0..50 {
        if topics(&mut rx).iter().any(|t| t == "/tasks/exit") {
            exit_seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(exit_seen, "TaskExit for the exec process");

    // a signal after the exit is accepted
    local
        .task_kill(KillRequest { id: "t1".into(), exec_id: "e1".into(), signal: 15, ..Default::default() })
        .await?;

    let st = local
        .task_state(StateRequest { id: "t1".into(), exec_id: "e1".into(), ..Default::default() })
        .await?;
    assert_eq!(st.status(), Status::STOPPED);
    assert_eq!(st.exit_status, 137);

    let del = local
        .task_delete(DeleteRequest { id: "t1".into(), exec_id: "e1".into(), ..Default::default() })
        .await?;
    assert_eq!((del.pid, del.exit_status), (4242, 137));
    assert!(topics(&mut rx).iter().all(|t| t != "/tasks/delete"), "no TaskDelete for an exec");
    assert!(
        local
            .task_state(StateRequest { id: "t1".into(), exec_id: "e1".into(), ..Default::default() })
            .await
            .is_err(),
        "gone after delete"
    );

    // the task itself still works
    local
        .task_kill(KillRequest { id: "t1".into(), signal: 9, ..Default::default() })
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_is_refused_in_the_wrong_circumstances() -> anyhow::Result<()> {
    // A task that is not running.
    let (tx, _rx) = channel();
    let local = Arc::new(Local::<ExecStub, _>::new(tx, WaitableCell::new(), "ns", "/addr"));
    let dir = tempdir()?;
    create_bundle(dir.path(), None)?;
    local
        .task_create(CreateTaskRequest {
            id: "t2".into(),
            bundle: dir.path().to_str().unwrap().into(),
            ..Default::default()
        })
        .await?;
    assert!(matches!(
        local.task_exec(exec_request("t2", "e1", false, b"{}")).await,
        Err(Error::FailedPrecondition(_))
    ));
    local.task_start(StartRequest { id: "t2".into(), ..Default::default() }).await?;

    // no such task, a bad id, a spec that is not JSON, one that is not an object, one that is too large
    assert!(matches!(
        local.task_exec(exec_request("nope", "e1", false, b"{}")).await,
        Err(Error::NotFound(_))
    ));
    for bad in ["", "a/b", "x y"] {
        assert!(matches!(
            local.task_exec(exec_request("t2", bad, false, b"{}")).await,
            Err(Error::InvalidArgument(_))
        ), "{bad:?}");
    }
    assert!(matches!(
        local.task_exec(exec_request("t2", "e1", false, b"not json")).await,
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        local.task_exec(exec_request("t2", "e1", false, b"[1]")).await,
        Err(Error::InvalidArgument(_))
    ));
    let big = vec![b' '; MAX_EXEC_SPEC_BYTES + 1];
    assert!(matches!(
        local.task_exec(exec_request("t2", "e1", false, &big)).await,
        Err(Error::InvalidArgument(_))
    ));
    let mut no_spec = exec_request("t2", "e1", false, b"{}");
    no_spec.spec = Default::default();
    assert!(matches!(local.task_exec(no_spec).await, Err(Error::InvalidArgument(_))));

    // the same id twice
    local.task_exec(exec_request("t2", "e1", false, b"{}")).await?;
    assert!(matches!(
        local.task_exec(exec_request("t2", "e1", false, b"{}")).await,
        Err(Error::AlreadyExists(_))
    ));

    // a signal to an exec that was not started; resize without a terminal
    assert!(
        local
            .task_kill(KillRequest { id: "t2".into(), exec_id: "e1".into(), signal: 9, ..Default::default() })
            .await
            .is_err()
    );
    assert!(
        local
            .task_resize_pty(ResizePtyRequest { id: "t2".into(), exec_id: "e1".into(), width: 80, height: 24, ..Default::default() })
            .await
            .is_err()
    );
    // the task has no terminal
    assert!(
        local
            .task_resize_pty(ResizePtyRequest { id: "t2".into(), width: 80, height: 24, ..Default::default() })
            .await
            .is_err()
    );
    // an exec that was never started can be deleted
    local
        .task_delete(DeleteRequest { id: "t2".into(), exec_id: "e1".into(), ..Default::default() })
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_that_fails_leaves_an_exec_that_is_stopped_and_can_be_deleted() -> anyhow::Result<()> {
    let (local, _rx, _dir) = running_task::<ExecStub>("t3").await?;
    local.task_exec(exec_request("t3", "unstartable", false, b"{}")).await?;
    let l2 = local.clone();
    let waiter = tokio::spawn(async move {
        l2.task_wait(WaitRequest { id: "t3".into(), exec_id: "unstartable".into(), ..Default::default() })
            .await
    });
    assert!(
        local
            .task_start(StartRequest { id: "t3".into(), exec_id: "unstartable".into(), ..Default::default() })
            .await
            .is_err()
    );
    // whoever waits for it is released, with a failure status
    let w = tokio::time::timeout(Duration::from_secs(5), waiter).await???;
    assert_eq!(w.exit_status, 255);
    let st = local
        .task_state(StateRequest { id: "t3".into(), exec_id: "unstartable".into(), ..Default::default() })
        .await?;
    assert_eq!(st.status(), Status::STOPPED);
    local
        .task_delete(DeleteRequest { id: "t3".into(), exec_id: "unstartable".into(), ..Default::default() })
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_resize_and_close_io_reach_the_instance() -> anyhow::Result<()> {
    let (local, _rx, _dir) = running_task::<ExecStub>("t4").await?;
    local.task_exec(exec_request("t4", "tty", true, b"{}")).await?;
    local
        .task_start(StartRequest { id: "t4".into(), exec_id: "tty".into(), ..Default::default() })
        .await?;
    local
        .task_resize_pty(ResizePtyRequest { id: "t4".into(), exec_id: "tty".into(), width: 120, height: 40, ..Default::default() })
        .await?;
    local
        .task_close_io(CloseIORequest { id: "t4".into(), exec_id: "tty".into(), stdin: true, ..Default::default() })
        .await?;
    let i = local.get_instance("t4").await?;
    let calls = i.instance.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec!["create tty terminal=true", "start tty", "resize tty 120x40", "close_stdin tty"]
    );
    // CloseIO of the task itself is accepted and does nothing
    local
        .task_close_io(CloseIORequest { id: "t4".into(), stdin: true, ..Default::default() })
        .await?;
    local
        .task_kill(KillRequest { id: "t4".into(), exec_id: "tty".into(), signal: 9, ..Default::default() })
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn an_instance_without_exec_answers_unimplemented() -> anyhow::Result<()> {
    let (local, _rx, _dir) = running_task::<InstanceStub>("t5").await?;
    let err = local.task_exec(exec_request("t5", "e1", false, b"{}")).await.unwrap_err();
    assert!(
        matches!(err, Error::Shim(containerd_shim::Error::Unimplemented(_))),
        "{err:?}"
    );
    local.task_kill(KillRequest { id: "t5".into(), signal: 9, ..Default::default() }).await?;
    Ok(())
}

// ── the terminal of the task itself ─────────────────────────────────────────

async fn task_with_terminal(
    id: &str,
    terminal: bool,
) -> anyhow::Result<(
    Arc<Local<ExecStub, Sender<(String, Box<dyn MessageDyn>)>>>,
    tokio::sync::mpsc::UnboundedReceiver<(String, Box<dyn MessageDyn>)>,
    tempfile::TempDir,
)> {
    let (tx, rx) = channel();
    let local = Arc::new(Local::<ExecStub, _>::new(tx, WaitableCell::new(), "ns", "/addr"));
    let dir = tempdir()?;
    create_bundle(dir.path(), None)?;
    local
        .task_create(CreateTaskRequest {
            id: id.to_string(),
            bundle: dir.path().to_str().unwrap().to_string(),
            terminal,
            stdout: "/run/containerd/fifo/out".into(),
            ..Default::default()
        })
        .await?;
    Ok((local, rx, dir))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_task_with_a_terminal_can_be_created_and_the_flag_reaches_the_instance() -> anyhow::Result<()> {
    let (local, _rx, _dir) = task_with_terminal("tt1", true).await?;
    let i = local.get_instance("tt1").await?;
    assert!(i.config.terminal);
    let st = local.task_state(StateRequest { id: "tt1".into(), ..Default::default() }).await?;
    assert!(st.terminal, "State says the task has a terminal");
    assert_eq!(st.status(), Status::CREATED);

    let (local2, _rx2, _dir2) = task_with_terminal("tt2", false).await?;
    assert!(!local2.get_instance("tt2").await?.config.terminal);
    let st = local2.task_state(StateRequest { id: "tt2".into(), ..Default::default() }).await?;
    assert!(!st.terminal);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resize_and_close_io_of_the_task_itself_reach_the_instance_only_when_they_make_sense() -> anyhow::Result<()> {
    let (local, _rx, _dir) = task_with_terminal("tt3", true).await?;
    let resize = |w, h| ResizePtyRequest { id: "tt3".into(), width: w, height: h, ..Default::default() };
    let close = || CloseIORequest { id: "tt3".into(), stdin: true, ..Default::default() };

    // Not started: a resize is refused, a close does nothing.
    assert!(local.task_resize_pty(resize(80, 24)).await.is_err());
    local.task_close_io(close()).await?;

    local.task_start(StartRequest { id: "tt3".into(), ..Default::default() }).await?;
    local.task_resize_pty(resize(120, 40)).await?;
    local.task_close_io(close()).await?;
    // CloseIO that is not about stdin does nothing.
    local.task_close_io(CloseIORequest { id: "tt3".into(), stdin: false, ..Default::default() }).await?;
    let i = local.get_instance("tt3").await?;
    assert_eq!(i.instance.calls.lock().unwrap().clone(), vec!["task resize 120x40", "task close_stdin"]);

    // No such task.
    assert!(matches!(
        local.task_resize_pty(ResizePtyRequest { id: "nope".into(), width: 1, height: 1, ..Default::default() }).await,
        Err(Error::NotFound(_))
    ));

    // A task without a terminal has nothing to resize.
    let (plain, _rx2, _dir2) = task_with_terminal("tt4", false).await?;
    plain.task_start(StartRequest { id: "tt4".into(), ..Default::default() }).await?;
    assert!(
        plain
            .task_resize_pty(ResizePtyRequest { id: "tt4".into(), width: 80, height: 24, ..Default::default() })
            .await
            .is_err()
    );

    local.task_kill(KillRequest { id: "tt3".into(), signal: 9, ..Default::default() }).await?;
    plain.task_kill(KillRequest { id: "tt4".into(), signal: 9, ..Default::default() }).await?;
    Ok(())
}
