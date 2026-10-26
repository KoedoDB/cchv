pub mod buildinfo;
pub mod error;
pub mod netlink;
pub mod types;

/// The Pod annotation that asks for a delegated cgroup subtree: a comma-separated list of container names (`io.kubernetes.cri.container-name`).
/// The daemon refuses it unless `allow_cgroup_delegate` is on; the guest agent acts on it. See docs/sandbox-daemon.md.
pub const CGROUP_DELEGATE_ANNOTATION: &str = "cloudhv.koedodb.com/cgroup-delegate";

/// Default vsock port for the guest agent ttrpc server.
pub const AGENT_VSOCK_PORT: u32 = 10789;

/// Guest vsock CID starts at this value (host=2, first guest=3).
pub const GUEST_CID_START: u64 = 3;

/// Default timeout (seconds) waiting for the guest agent to become ready.
pub const AGENT_STARTUP_TIMEOUT_SECS: u64 = 10;

/// Default number of boot vCPUs.
pub const DEFAULT_VCPUS: u32 = 1;

/// Default guest memory in MiB.
pub const DEFAULT_MEMORY_MB: u64 = 320;
/// The least memory of the VM of a pod that has a memory limit (see `min_memory_mb`). Kata's default VM is 64 MiB; this is the limit + the guest's 64 MiB for a 64 MiB pod.
pub const MIN_MEMORY_MB: u64 = 128;
/// What the guest needs besides the pod's containers: its kernel, the agent, crun, the page cache of the rootfs. Added to the pod's memory limit (see `guest_overhead_mb`).
pub const GUEST_OVERHEAD_MB: u64 = 64;

/// Runtime state directory on the host.
pub const RUNTIME_STATE_DIR: &str = "/run/cloudhv";

/// Where the shim keeps the erofs images of the containers' root file systems, and the only place the daemon takes an image from
/// (`erofs_cache_dir` in the shim's and in the daemon's configuration; the two must be the same). On the disk, under the state
/// directory of the platform: the cache is bounded by `erofs_cache_max_mib` and survives a restart, and it does not pin that much
/// of the host's memory (a host has no swap, and tmpfs pages are in no pod's cgroup).
pub const DEFAULT_EROFS_CACHE_DIR: &str = "/var/lib/cloud-hypervisor/erofs-cache";

/// Default path to the Cloud Hypervisor binary.
pub const DEFAULT_CH_BINARY: &str = "/usr/local/bin/cloud-hypervisor";

/// Default maximum containers per VM.
pub const DEFAULT_MAX_CONTAINERS_PER_VM: usize = 5;

/// Default hotplug memory size in MiB (0 = no hotplug).
pub const DEFAULT_HOTPLUG_MEMORY_MB: u64 = 0;
