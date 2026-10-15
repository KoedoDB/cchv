//! Shadow VM management for warm workload snapshots.
//!
//! A shadow VM is a throwaway instance that exists solely to produce a
//! warm workload snapshot. It runs outside Kubernetes, has no networking,
//! and is destroyed after the snapshot is captured.
//!
//! Flow:
//! 1. Restore from base snapshot (clean VM, agent ready)
//! 2. Hot-plug container rootfs disk
//! 3. RunContainer via agent RPC (workload starts)
//! 4. Wait for warmup (configurable, default 30s)
//! 5. Pause → snapshot → destroy the VM
//! 6. Hand the snapshot over (root's alone, rootless), check it, cache it under the image key
//!
//! The image key is made by the daemon from the content of the image (`imagekey.rs`), and the snapshot is the work of a VM that ran
//! that image: the VM is dead, and its user has no process left, before anything in the snapshot directory is read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use log::{info, warn};
use tokio::sync::Mutex;

use crate::config::DaemonConfig;
use crate::pool::{Pool, PoolVm};

/// How long a shadow that failed for an image is not tried again (each one holds a pool VM for its warm-up).
const FAILURE_BACKOFF: std::time::Duration = std::time::Duration::from_secs(300);

/// Metadata for a cached workload snapshot.
struct SnapshotEntry {
    /// Path to the snapshot directory.
    dir: PathBuf,
    /// Last time this snapshot was used (for LRU eviction).
    last_used: std::time::Instant,
}

/// Manages workload snapshots created by shadow VMs.
pub struct SnapshotManager {
    config: DaemonConfig,
    pool: Arc<Pool>,
    /// Cached workload snapshots: image_key → entry
    snapshots: Mutex<HashMap<String, SnapshotEntry>>,
    /// Shadow VMs currently running (image_key → task handle). Looked at and added to under one lock.
    active_shadows: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// Images whose shadow failed, and when.
    failed: Mutex<HashMap<String, std::time::Instant>>,
    /// Maximum number of cached snapshots.
    max_snapshots: usize,
}

impl SnapshotManager {
    pub fn new(config: DaemonConfig, pool: Arc<Pool>) -> Arc<Self> {
        let max = config.max_snapshots;
        Arc::new(Self {
            config,
            pool,
            snapshots: Mutex::new(HashMap::new()),
            active_shadows: Mutex::new(HashMap::new()),
            failed: Mutex::new(HashMap::new()),
            max_snapshots: max,
        })
    }

    /// Whether a workload snapshot exists for the given image key. The key is a hash of the image's content, so a changed image has
    /// another key and there is nothing to invalidate.
    pub async fn has_snapshot(&self, image_key: &str) -> bool {
        self.snapshots.lock().await.contains_key(image_key)
    }

    /// Get the snapshot directory and update last-used time.
    pub async fn get_snapshot_dir(&self, image_key: &str) -> Option<PathBuf> {
        let mut snapshots = self.snapshots.lock().await;
        if let Some(entry) = snapshots.get_mut(image_key) {
            entry.last_used = std::time::Instant::now();
            Some(entry.dir.clone())
        } else {
            None
        }
    }

    /// Forget a snapshot and remove its directory (it did not pass a check, or the image is gone).
    pub async fn drop_snapshot(&self, image_key: &str) {
        if let Some(entry) = self.snapshots.lock().await.remove(image_key) {
            let _ = std::fs::remove_dir_all(&entry.dir);
        }
    }

