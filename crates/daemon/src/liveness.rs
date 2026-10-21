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
//!    row (each with `check_timeout`) has a hung guest: its Cloud Hypervisor is killed, and the first watch then releases it. A slow guest
//!    is not a hung one: the limits are long (by default 5 failures of 5 s, one every 10 s: over a minute), and a check that works resets
//!    the count.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use log::{info, warn};

use crate::api::{forget_vm, AgentRegistry};
use crate::pool::Pool;

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
        loop {
            tokio::time::sleep(s.check_every).await;
            let clients = health_clients();
            let ids: Vec<String> = clients.iter().map(|(id, _)| id.clone()).collect();
            strikes.keep_only(&ids);
            // All the VMs at once: a hung one takes its time-out, and the others are not held up by it.
            let mut set = tokio::task::JoinSet::new();
            for (id, health) in clients {
                set.spawn(async move {
                    let ctx = ttrpc::context::with_duration(s.check_timeout);
                    let ok = matches!(
                        tokio::time::timeout(
                            s.check_timeout + Duration::from_secs(1),
                            health.check(ctx, &cloudhv_proto::CheckRequest::new())
                        )
                        .await,
                        Ok(Ok(r)) if r.ready
                    );
                    (id, ok)
                });
            }
            while let Some(done) = set.join_next().await {
                let Ok((id, ok)) = done else { continue };
                if !ok {
                    warn!("VM {id}: the guest did not answer a check");
                }
                if strikes.record(&id, ok, s.failures) {
                    if pool.kill_ch(&id) {
                        warn!(
                            "VM {id}: {} checks in a row failed: its guest is hung, its Cloud Hypervisor is killed",
                            s.failures
                        );
                    }
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
