use std::path::Path;

use anyhow::{Context, Result};
use cloudhv_common::types::RuntimeConfig;
use log::debug;
use serde::Deserialize;

/// Default configuration file path.
const DEFAULT_CONFIG_PATH: &str = "/opt/cloudhv/config.json";

/// Load runtime configuration from the default path or a specified path.
pub fn load_config(path: Option<&str>) -> Result<RuntimeConfig> {
    let config_path = path.unwrap_or(DEFAULT_CONFIG_PATH);
    let p = Path::new(config_path);

    if !p.exists() {
        anyhow::bail!(
            "runtime config not found at {config_path}; \
             create it or specify an alternative path"
        );
    }

    let data = std::fs::read_to_string(p)
        .with_context(|| format!("failed to read config from {config_path}"))?;

    let config: RuntimeConfig = serde_json::from_str(&data)
        .with_context(|| format!("failed to parse config from {config_path}"))?;

    debug!("loaded runtime config: {:?}", config);
    Ok(config)
}

/// What the shim itself limits, from the same file (`/opt/cloudhv/config.json`). Every field is optional; a file without them (or
/// without the file) gives the defaults. Kept apart from `RuntimeConfig`, which the daemon reads too.
#[derive(Debug, Clone, Deserialize)]
pub struct ShimLimits {
    /// The most memory (MiB) a pod's VM may have (its memory limit plus the guest's own needs). A pod that asks for more does not
    /// start: its limit comes from the pod itself, and the scheduler counts only requests.
    #[serde(default = "default_max_memory_mb")]
    pub max_memory_mb: u64,

    /// The most vCPUs a pod's VM may have. 0 (default): the host's CPUs. A pod with a higher CPU limit gets this many (its CPU time
    /// is limited by its cgroup quota anyway).
    #[serde(default)]
    pub max_vcpus: u32,

    /// The size of the cache of erofs images (MiB). Images nobody is using are evicted, oldest first, to make room.
    #[serde(default = "default_erofs_cache_max_mib")]
    pub erofs_cache_max_mib: u64,

    /// Where the images are kept (an absolute path): the daemon's `erofs_cache_dir`, which must be the same.
    #[serde(default = "default_erofs_cache_dir")]
    pub erofs_cache_dir: String,

    /// Where `mkfs.erofs` is (an absolute path). Default: `/usr/local/bin/mkfs.erofs`, then `/usr/sbin/mkfs.erofs`.
    #[serde(default)]
    pub mkfs_erofs_path: Option<String>,

    /// Kill `mkfs.erofs` after this many seconds.
    #[serde(default = "default_mkfs_timeout_secs")]
    pub mkfs_erofs_timeout_secs: u64,

    /// Directories, besides the kubelet's and containerd's own, that persistent volumes are made in. A bind mount from anywhere else
    /// is refused. Default: the local-path provisioners of OpenEBS and k3s.
    #[serde(default = "crate::volumes::default_pv_roots")]
    pub volume_source_roots: Vec<String>,

    /// The size (MiB) of the disk an emptyDir gets when it is not `medium: Memory` (the kubelet does not tell the runtime the volume's
    /// `sizeLimit`), and the most one may be (the daemon's `emptydir_max_mib` refuses more). 0 turns disk-backed emptyDirs off: they are
    /// then in the guest's memory, as `medium: Memory` ones are.
    #[serde(default = "default_emptydir_default_mib")]
    pub emptydir_default_mib: u64,
    #[serde(default = "default_emptydir_max_mib")]
    pub emptydir_max_mib: u64,

    /// Seconds a request to the daemon may take (not WaitContainer, which has no limit).
    #[serde(default = "default_rpc_timeout_secs")]
    pub rpc_timeout_secs: u64,

    /// Seconds AcquireSandbox and AddContainer may take (a VM of its own size is booted for them).
    #[serde(default = "default_acquire_timeout_secs")]
    pub acquire_timeout_secs: u64,

    /// A pod's Cloud Hypervisor must be placed in the pod's cgroup, or the pod does not start (without it the VM runs outside the
    /// pod's limits). Turn it off only where the cgroup tree of the pod is not visible to the shim (a test cluster in a container).
    #[serde(default = "default_true")]
    pub require_pod_cgroup: bool,
}