    /// Restore a VM from a workload snapshot. Returns the VM details.
    /// The caller is responsible for configuring networking after restore.
    ///
    /// `erofs_path` is the image of this pod. The snapshot's own account of its disks is not used (`prepare_snapshot` checks the whole
    /// configuration and puts this path in the place of the image disk), and the VM's user is given this image, the two big files of
    /// the snapshot, and nothing else. If the snapshot does not pass the check it is dropped, and the error is returned (the pod then
    /// gets a VM from the pool). After the Cloud Hypervisor has started, any failure stops it and removes everything of the VM.
    pub async fn restore_from_snapshot(
        &self,
        image_key: &str,
        cid: u64,
        state_dir: &Path,
        erofs_path: &str,
    ) -> Result<(u32, PathBuf, PathBuf)> {
        use crate::vm_lifecycle;

        let snap_dir = self
            .get_snapshot_dir(image_key)
            .await
            .context("snapshot not found")?;

        let vsock_socket = state_dir.join("vsock.sock");
        let snapshot_work_dir = match vm_lifecycle::prepare_snapshot(
            &self.config,
            &snap_dir,
            state_dir,
            cid,
            &vsock_socket,
            Some(erofs_path),
        ) {
            Ok(dir) => dir,
            Err(e) => {
                warn!("snapshot {image_key} is not used and is dropped: {e:#}");
                self.drop_snapshot(image_key).await;
                return Err(e);
            }
        };

        let ch_pid = vm_lifecycle::spawn_ch(&self.config, state_dir, self.config.default_memory_mb, self.config.default_vcpus).await?;
        let api_socket = state_dir.join("api.sock");
        let steps = async {
            // Rootless: the VM may read the two big files of this snapshot (hard links of the cached ones) and the image, and nothing
            // else of it.
            // The grant is made on the cached file (the VM's directory has a hard link to the same file, in a place the VM could
            // change), so it can be taken back from the same file.
            for name in ["memory-ranges", "state.json"] {
                crate::rootless::allow_read(&snap_dir.join(name), state_dir)
                    .context("let the VM read the snapshot")?;
            }
            crate::rootless::allow_read(Path::new(erofs_path), state_dir)
                .context("let the VM read the image")?;
            vm_lifecycle::wait_ch_ready(&api_socket).await?;
            vm_lifecycle::restore_vm(&api_socket, &snapshot_work_dir).await?;
            vm_lifecycle::resume_vm(&api_socket).await
        }
        .await;
        if let Err(e) = steps {
            vm_lifecycle::shutdown_and_destroy(&api_socket, ch_pid, state_dir).await;
            return Err(e);
        }

        // Skip agent health check for warm restores — the agent was verified
        // when the snapshot was created. The shim will connect directly.
        // Connecting here would hold the vsock proxy and delay the shim.

        info!("restored workload snapshot for image_key={}", image_key);
        Ok((ch_pid, api_socket, vsock_socket))
    }

    /// Trigger shadow VM creation for an image key.
    /// Does nothing if a shadow is already running, a snapshot exists, one failed lately, or there are as many shadows as allowed.
    pub async fn trigger_shadow(self: &Arc<Self>, image_key: &str, erofs_path: &str) {
        if !crate::validate::is_id(image_key) {
            return;
        }
        // Skip if snapshot already exists
        if self.has_snapshot(image_key).await {
            return;
        }
        if let Some(when) = self.failed.lock().await.get(image_key) {
            if when.elapsed() < FAILURE_BACKOFF {
                return;
            }
        }

        // The check and the insert are under one lock: two acquires for one image start one shadow, and a shadow that ends at once
        // removes its entry only after it was put there.
        let mut shadows = self.active_shadows.lock().await;
        if shadows.contains_key(image_key) || shadows.len() >= self.config.max_shadow_vms {
            return;
        }
        let mgr = Arc::clone(self);
        let key = image_key.to_string();
        let erofs = erofs_path.to_string();
        let handle = tokio::spawn(async move {
            if let Err(e) = mgr.run_shadow(&key, &erofs).await {
                warn!("shadow VM failed for image_key={}: {:#}", key, e);
                mgr.failed
                    .lock()
                    .await
                    .insert(key.clone(), std::time::Instant::now());
            }
            mgr.active_shadows.lock().await.remove(&key);
        });
        shadows.insert(image_key.to_string(), handle);
        info!("shadow VM triggered for image_key={}", image_key);
    }

