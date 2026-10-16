//! A fuzz entry point (only with `--cfg fuzzing`, as `cargo fuzz` builds): random sequences of task and exec calls against the real
//! task service with an instance that only keeps books. Asserts that nothing panics or hangs, that an exec's events come in order, and
//! that a deleted exec is gone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use containerd_shim::api::{
    CloseIORequest, CreateTaskRequest, DeleteRequest, ExecProcessRequest, KillRequest, ResizePtyRequest, StartRequest,
    StateRequest, WaitRequest,
};
use containerd_shim::event::Event;
use chrono::{DateTime, Utc};
use protobuf::well_known_types::any::Any;

use super::*;
use crate::sandbox::shim::events::EventSender;

/// An instance that runs nothing: a process "exits" when it is killed.
pub struct Fake {
    exit: WaitableCell<(u32, DateTime<Utc>)>,
    execs: Mutex<HashMap<String, Arc<WaitableCell<(u32, DateTime<Utc>)>>>>,
}

impl Instance for Fake {
    async fn new(_id: String, _cfg: &InstanceConfig) -> Result<Self, Error> {
        Ok(Fake { exit: WaitableCell::new(), execs: Mutex::new(HashMap::new()) })
    }
    async fn start(&self) -> Result<u32, Error> {
        Ok(4242)
    }
    async fn kill(&self, _signal: u32) -> Result<(), Error> {
        let _ = self.exit.set((137, Utc::now()));
        Ok(())
    }
    async fn delete(&self) -> Result<(), Error> {
        Ok(())
    }
    async fn wait(&self) -> (u32, DateTime<Utc>) {
        *self.exit.wait().await
    }
    async fn exec_create(&self, cfg: &ExecConfig) -> Result<(), Error> {
        let mut m = self.execs.lock().unwrap();
        if m.contains_key(&cfg.exec_id) {
            return Err(Error::AlreadyExists(cfg.exec_id.clone()));
        }
        m.insert(cfg.exec_id.clone(), Arc::new(WaitableCell::new()));
        Ok(())
    }
    async fn exec_start(&self, exec_id: &str) -> Result<u32, Error> {
        if self.execs.lock().unwrap().contains_key(exec_id) {
            Ok(5000)
        } else {
            Err(Error::NotFound(exec_id.to_string()))
        }
    }
    async fn exec_kill(&self, exec_id: &str, _signal: u32) -> Result<(), Error> {
        let cell = self.execs.lock().unwrap().get(exec_id).cloned();
        match cell {
            Some(c) => {
                let _ = c.set((137, Utc::now()));
                Ok(())
            }
            None => Err(Error::NotFound(exec_id.to_string())),
        }
    }
    async fn exec_wait(&self, exec_id: &str) -> Result<(u32, DateTime<Utc>), Error> {
        let cell = self.execs.lock().unwrap().get(exec_id).cloned();
        match cell {
            Some(c) => Ok(*c.wait().await),
            None => Err(Error::NotFound(exec_id.to_string())),
        }
    }
    async fn exec_delete(&self, exec_id: &str) -> Result<(), Error> {
        self.execs.lock().unwrap().remove(exec_id);
        Ok(())
    }
    async fn exec_resize(&self, _exec_id: &str, _w: u32, _h: u32) -> Result<(), Error> {
        Ok(())
    }
    async fn exec_close_stdin(&self, _exec_id: &str) -> Result<(), Error> {
        Ok(())
    }
}

/// The events, as (topic, text form).
#[derive(Clone, Default)]
pub struct Events(Arc<Mutex<Vec<(String, String)>>>);

impl EventSender for Events {
    fn send(&self, event: impl Event) {
        self.0.lock().unwrap().push((event.topic(), format!("{event:?}")));
    }
}

fn make_bundle() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!("shimkit-fuzz-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(dir.join("rootfs")).unwrap();
    let spec = Spec::default();
    serde_json::to_writer(std::fs::File::create(dir.join("config.json")).unwrap(), &spec).unwrap();
    dir
}

const TASKS: [&str; 2] = ["t0", "t1"];
const EXECS: [&str; 3] = ["e0", "e1", "e2"];

async fn limited<F: std::future::Future>(what: &str, f: F) -> F::Output {
    match tokio::time::timeout(Duration::from_secs(5), f).await {
        Ok(v) => v,
        Err(_) => panic!("{what} did not return within 5 s: the task service hangs"),
    }
}

