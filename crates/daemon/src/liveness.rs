//! The daemon decides whether a VM handed to a pod is alive.
//!
//! Until now nobody did: a pool VM was looked at by the pool's reaper, but a VM in a pod's hands was left to the shim ("the shim watches its
//! process"). The shim polls `/proc/<pid>`, and when the shim is gone, restarted or confused (it decided the VM was lost, the daemon
//! still lists it), the VM stays in the daemon's list, its agent connection stays open, and nothing ever releases it: VMs without an owner.
//!
//! Two watches, both by what is true now and not by what a file says (the registry in `.ctl/` is a hint for a restarted daemon, never the
//! answer to "is it alive"):
//!
//! 1. **The process**, every `process_every`: a VM whose Cloud Hypervisor is gone (no process, a zombie, or a process that is not that
//!    VM's any more: the pid was reused) is released at once, with its agent connection, whoever else knew about it.
//! 2. **The guest**, every `check_every`: the agent's `Check` (a call over vsock that needs the guest's kernel, its agent and the vsock to
//!    work, which the Cloud Hypervisor's own API does not: that answers while the guest is hung). A VM that fails `failures` checks in a
//!    row (each with `check_timeout`) has a hung guest. What happens then is `action`: a warning that says it would be killed (`Log`, the
//!    default, to learn how often a healthy VM misses checks before anything is killed), or its Cloud Hypervisor is killed (`Kill`) and the
//!    first watch releases it. A slow guest is not a hung one, so a failure is not counted when the VM is not `Running` (paused by the daemon
//!    itself for a snapshot or a hot-plug) or when the daemon's own round came late (the daemon or the host stalled: the VMs could not be
//!    asked in time), and a check that works resets the count. The limits are long (by default 5 failures, one every 30 s, each with 10 s).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use log::{info, warn};

use crate::api::{forget_vm, AgentRegistry};
use crate::pool::Pool;

/// A check that answered but took this long is logged.
const SLOW_CHECK: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    /// How often the processes are looked at.
    pub process_every: Duration,
    /// How often each VM's guest is checked. Zero: no guest checks.
    pub check_every: Duration,
    /// How long one `Check` may take.
    pub check_timeout: Duration,
    /// How many checks in a row may fail before the VM counts as hung.
    pub failures: u32,
    /// What a hung VM gets.
    pub action: crate::config::GuestCheckAction,
}

/// Whether the answer of `vm.info` says the VM is running (not paused, not shut down).
pub fn is_running(vm_info: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(vm_info)
        .ok()
        .and_then(|v| {
            v.get("state")
                .and_then(|s| s.as_str())
                .map(|s| s == "Running")
        })
        .unwrap_or(false)
}

/// The health clients of the VMs that have an agent connection (the agent registry keeps only the agent client).
static HEALTH: LazyLock<Mutex<HashMap<String, cloudhv_proto::HealthServiceClient>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn register(vm_id: &str, health: cloudhv_proto::HealthServiceClient) {
    HEALTH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(vm_id.to_string(), health);
}

pub fn forget(vm_id: &str) {
    HEALTH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(vm_id);
}