    /// Run a shadow VM: restore base → hot-plug rootfs → run workload →
    /// warmup → pause → snapshot → destroy → hand over, check, cache.
    async fn run_shadow(&self, image_key: &str, erofs_path: &str) -> Result<()> {
        let t0 = std::time::Instant::now();
        let snap_dir = self.config.snapshot_cache_dir().join(image_key);

        // 1. Acquire a clean VM from pool (or restore synchronously)
        let pool_vm = self.pool.acquire().await.context("acquire for shadow")?;
        info!(
            "shadow VM for {} using pool VM {} (pid={})",
            image_key, pool_vm.vm_id, pool_vm.ch_pid
        );

        // 2-5. Everything the VM does. Whatever happens, the VM is destroyed next, and the snapshot directory is not touched by root
        // before that.
        let steps = self
            .shadow_steps(&pool_vm, image_key, erofs_path, &snap_dir)
            .await;
        let released = self.pool.release(&pool_vm.vm_id).await;
        let vm_dir = pool_vm.api_socket.parent().map(|p| p.to_path_buf());
        if let Err(e) = steps {
            if let Some(d) = &vm_dir {
                crate::rootless::discard_outbox(d);
            }
            let _ = std::fs::remove_dir_all(&snap_dir);
            return Err(e);
        }
        released.context("destroy the shadow VM")?;
        // A confined VM wrote its snapshot into its outbox: it is moved to where it will live, still untrusted until handed over.
        if let Some(d) = &vm_dir {
            if let Err(e) = crate::rootless::collect_outbox(d, &snap_dir) {
                let _ = std::fs::remove_dir_all(&snap_dir);
                return Err(e);
            }
        }

        // 6. The VM is gone. Now the directory is root's: hand it over (links and the like are removed), then it can be read.
        let cached = (|| -> Result<()> {
            crate::rootless::hand_over_snapshot(&snap_dir, false)?;
            // Strip kernel ip= from snapshot (networking is per-pod)
            strip_kernel_ip(&snap_dir)?;
            // Check it now as it will be at a restore, so that a bad snapshot is never cached.
            let cfg: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(snap_dir.join("config.json"))?)?;
            crate::validate::validate_snapshot_config(
                &cfg,
                &self.config.kernel_path,
                &self.config.rootfs_path,
                Some(erofs_path),
                crate::validate::SnapshotSize::of(&self.config),
            )
            .context("the snapshot's configuration is not accepted")?;
            Ok(())
        })();
        if let Err(e) = cached {
            let _ = std::fs::remove_dir_all(&snap_dir);
            return Err(e);
        }

        {
            let mut snapshots = self.snapshots.lock().await;

            // LRU eviction if at capacity
            while snapshots.len() >= self.max_snapshots {
                // Find the least recently used entry
                let lru_key = snapshots
                    .iter()
                    .min_by_key(|(_, e)| e.last_used)
                    .map(|(k, _)| k.clone());
                if let Some(key) = lru_key {
                    if let Some(entry) = snapshots.remove(&key) {
                        info!("evicting LRU snapshot: {}", key);
                        let _ = std::fs::remove_dir_all(&entry.dir);
                    }
                } else {
                    break;
                }
            }

            snapshots.insert(
                image_key.to_string(),
                SnapshotEntry {
                    dir: snap_dir,
                    last_used: std::time::Instant::now(),
                },
            );
        }

