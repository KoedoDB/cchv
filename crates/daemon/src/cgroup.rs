//! A cgroup (v2) for each confined Cloud Hypervisor.
//!
//! The VM's memory is what the guest has, plus Cloud Hypervisor's own: a guest that makes its Cloud Hypervisor use more must not be
//! able to take the host's memory, and a fork bomb in a compromised Cloud Hypervisor must stop at a limit of its own. The cgroup also
//! is the one sure way to end a VM: `cgroup.kill` ends every process in it, however many it made and in whatever namespace (the
//! scan for the processes of the VM's user, in `rootless.rs`, is the second way, and what is used when there is no cgroup).
//!
//! The cgroup of VM `x` is `<ch_cgroup_root>/x`. The launcher moves itself into it before it starts anything (`confine.rs`), so
//! everything it starts is in it. Where cgroups cannot be used (no cgroup v2, no write access, controllers that cannot be turned on),
//! the daemon says so once and goes on without, unless `ch_require_cgroup` is set.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use log::{info, warn};

use crate::config::DaemonConfig;

/// The limits of one VM's cgroup, in the units of the files.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Limits {
    pub memory_max_bytes: u64,
    pub pids_max: u64,
    /// `cpu.max` as "<quota> <period>", None for no limit.
    pub cpu_max: Option<String>,
}

/// What a VM of `memory_mb` MiB and `vcpus` vCPUs gets: the guest's memory plus `overhead_mib`, `max_processes` processes and threads,
/// and (if asked) the CPU time of its vCPUs plus one thread for the rest of Cloud Hypervisor.
pub fn limits_for(memory_mb: u64, vcpus: u32, overhead_mib: u64, max_processes: u64, cpu_limit: bool) -> Limits {
    Limits {
        memory_max_bytes: memory_mb.saturating_add(overhead_mib).saturating_mul(1024 * 1024),
        pids_max: max_processes,
        cpu_max: cpu_limit.then(|| format!("{} 100000", (vcpus as u64 + 1) * 100_000)),
    }
}

struct Root {
    path: PathBuf,
}

/// Some(root) once the root is usable, None when it is not (and this has been said).
static ROOT: OnceLock<Option<Root>> = OnceLock::new();

const MOUNT: &str = "/sys/fs/cgroup";

/// Prepare the root at start (only with `ch_rootless` and `ch_confine`). Fails only if `ch_require_cgroup` is set and it cannot work.
pub fn init(config: &DaemonConfig) -> Result<()> {
    if !(config.ch_rootless && config.ch_confine) {
        let _ = ROOT.set(None);
        return Ok(());
    }
    let result = prepare_root(Path::new(&config.ch_cgroup_root));
    match result {
        Ok(path) => {
            info!("confined Cloud Hypervisors get a cgroup each under {}", path.display());
            let _ = ROOT.set(Some(Root { path }));
            Ok(())
        }
        Err(e) if config.ch_require_cgroup => Err(e.context("ch_require_cgroup is set and the cgroup root cannot be used")),
        Err(e) => {
            warn!("no cgroup limits for the Cloud Hypervisors: {e:#}; they run without (set ch_require_cgroup to refuse that)");
            let _ = ROOT.set(None);
            Ok(())
        }
    }
}

/// The directories from the cgroup mount down to (not including) `root`, top first. Empty if `root` is not strictly below the mount or
/// has a `.`/`..` in it.
fn ancestors_below_mount(root: &Path) -> Vec<PathBuf> {
    use std::path::Component;
    if root.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) || !root.starts_with(MOUNT) || root == Path::new(MOUNT) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut d = root.parent();
    while let Some(p) = d {
        if !p.starts_with(MOUNT) {
            break;
        }
        out.push(p.to_path_buf());
        d = p.parent();
    }
    out.reverse();
    out
}

fn prepare_root(root: &Path) -> Result<PathBuf> {
    let parents = ancestors_below_mount(root);
    if parents.is_empty() {
        anyhow::bail!("{} is not a directory below {MOUNT}", root.display());
    }
    if !Path::new(MOUNT).join("cgroup.controllers").exists() {
        anyhow::bail!("{MOUNT} is not a cgroup v2 file system");
    }
    std::fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
    // The controllers the limits need have to be on in every parent and in the root itself, one at a time (the others are none of our
    // business, and one that cannot be turned on must not stop the rest). A parent with processes of its own may refuse (a cgroup v2
    // rule); then the files below do not exist, which is checked next.
    for d in parents.iter().chain(std::iter::once(&root.to_path_buf())) {
        for c in ["memory", "pids", "cpu"] {
            let _ = std::fs::write(d.join("cgroup.subtree_control"), format!("+{c}"));
        }
    }
    // A child made now shows which controllers it has.
    let probe = root.join(".probe");
    std::fs::create_dir_all(&probe)?;
    let have = |f: &str| probe.join(f).exists();
    let ok = have("memory.max") && have("pids.max");
    let _ = std::fs::remove_dir(&probe);
    if !ok {
        anyhow::bail!(
            "the memory and pids controllers are not on in {} (a parent cgroup with processes of its own cannot turn them on; \
             give the daemon a delegated cgroup)",
            root.display()
        );
    }
    Ok(root.to_path_buf())
}