fn default_emptydir_default_mib() -> u64 {
    1024
}
fn default_emptydir_max_mib() -> u64 {
    16 * 1024
}
fn default_max_memory_mb() -> u64 {
    16 * 1024
}
fn default_erofs_cache_max_mib() -> u64 {
    4096
}
fn default_erofs_cache_dir() -> String {
    cloudhv_common::DEFAULT_EROFS_CACHE_DIR.to_string()
}
fn default_mkfs_timeout_secs() -> u64 {
    300
}
fn default_rpc_timeout_secs() -> u64 {
    30
}
fn default_acquire_timeout_secs() -> u64 {
    120
}
fn default_true() -> bool {
    true
}

impl Default for ShimLimits {
    fn default() -> Self {
        serde_json::from_str("{}").expect("every field has a default")
    }
}

impl ShimLimits {
    /// The places `mkfs.erofs` may be.
    pub fn mkfs_candidates(&self) -> Vec<std::path::PathBuf> {
        match &self.mkfs_erofs_path {
            Some(p) => vec![p.into()],
            None => vec![
                "/usr/local/bin/mkfs.erofs".into(),
                "/usr/sbin/mkfs.erofs".into(),
            ],
        }
    }
}

/// The shim's limits from the configuration file (the defaults for a file that does not set them).
pub fn load_shim_limits(path: Option<&str>) -> Result<ShimLimits> {
    let config_path = path.unwrap_or(DEFAULT_CONFIG_PATH);
    let data = std::fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config from {config_path}"))?;
    let limits: ShimLimits = serde_json::from_str(&data)
        .with_context(|| format!("failed to parse the shim's limits in {config_path}"))?;
    anyhow::ensure!(
        Path::new(&limits.erofs_cache_dir).is_absolute(),
        "erofs_cache_dir must be an absolute path, not {:?}",
        limits.erofs_cache_dir
    );
    Ok(limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_load_valid_config() {
        let config_json = r#"{
            "kernel_path": "/opt/cloudhv/vmlinux",
            "rootfs_path": "/opt/cloudhv/rootfs.ext4"
        }"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(config_json.as_bytes()).unwrap();

        let config = load_config(Some(f.path().to_str().unwrap())).unwrap();
        assert_eq!(config.kernel_path, "/opt/cloudhv/vmlinux");
        assert_eq!(config.rootfs_path, "/opt/cloudhv/rootfs.ext4");
        assert_eq!(config.default_vcpus, cloudhv_common::DEFAULT_VCPUS);
        assert_eq!(config.default_memory_mb, cloudhv_common::DEFAULT_MEMORY_MB);
    }

    #[test]
    fn the_shims_limits_have_defaults_and_can_be_set() {
        let none = ShimLimits::default();
        assert_eq!(none.max_memory_mb, 16384);
        assert_eq!(none.max_vcpus, 0);
        assert_eq!(none.erofs_cache_max_mib, 4096);
        assert_eq!(none.erofs_cache_dir, "/var/lib/cloud-hypervisor/erofs-cache");
        assert_eq!(none.mkfs_erofs_timeout_secs, 300);
        assert!(none.require_pod_cgroup);
        assert!(none
            .volume_source_roots
            .contains(&"/var/openebs/local".to_string()));
        assert_eq!(none.mkfs_candidates().len(), 2);

        let mut f = NamedTempFile::new().unwrap();
        f.write_all(
            br#"{"kernel_path": "k", "rootfs_path": "r", "max_memory_mb": 2048, "mkfs_erofs_path": "/opt/mkfs.erofs",
                 "volume_source_roots": ["/data/pv"], "require_pod_cgroup": false}"#,
        )
        .unwrap();
        let l = load_shim_limits(Some(f.path().to_str().unwrap())).unwrap();
        assert_eq!(l.max_memory_mb, 2048);
        assert_eq!(l.volume_source_roots, vec!["/data/pv".to_string()]);
        assert_eq!(
            l.mkfs_candidates(),
            vec![std::path::PathBuf::from("/opt/mkfs.erofs")]
        );
        assert!(!l.require_pod_cgroup);
    }

    #[test]
    fn test_load_missing_config() {
        let result = load_config(Some("/nonexistent/path.json"));
        assert!(result.is_err());
    }
}