        info!(
            "shadow for {} complete in {}ms (warmup {}s)",
            image_key,
            t0.elapsed().as_millis(),
            self.config.warmup_duration_secs
        );
        Ok(())
    }

    /// The work of the shadow VM: the image is plugged in, its container runs, the guest is paused and snapshotted into `snap_dir`
    /// (made fresh here).
    async fn shadow_steps(
        &self,
        pool_vm: &PoolVm,
        image_key: &str,
        erofs_path: &str,
        snap_dir: &Path,
    ) -> Result<()> {
        use crate::vm_lifecycle;
        let t0 = std::time::Instant::now();
        let api_socket = &pool_vm.api_socket;
        let vsock_socket = &pool_vm.vsock_socket;

        // 2. Hot-plug container rootfs
        let disk_json = serde_json::json!({
            "path": erofs_path,
            "readonly": true,
            "image_type": "Raw",
            "id": "ctr-rootfs",
        });
        if let Some(state_dir) = api_socket.parent() {
            crate::rootless::allow_read(Path::new(erofs_path), state_dir)
                .context("let the shadow VM read the rootfs")?;
        }
        vm_lifecycle::api_request_with_body(api_socket, "PUT", "/api/v1/vm.add-disk", &disk_json.to_string())
            .await
            .context("hot-plug rootfs for shadow")?;
        info!("shadow {}: rootfs hot-plugged ({}ms)", image_key, t0.elapsed().as_millis());

        // 3. Run container via agent RPC
        // Connect to agent and send RunContainer
        let (agent, _health) = vm_lifecycle::connect_agent(vsock_socket).await?;

        let mut run_req = cloudhv_proto::CreateContainerRequest::new();
        run_req.container_id = crate::api::shadow_container_id(image_key);
        run_req.rootfs_preattached = false;
        run_req.erofs_layers = 1;
        // Intentionally empty OCI config: the shadow container only exists to
        // warm up the rootfs and runtime (file caches, shared libraries, etc.).
        // The agent falls back to defaults for an empty config, which is
        // sufficient for warmup. The real pod's OCI config will be applied
        // when restoring from this snapshot.
        // (An empty object is refused by the agent, which will not run a spec without `process` and `linux`; this one has both and
        // asks for nothing: the process ends at once, whether or not the image has the program.)
        run_req.config_json = br#"{"ociVersion":"1.0.2","root":{"path":"rootfs"},"process":{"args":["/bin/true"],"cwd":"/"},"linux":{}}"#.to_vec();

        let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(30));
        let run_resp = agent
            .run_container(ctx, &run_req)
            .await
            .context("RunContainer in shadow VM")?;
        info!(
            "shadow {}: container started (pid={}, {}ms)",
            image_key,
            run_resp.pid,
            t0.elapsed().as_millis()
        );

        // 4. Wait for warmup
        let warmup = self.config.warmup_duration_secs;
        info!("shadow {}: warming up for {}s...", image_key, warmup);
        tokio::time::sleep(std::time::Duration::from_secs(warmup)).await;

        // 5. Pause + snapshot, into a directory made now (nothing of an earlier one is in it)
        let _ = std::fs::remove_dir_all(snap_dir);
        std::fs::create_dir(snap_dir).with_context(|| format!("create {}", snap_dir.display()))?;
        vm_lifecycle::pause_vm(api_socket).await?;
        vm_lifecycle::snapshot_vm(api_socket, snap_dir).await?;
        info!("shadow {}: snapshot created ({}ms total)", image_key, t0.elapsed().as_millis());
        Ok(())
    }

    /// Number of active shadow VMs.
    pub async fn active_shadow_count(&self) -> usize {
        self.active_shadows.lock().await.len()
    }

    /// List cached snapshot keys.
    pub async fn snapshot_keys(&self) -> Vec<String> {
        self.snapshots.lock().await.keys().cloned().collect()
    }
}

/// Remove `ip=...` from the kernel cmdline in a snapshot's config.json. Only on a snapshot that has been handed over (root's, no VM
/// can still be writing to it).
fn strip_kernel_ip(snap_dir: &Path) -> Result<()> {
    let config_path = snap_dir.join("config.json");
    let config_str = std::fs::read_to_string(&config_path)?;
    let mut config: serde_json::Value = serde_json::from_str(&config_str)?;

    if let Some(cmdline) = config.pointer_mut("/payload/cmdline") {
        if let Some(s) = cmdline.as_str() {
            let stripped = s
                .split_whitespace()
                .filter(|part| !part.starts_with("ip="))
                .collect::<Vec<_>>()
                .join(" ");
            *cmdline = serde_json::json!(stripped);
        }
    }

    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kernel_ip_is_stripped_from_the_command_line() {
        let d = std::env::temp_dir().join(format!("cloudhv-shadow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("config.json"),
            r#"{"payload":{"kernel":"/k","cmdline":"console=ttyS0 ip=10.0.0.2::10.0.0.1:255.255.255.0 root=/dev/vda"}}"#,
        )
        .unwrap();
        strip_kernel_ip(&d).unwrap();
        let cfg: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(d.join("config.json")).unwrap()).unwrap();
        assert_eq!(cfg["payload"]["cmdline"], "console=ttyS0 root=/dev/vda");
        let _ = std::fs::remove_dir_all(&d);
    }
}
