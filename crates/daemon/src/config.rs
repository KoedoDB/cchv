use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Daemon configuration loaded from a JSON file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// Number of warm VMs to keep ready in the pool.
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,

    /// Maximum idle VMs in the pool (hard cap).
    #[serde(default = "default_max_pool_size")]
    pub max_pool_size: usize,

    /// Delay in ms before replenishing pool after an acquire.
    #[serde(default = "default_replenish_delay_ms")]
    pub replenish_delay_ms: u64,

    /// Destroy idle VMs after this many seconds.
    #[serde(default = "default_idle_timeout_secs")]
    pub vm_idle_timeout_secs: u64,

    /// Default vCPUs per pool VM.
    #[serde(default = "default_vcpus")]
    pub default_vcpus: u32,

    /// Default memory in MiB per pool VM.
    #[serde(default = "default_memory_mb")]
    pub default_memory_mb: u64,

    /// Path to cloud-hypervisor binary.
    #[serde(default = "default_ch_binary")]
    pub cloud_hypervisor_binary: String,

    /// Path to guest kernel (vmlinux).
    pub kernel_path: String,

    /// Path to guest rootfs image (erofs).
    pub rootfs_path: String,

    /// Kernel command line arguments.
    #[serde(default = "default_kernel_args")]
    pub kernel_args: String,

    /// Unix socket path for the daemon's ttrpc API.
    #[serde(default = "default_socket_path")]
    pub socket_path: String,

    /// Directory for daemon state (base snapshot, pool VM state).
    #[serde(default = "default_state_dir")]
    pub state_dir: String,

    /// Warmup duration in seconds for shadow VMs before snapshotting.
    #[serde(default = "default_warmup_secs")]
    pub warmup_duration_secs: u64,

    /// Run each Cloud Hypervisor as a user of its own, not root (see rootless.rs). Off by default.
    #[serde(default)]
    pub ch_rootless: bool,

    /// First user ID given to VMs when `ch_rootless` is on; `ch_uid_count` IDs from here belong to the daemon.
    #[serde(default = "default_ch_uid_base")]
    pub ch_uid_base: u32,

    #[serde(default = "default_ch_uid_count")]
    pub ch_uid_count: u32,

    /// Group Cloud Hypervisor runs as; the group of /dev/kvm when not set.
    #[serde(default)]
    pub ch_gid: Option<u32>,

    /// Maximum number of cached workload snapshots.
    /// Oldest (LRU) snapshots are evicted when this limit is reached.
    #[serde(default = "default_max_snapshots")]
    pub max_snapshots: usize,

    /// Where the shim keeps the container images (erofs). The daemon accepts an image path only inside this directory, and with
    /// `ch_rootless` gives a VM read access to nothing else outside its own snapshot. Must be the shim's `EROFS_CACHE_DIR`.
    #[serde(default = "default_erofs_cache_dir")]
    pub erofs_cache_dir: String,

    /// The most memory (MiB) and vCPUs a request may ask a VM for. A request above is refused: the sizes come from a pod's limits,
    /// which a customer chooses.
    #[serde(default = "default_max_memory_mb")]
    pub max_memory_mb: u64,
    #[serde(default = "default_max_vcpus")]
    pub max_vcpus: u32,

    /// The most VMs that may be handed to pods at once.
    #[serde(default = "default_max_active_vms")]
    pub max_active_vms: usize,

    /// The most shadow VMs (the ones that make warm snapshots) that may run at once; each takes a pool VM for its warm-up.
    #[serde(default = "default_max_shadow_vms")]
    pub max_shadow_vms: usize,

    /// The longest request line, in bytes, and the most connections the API serves at once.
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,

    /// Whether a VM's serial console is written to `console.log` in its state directory. Off by default with `ch_rootless`: the file is
    /// in memory (`/run`), has no size limit, and the guest writes to it. On by default otherwise.
    #[serde(default)]
    pub serial_console: Option<bool>,

    /// The most processes (threads count) the user of a rootless Cloud Hypervisor may have (`RLIMIT_NPROC`).
    #[serde(default = "default_ch_max_processes")]
    pub ch_max_processes: u64,
}

fn default_erofs_cache_dir() -> String {
    "/run/cloudhv/erofs-cache".to_string()
}
fn default_max_memory_mb() -> u64 {
    65_536
}
fn default_max_vcpus() -> u32 {
    64
}
fn default_max_active_vms() -> usize {
    512
}
fn default_max_shadow_vms() -> usize {
    2
}
fn default_max_request_bytes() -> usize {
    8 * 1024 * 1024
}
fn default_max_connections() -> usize {
    64
}
fn default_ch_max_processes() -> u64 {
    1024
}
fn default_ch_uid_base() -> u32 {
    200_000
}
fn default_ch_uid_count() -> u32 {
    60_000
}
fn default_pool_size() -> usize {
    3
}
fn default_max_pool_size() -> usize {
    10
}
fn default_replenish_delay_ms() -> u64 {
    100
}
fn default_idle_timeout_secs() -> u64 {
    300
}
fn default_vcpus() -> u32 {
    1
}
fn default_memory_mb() -> u64 {
    512
}
fn default_ch_binary() -> String {
    "/usr/local/bin/cloud-hypervisor".to_string()
}
fn default_kernel_args() -> String {
    "console=ttyS0 root=/dev/vda rw init=/init net.ifnames=0".to_string()
}
fn default_socket_path() -> String {
    "/run/cloudhv/daemon.sock".to_string()
}
fn default_state_dir() -> String {
    "/run/cloudhv/daemon".to_string()
}
fn default_warmup_secs() -> u64 {
    30
}
fn default_max_snapshots() -> usize {
    100
}

impl DaemonConfig {
    /// Load configuration from a JSON file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("read config: {}", path.display()))?;
        let config: Self = serde_json::from_str(&content).with_context(|| "parse daemon config")?;
        Ok(config)
    }

    /// Whether the serial console of a VM is written to a file (see `serial_console`).
    pub fn serial_console_enabled(&self) -> bool {
        self.serial_console.unwrap_or(!self.ch_rootless)
    }

    /// Path to the base snapshot directory.
    pub fn base_snapshot_dir(&self) -> PathBuf {
        PathBuf::from(&self.state_dir).join("base-snapshot")
    }

    /// Path to the workload snapshot cache directory.
    pub fn snapshot_cache_dir(&self) -> PathBuf {
        PathBuf::from(&self.state_dir).join("snapshot-cache")
    }
}