fn health_clients() -> Vec<(String, cloudhv_proto::HealthServiceClient)> {
    HEALTH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Consecutive failed checks, by VM.
#[derive(Default)]
pub struct Strikes(HashMap<String, u32>);

impl Strikes {
    /// Record the result of a check. Returns true when the VM has now failed `limit` checks in a row.
    pub fn record(&mut self, vm_id: &str, ok: bool, limit: u32) -> bool {
        if ok {
            self.0.remove(vm_id);
            return false;
        }
        let n = self.0.entry(vm_id.to_string()).or_insert(0);
        *n += 1;
        *n >= limit.max(1)
    }

    /// Forget the VMs that are not in `alive` any more.
    pub fn keep_only(&mut self, alive: &[String]) {
        self.0.retain(|id, _| alive.iter().any(|a| a == id));
    }
}

pub fn spawn(pool: std::sync::Arc<Pool>, agents: AgentRegistry, s: Settings) {
    {
        let (pool, agents) = (pool.clone(), agents.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(s.process_every).await;
                for vm in pool.dead_active().await {
                    warn!(
                        "VM {} (pid {}) is gone (its Cloud Hypervisor is not there): releasing it",
                        vm.vm_id, vm.ch_pid
                    );
                    forget_vm(&agents, &vm.vm_id).await;
                    let _ = pool.release(&vm.vm_id).await;
                }
            }
        });
    }
    if s.check_every.is_zero() {
        info!("guest checks are off (guest_check_secs = 0)");
        return;
    }
    tokio::spawn(async move {
        let mut strikes = Strikes::default();
        let mut last_round = std::time::Instant::now();
        loop {
            tokio::time::sleep(s.check_every).await;
            // A round that came much later than planned: the daemon (or the host) stalled, and the guests could not be asked in time.
            let late = last_round.elapsed() > s.check_every * 2;
            last_round = std::time::Instant::now();
            let clients = health_clients();
            let ids: Vec<String> = clients.iter().map(|(id, _)| id.clone()).collect();
            strikes.keep_only(&ids);
            // All the VMs at once: a hung one takes its time-out, and the others are not held up by it.
            let mut set = tokio::task::JoinSet::new();
            for (id, health) in clients {
                set.spawn(async move {
                    let started = std::time::Instant::now();
                    let ctx = ttrpc::context::with_duration(s.check_timeout);
                    let ok = matches!(
                        tokio::time::timeout(
                            s.check_timeout + Duration::from_secs(1),
                            health.check(ctx, &cloudhv_proto::CheckRequest::new())
                        )
                        .await,
                        Ok(Ok(r)) if r.ready
                    );
                    (id, ok, started.elapsed())
                });
            }
            while let Some(done) = set.join_next().await {
                let Ok((id, ok, took)) = done else { continue };
                // The time of an answer, for choosing the limits from what healthy VMs do under load: a check that answered but took a
                // while is logged too.
                if ok && took >= SLOW_CHECK {
                    info!("VM {id}: a check took {} ms", took.as_millis());
                }
                if !ok {
                    warn!("VM {id}: the guest did not answer a check");
                    if late {
                        warn!("VM {id}: not counted: this round came late (the daemon or the host stalled)");
                        continue;
                    }
                    if let Some(sock) = pool.api_socket_of(&id) {
                        // The Cloud Hypervisor answers while the guest is hung; if it does and the VM is not running, the daemon
                        // paused it. (If it does not answer either, the failure counts.)
                        if let Ok(info) = crate::vm_lifecycle::api_request_text(
                            &sock,
                            "GET",
                            "/api/v1/vm.info",
                            None,
                            Duration::from_secs(5),
                        )
                        .await
                        {
                            if !is_running(&info) {
                                warn!("VM {id}: not counted: the VM is not running (paused)");
                                continue;
                            }
                        }
                    }
                }
                if strikes.record(&id, ok, s.failures) {
                    match s.action {
                        crate::config::GuestCheckAction::Kill => {
                            if pool.kill_ch(&id) {
                                warn!(
                                    "VM {id}: {} checks in a row failed: its guest is hung, its Cloud Hypervisor is killed",
                                    s.failures
                                );
                            }
                        }
                        crate::config::GuestCheckAction::Log => warn!(
                            "VM {id}: {} checks in a row failed: its guest looks hung; guest_check_action is \"log\", so nothing is done (\"kill\" would kill its Cloud Hypervisor)",
                            s.failures
                        ),
                    }
                    // Counting starts again: while it stays hung this warns every `failures` rounds.
                    strikes.keep_only(&[]);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vm_is_hung_after_the_limit_in_a_row_and_a_good_check_resets_the_count() {
        let mut s = Strikes::default();
        assert!(!s.record("a", false, 3));
        assert!(!s.record("a", false, 3));
        assert!(!s.record("a", true, 3));
        assert!(!s.record("a", false, 3));
        assert!(!s.record("a", false, 3));
        assert!(s.record("a", false, 3));
    }

    #[test]
    fn only_a_running_vm_counts_as_running() {
        assert!(is_running(r#"{"config":{},"state":"Running"}"#));
        assert!(!is_running(r#"{"state":"Paused"}"#));
        assert!(!is_running(r#"{"state":"Shutdown"}"#));
        assert!(!is_running("not json"));
        assert!(!is_running("{}"));
    }

    #[test]
    fn the_vms_are_counted_apart_and_a_gone_vm_is_forgotten() {
        let mut s = Strikes::default();
        assert!(!s.record("a", false, 2));
        assert!(!s.record("b", false, 2));
        assert!(s.record("a", false, 2));
        s.keep_only(&["b".to_string()]);
        assert!(s.record("b", false, 2));
        assert!(!s.record("a", false, 2));
    }
}