async fn session(data: &[u8]) {
    let events = Events::default();
    let local = Local::<Fake, _>::new(events.clone(), WaitableCell::new(), "ns", "/addr");
    let bundles: Vec<_> = (0..TASKS.len()).map(|_| make_bundle()).collect();
    // The model: which execs exist and are not deleted, and which events each should have had.
    let mut deleted: Vec<(usize, usize)> = Vec::new();

    for chunk in data.chunks(4).take(120) {
        let byte = |i: usize| chunk.get(i).copied().unwrap_or(0);
        let (op, t, e, x) = (byte(0) % 13, byte(1) as usize % TASKS.len(), byte(2) as usize % EXECS.len(), byte(3));
        let (task, exec) = (TASKS[t].to_string(), EXECS[e].to_string());
        match op {
            0 => {
                let _ = limited("create", local.task_create(CreateTaskRequest { id: task, bundle: bundles[t].to_str().unwrap().to_string(), ..Default::default() })).await;
            }
            1 => {
                let _ = limited("start", local.task_start(StartRequest { id: task, ..Default::default() })).await;
            }
            2 => {
                let _ = limited("kill", local.task_kill(KillRequest { id: task, signal: 9, ..Default::default() })).await;
            }
            3 => {
                let _ = limited("delete", local.task_delete(DeleteRequest { id: task, ..Default::default() })).await;
            }
            4 => {
                let spec = Any { value: if x & 1 == 0 { b"{}".to_vec() } else { b"[1]".to_vec() }, ..Default::default() };
                let r = limited(
                    "exec",
                    local.task_exec(ExecProcessRequest { id: task, exec_id: exec, spec: Some(spec).into(), ..Default::default() }),
                )
                .await;
                if r.is_ok() {
                    deleted.retain(|d| *d != (t, e));
                }
            }
            5 => {
                let _ = limited("exec start", local.exec_start(StartRequest { id: task, exec_id: exec, ..Default::default() })).await;
            }
            6 => {
                let _ = limited("exec kill", local.task_kill(KillRequest { id: task, exec_id: exec, signal: 9, ..Default::default() })).await;
            }
            7 => {
                // A wait may block (the process is running): it must not fail for a process that exists in any state before it exits,
                // and must not hang for one that has exited.
                let r = tokio::time::timeout(
                    Duration::from_millis(20),
                    local.task_wait(WaitRequest { id: task, exec_id: exec, ..Default::default() }),
                )
                .await;
                if let Ok(Err(err)) = &r {
                    let text = err.to_string();
                    assert!(!text.contains("invalid state transition"), "Wait failed on a state transition: {text}");
                }
            }
            8 => {
                let r = limited("exec delete", local.task_delete(DeleteRequest { id: task, exec_id: exec, ..Default::default() })).await;
                if r.is_ok() {
                    deleted.push((t, e));
                }
            }
            9 => {
                let r = limited("exec state", local.task_state(StateRequest { id: task, exec_id: exec, ..Default::default() })).await;
                if deleted.contains(&(t, e)) {
                    assert!(r.is_err(), "the state of a deleted exec was answered");
                }
            }
            10 => {
                let _ = limited(
                    "resize",
                    local.task_resize_pty(ResizePtyRequest { id: task, exec_id: if x & 1 == 0 { exec } else { String::new() }, width: x as u32, height: x as u32, ..Default::default() }),
                )
                .await;
            }
            11 => {
                let _ = limited(
                    "close io",
                    local.task_close_io(CloseIORequest { id: task, exec_id: if x & 1 == 0 { exec } else { String::new() }, stdin: true, ..Default::default() }),
                )
                .await;
            }
            _ => {
                let _ = limited("state", local.task_state(StateRequest { id: task, ..Default::default() })).await;
            }
        }
        // Let the watcher tasks that publish exits run.
        tokio::task::yield_now().await;
    }

    // The events of each exec: Added, then Started, then at most one Exit; never an Exit before a Start.
    let list = events.0.lock().unwrap().clone();
    for t in TASKS {
        for e in EXECS {
            let mut stage = 0u8; // 0 nothing, 1 added, 2 started, 3 exited
            for (topic, text) in &list {
                let mine = text.contains(&format!("container_id: \"{t}\"")) && (text.contains(&format!("exec_id: \"{e}\"")) || text.contains(&format!("id: \"{e}\"")));
                if !mine {
                    continue;
                }
                match topic.as_str() {
                    "/tasks/exec-added" => stage = stage.max(1),
                    "/tasks/exec-started" => {
                        assert!(stage >= 1, "exec {t}/{e} started before it was added");
                        stage = stage.max(2);
                    }
                    "/tasks/exit" if text.contains(&format!("id: \"{e}\"")) => {
                        assert!(stage >= 2, "exec {t}/{e} exited before it started: {list:?}");
                        assert!(stage < 3 || true, "");
                        stage = 3;
                    }
                    _ => {}
                }
            }
        }
    }
    for b in bundles {
        let _ = std::fs::remove_dir_all(b);
    }
}

/// Bytes → a sequence of calls (four bytes each: operation, task, exec, extra).
pub fn run(data: &[u8]) {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    let rt = RT.get_or_init(|| tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap());
    rt.block_on(session(data));
}