fn root() -> Option<&'static Path> {
    ROOT.get().and_then(|r| r.as_ref()).map(|r| r.path.as_path())
}

/// The cgroup directory of the VM of this name (it need not exist).
pub fn path_of(vm_name: &str) -> Option<PathBuf> {
    root().map(|r| r.join(vm_name))
}

/// Make the cgroup of a VM and set its limits. None when cgroups are not in use.
pub fn create(vm_name: &str, limits: &Limits) -> Result<Option<PathBuf>> {
    let Some(dir) = path_of(vm_name) else { return Ok(None) };
    std::fs::create_dir(&dir).or_else(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            Ok(())
        } else {
            Err(e)
        }
    })
    .with_context(|| format!("create {}", dir.display()))?;
    let set = |file: &str, value: &str| -> Result<()> {
        std::fs::write(dir.join(file), value).with_context(|| format!("write {value:?} to {}", dir.join(file).display()))
    };
    set("memory.max", &limits.memory_max_bytes.to_string())?;
    // The guest's memory is not to go to the host's swap.
    let _ = std::fs::write(dir.join("memory.swap.max"), "0");
    set("pids.max", &limits.pids_max.to_string())?;
    if let Some(cpu) = &limits.cpu_max {
        set("cpu.max", cpu)?;
    }
    Ok(Some(dir))
}

/// Kill everything in the cgroup of the VM (if there is one) and remove it. Returns once the cgroup is empty and gone, or after a few
/// seconds of trying.
pub async fn kill_and_remove(vm_name: &str) {
    let Some(dir) = path_of(vm_name) else { return };
    if !dir.exists() {
        return;
    }
    for _ in 0..100 {
        kill_all(&dir);
        if populated(&dir) == Some(false) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    for _ in 0..50 {
        match std::fs::remove_dir(&dir) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    }
    warn!("the cgroup {} could not be removed", dir.display());
}

/// SIGKILL everything in the cgroup: `cgroup.kill` (Linux 5.14), or each process in `cgroup.procs`.
pub fn kill_all(dir: &Path) {
    if std::fs::write(dir.join("cgroup.kill"), "1").is_ok() {
        return;
    }
    if let Ok(procs) = std::fs::read_to_string(dir.join("cgroup.procs")) {
        for pid in procs.lines().filter_map(|l| l.trim().parse::<u32>().ok()) {
            if crate::ctl::valid_pid(pid) {
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            }
        }
    }
}

/// Whether the cgroup has processes (`cgroup.events`), None if that cannot be read.
fn populated(dir: &Path) -> Option<bool> {
    let events = std::fs::read_to_string(dir.join("cgroup.events")).ok()?;
    parse_populated(&events)
}

pub fn parse_populated(events: &str) -> Option<bool> {
    events
        .lines()
        .find_map(|l| l.strip_prefix("populated "))
        .map(|v| v.trim() == "1")
}

/// At start: the cgroups of VMs of an earlier daemon go, with whatever is left in them.
pub async fn sweep() {
    let Some(root) = root() else { return };
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if e.path().is_dir() && !name.starts_with('.') {
            kill_and_remove(&name).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limits_are_the_guest_plus_the_overhead() {
        let l = limits_for(512, 2, 256, 1024, false);
        assert_eq!(l.memory_max_bytes, 768 * 1024 * 1024);
        assert_eq!(l.pids_max, 1024);
        assert_eq!(l.cpu_max, None);
        let l = limits_for(128, 1, 256, 1024, true);
        assert_eq!(l.cpu_max.as_deref(), Some("200000 100000"), "one vCPU plus one thread");
        // absurd sizes do not wrap around
        assert_eq!(limits_for(u64::MAX, 1, 256, 1, false).memory_max_bytes, u64::MAX);
    }

    #[test]
    fn the_events_file_tells_whether_the_cgroup_has_processes() {
        assert_eq!(parse_populated("populated 1\nfrozen 0\n"), Some(true));
        assert_eq!(parse_populated("populated 0\nfrozen 0\n"), Some(false));
        assert_eq!(parse_populated("frozen 0\n"), None);
    }

    #[test]
    fn a_root_outside_the_cgroup_file_system_is_refused() {
        assert!(ancestors_below_mount(Path::new("/tmp/cloudhv")).is_empty());
        assert!(ancestors_below_mount(Path::new("/sys/fs/cgroup")).is_empty(), "the mount itself is not a root");
        assert!(ancestors_below_mount(Path::new("/sys/fs/cgroup/../../tmp")).is_empty(), "no ..");
        assert!(prepare_root(Path::new("/tmp/cloudhv")).is_err());
        assert_eq!(
            ancestors_below_mount(Path::new("/sys/fs/cgroup/a/b/cloudhv")),
            vec![
                PathBuf::from("/sys/fs/cgroup"),
                PathBuf::from("/sys/fs/cgroup/a"),
                PathBuf::from("/sys/fs/cgroup/a/b"),
            ]
        );
    }
}
