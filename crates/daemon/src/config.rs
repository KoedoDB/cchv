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

    /// Accept `/proc/<pid>/ns/net` as a pod's network namespace. Off by default: a runtime that pins namespaces under /var/run/netns (the
    /// CNI way, which k3s and containerd use) does not need it, and a path of that form can name any process's namespace.
    #[serde(default)]
    pub allow_proc_netns: bool,

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

    /// The longest request line, in bytes, and the most connections the API serves at once (see `default_max_connections`).
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,

    /// Whether a VM's console (the virtio-console: the kernel's messages and the agent's log, the agent being the init) is written to `console.log` in its state
    /// directory. The name is the old one, from the serial port the guest kernel does not have. Off by default with `ch_rootless`: the file is in memory (`/run`),
    /// has no size limit, and the guest writes to it. On by default otherwise.
    #[serde(default)]
    pub serial_console: Option<bool>,

    /// With the console written to a file (`serial_console`): forward each new line of it to the daemon's log as `console[vm=<name>] <line>` (`consolefwd.rs`), so
    /// that a log collector has it and what a VM said before it died is kept. What was forwarded, except the last `console_keep_bytes`, is dropped from the file.
    #[serde(default = "default_true")]
    pub console_forward: bool,
    /// How often the files are read (ms), the most lines a VM may forward a second (a burst of five times that; the rest are counted, not forwarded), the longest line
    /// (bytes; a longer one is cut), the tail left in the file, and the least that is dropped at one time (a hole is not made for less).
    #[serde(default = "default_console_forward_interval_ms")]
    pub console_forward_interval_ms: u64,
    #[serde(default = "default_console_forward_max_lines_per_sec")]
    pub console_forward_max_lines_per_sec: u32,
    #[serde(default = "default_console_forward_max_line_bytes")]
    pub console_forward_max_line_bytes: usize,
    #[serde(default = "default_console_keep_bytes")]
    pub console_keep_bytes: u64,
    #[serde(default = "default_console_min_punch_bytes")]
    pub console_min_punch_bytes: u64,

    /// The most processes (threads count) the user of a rootless Cloud Hypervisor may have (`RLIMIT_NPROC`).
    #[serde(default = "default_ch_max_processes")]
    pub ch_max_processes: u64,

    /// With `ch_rootless`: also confine each Cloud Hypervisor (default on). It then runs in a mount namespace whose root holds only what it
    /// needs, in PID, network (empty), IPC and UTS namespaces of its own, in a cgroup of its own with limits, and with a group of its own
    /// (see `confine.rs`). Off: only a user of its own, as before.
    #[serde(default = "default_true")]
    pub ch_confine: bool,

    /// The cgroup (v2) directory under which each confined VM gets a cgroup of its own. The daemon makes it, and turns on the memory and
    /// pids controllers for it where it can.
    #[serde(default = "default_ch_cgroup_root")]
    pub ch_cgroup_root: String,

    /// Refuse to start (or to start a VM) when the cgroup cannot be made or limited; off, the VM runs without cgroup limits and the daemon
    /// says so once.
    #[serde(default)]
    pub ch_require_cgroup: bool,

    /// What a VM's cgroup allows beyond the guest's memory (Cloud Hypervisor's own, the page cache of the snapshot it reads), in MiB.
    #[serde(default = "default_ch_memory_overhead_mib")]
    pub ch_memory_overhead_mib: u64,

    /// Limit a VM's CPU time to its vCPUs plus one (`cpu.max`). Off by default: a pod's own limits already size the VM.
    #[serde(default)]
    pub ch_cpu_limit: bool,

    /// Paths made visible, read-only, in the confined root of every Cloud Hypervisor, besides what it is known to need (its binary and,
    /// if that is dynamically linked, the system libraries; the kernel, the base rootfs, the images, its own directories, /dev/kvm).
    #[serde(default)]
    pub ch_extra_ro_paths: Vec<String>,

    /// Where the disks of emptyDirs are made: one sparse file per emptyDir, in a directory of each VM, on the host's disk (not `/run`,
    /// which is memory). Empty turns disk-backed emptyDirs off (a request for one is refused). The directory is root's, 0711.
    #[serde(default = "default_emptydir_dir")]
    pub emptydir_dir: String,

    /// The largest disk (MiB) a request may ask for, and the free space (MiB) the host's disk keeps beyond a quarter of a new disk's
    /// size: a disk is refused when the free space is below `size / 4 + emptydir_reserve_mib`.
    #[serde(default = "default_emptydir_max_mib")]
    pub emptydir_max_mib: u64,
    #[serde(default = "default_emptydir_reserve_mib")]
    pub emptydir_reserve_mib: u64,

    /// `mke2fs`, which makes the file system of an emptyDir's disk (on the host, on the file; the host never mounts it).
    #[serde(default = "default_mke2fs_path")]
    pub mke2fs_path: String,
}

fn default_emptydir_dir() -> String {
    "/var/lib/cloudhv/emptydir".to_string()
}
fn default_emptydir_max_mib() -> u64 {
    16_384
}
fn default_emptydir_reserve_mib() -> u64 {
    2048
}
fn default_mke2fs_path() -> String {
    "/usr/sbin/mke2fs".to_string()
}
fn default_true() -> bool {
    true
}
fn default_ch_cgroup_root() -> String {
    "/sys/fs/cgroup/cloudhv".to_string()
}
fn default_ch_memory_overhead_mib() -> u64 {
    256
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
fn default_console_forward_interval_ms() -> u64 {
    1000
}
fn default_console_forward_max_lines_per_sec() -> u32 {
    200
}
fn default_console_forward_max_line_bytes() -> usize {
    2048
}
fn default_console_keep_bytes() -> u64 {
    64 * 1024
}
fn default_console_min_punch_bytes() -> u64 {
    256 * 1024
}
fn default_max_request_bytes() -> usize {
    8 * 1024 * 1024
}
fn default_max_connections() -> usize {
    // Each running container holds a connection of its shim for as long as it runs (Wait blocks), and its log pump and any exec use
    // more: with up to `max_active_vms` VMs of several containers each, tens of connections would stop pods from starting. Only root can
    // connect (the socket is 0600 and the peer is checked), so the limit only guards against a runaway shim.
    8192
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
    // 320: 256 MiB for the workload, and the guest kernel, the agent and its reserved memory take the rest (measured: a 288 MiB VM has
    // 240 MiB available).
    320
}
fn default_ch_binary() -> String {
    "/usr/local/bin/cloud-hypervisor".to_string()
}
fn default_kernel_args() -> String {
    "console=hvc0 root=/dev/vda rw init=/init net.ifnames=0".to_string()
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

    /// Whether the console of a VM is written to a file (see `serial_console`).
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
