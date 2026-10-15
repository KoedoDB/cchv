//! What the daemon accepts, checked before anything is created or run.
//!
//! The daemon is root, and its API is the one door through which the shim (and anything that can reach the socket) asks it to make
//! directories, change owners, remove trees, enter network namespaces and attach files as disks. Every string of a request that
//! becomes a path, a name or a size goes through here first. Pure functions, so that they can be tested without a host.

use std::path::{Component, Path};

use anyhow::{Context, Result};

use crate::config::DaemonConfig;
use crate::fsutil;

/// An identifier (VM, container, image key) that is safe to use as a file name, a command-line word and a log entry:
/// `[A-Za-z0-9_-]{1,64}`. No `/`, no `.`, so no `..`, no absolute path, no NUL, no newline, no multi-byte character.
pub fn is_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub fn check_id(what: &str, s: &str) -> Result<()> {
    if is_id(s) {
        Ok(())
    } else {
        anyhow::bail!("{what} is not a valid identifier (1 to 64 of A-Z a-z 0-9 _ -)")
    }
}

/// A requested size: within the limits of the configuration, and one whose byte count fits. 0 means "the default".
pub fn check_size(config: &DaemonConfig, memory_mb: u64, vcpus: u64) -> Result<()> {
    if memory_mb > config.max_memory_mb {
        anyhow::bail!("a VM of {memory_mb} MiB is more than max_memory_mb ({})", config.max_memory_mb);
    }
    if memory_mb.checked_mul(1024 * 1024).is_none() {
        anyhow::bail!("{memory_mb} MiB does not fit");
    }
    if vcpus > config.max_vcpus as u64 {
        anyhow::bail!("{vcpus} vCPUs is more than max_vcpus ({})", config.max_vcpus);
    }
    Ok(())
}

/// Whether `path` names a network namespace the daemon may enter: `/var/run/netns/<name>`, `/run/netns/<name>` (where the runtime
/// puts a pod's) or `/proc/<pid>/ns/net`. Lexical only; `netns::open_checked` looks at what the path really is.
pub fn netns_path_allowed(path: &str) -> bool {
    if path.is_empty() || path.contains('\0') || path.contains('\n') {
        return false;
    }
    let p = Path::new(path);
    if !p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return false;
    }
    for root in ["/var/run/netns", "/run/netns"] {
        if let Ok(rest) = p.strip_prefix(root) {
            let mut c = rest.components();
            return matches!((c.next(), c.next()), (Some(Component::Normal(_)), None));
        }
    }
    if let Ok(rest) = p.strip_prefix("/proc") {
        let parts: Vec<_> = rest.components().collect();
        return parts.len() == 3
            && matches!(parts[0], Component::Normal(n) if n.to_str().map(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).unwrap_or(false))
            && matches!(parts[1], Component::Normal(n) if n == "ns")
            && matches!(parts[2], Component::Normal(n) if n == "net");
    }
    false
}

/// Whether `path` is an image the daemon may attach: inside `root`, a plain `<name>.erofs` file name, no `.`/`..`, no newline.
pub fn erofs_path_allowed(path: &str, root: &str) -> bool {
    if path.contains('\0') || path.contains('\n') {
        return false;
    }
    let p = Path::new(path);
    if !p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return false;
    }
    match p.strip_prefix(root) {
        Ok(rest) => {
            let mut c = rest.components();
            match (c.next(), c.next()) {
                (Some(Component::Normal(name)), None) => name
                    .to_str()
                    .map(|n| n.len() > ".erofs".len() && n.ends_with(".erofs"))
                    .unwrap_or(false),
                _ => false,
            }
        }
        Err(_) => false,
    }
}

/// An image path from a request: lexically inside the images' directory, and (opened without following a link) a regular file.
pub fn check_erofs_path(config: &DaemonConfig, path: &str) -> Result<()> {
    if !erofs_path_allowed(path, &config.erofs_cache_dir) {
        anyhow::bail!("erofs_path is not an image inside {}", config.erofs_cache_dir);
    }
    let fd = fsutil::open_nosymlinks(Path::new(path), libc::O_RDONLY | libc::O_NONBLOCK)
        .with_context(|| format!("erofs_path {path}"))?;
    use std::os::unix::io::AsRawFd;
    if !fsutil::is_reg(&fsutil::fstat(fd.as_raw_fd())?) {
        anyhow::bail!("erofs_path is not a regular file");
    }
    Ok(())
}

/// A value that stands for "none" in a Cloud Hypervisor configuration: absent, null, false, an empty list or object.
fn unset(v: Option<&serde_json::Value>) -> bool {
    match v {
        None | Some(serde_json::Value::Null) | Some(serde_json::Value::Bool(false)) => true,
        Some(serde_json::Value::Array(a)) => a.is_empty(),
        Some(serde_json::Value::Object(o)) => o.is_empty(),
        _ => false,
    }
}

/// The keys of a Cloud Hypervisor VM configuration the daemon knows. A snapshot's `config.json` is written by the Cloud Hypervisor
/// that took it, and a guest that escaped can have changed it; a key not listed means a Cloud Hypervisor newer than this check, and
/// the snapshot is not used (the pod gets a VM from the pool).
const KNOWN_KEYS: &[&str] = &[
    "cpus", "memory", "payload", "rate_limit_groups", "disks", "net", "rng", "balloon", "generic_vhost_user", "fs", "pmem", "serial",
    "console", "debug_console", "devices", "user_devices", "vdpa", "vsock", "pvmemcontrol", "pvpanic", "iommu", "numa", "watchdog",
    "rtc", "gdb", "pci_segments", "platform", "tpm", "preserved_fds", "landlock_enable", "landlock_rules", "ivshmem",
];

/// Keys that must be empty: each gives a guest access to something of the host (files shared in, host devices, pass-through).
const MUST_BE_UNSET: &[&str] = &[
    "fs", "pmem", "devices", "user_devices", "vdpa", "generic_vhost_user", "tpm", "numa", "iommu", "preserved_fds", "landlock_rules",
    "net", "gdb", "ivshmem", "pvmemcontrol",
];

/// Check the configuration of a snapshot before a Cloud Hypervisor is told to restore it. The disks may only be the platform's rootfs
/// and the image of the pod (`extra_disk`); no device that reaches the host; the kernel is the daemon's own; the console is off or a
/// file (whose path the daemon sets). Anything else is refused.
pub fn validate_snapshot_config(
    cfg: &serde_json::Value,
    kernel: &str,
    rootfs: &str,
    extra_disk: Option<&str>,
) -> Result<()> {
    let obj = cfg.as_object().context("the configuration is not an object")?;
    for k in obj.keys() {
        if !KNOWN_KEYS.contains(&k.as_str()) {
            anyhow::bail!("unknown key {k:?}");
        }
    }
    for k in MUST_BE_UNSET {
        if !unset(obj.get(*k)) {
            anyhow::bail!("{k} is set");
        }
    }

    // payload: our kernel and nothing else to boot from
    let payload = obj.get("payload").and_then(|p| p.as_object()).context("no payload")?;
    if payload.get("kernel").and_then(|k| k.as_str()) != Some(kernel) {
        anyhow::bail!("the kernel is not the daemon's");
    }
    for k in ["initramfs", "firmware", "igvm", "host_data", "fw_cfg_config"] {
        if !unset(payload.get(k)) {
            anyhow::bail!("payload.{k} is set");
        }
    }

    // memory: guest RAM, not a file of the host
    if let Some(m) = obj.get("memory").and_then(|m| m.as_object()) {
        // (`file` is there for a Cloud Hypervisor that has it)
        for k in ["file", "zones"] {
            if !unset(m.get(k)) {
                anyhow::bail!("memory.{k} is set");
            }
        }
    }

    // disks: the rootfs of the platform and the pod's image, read-only
    for d in obj.get("disks").and_then(|d| d.as_array()).into_iter().flatten() {
        let path = d.get("path").and_then(|p| p.as_str()).unwrap_or("");
        if path != rootfs && Some(path) != extra_disk {
            anyhow::bail!("disk {path:?} is not one the VM may have");
        }
        if d.get("readonly").and_then(|r| r.as_bool()) != Some(true) {
            anyhow::bail!("disk {path:?} is not read-only");
        }
        if !unset(d.get("vhost_user")) || !unset(d.get("vhost_socket")) {
            anyhow::bail!("disk {path:?} is a vhost-user disk");
        }
        // Only raw images: a qcow2 file can name backing files, which are read from the host.
        if !unset(d.get("backing_files")) {
            anyhow::bail!("disk {path:?} may use backing files");
        }
    }

    // consoles
    for (key, modes) in [("serial", &["Off", "Null", "File"][..]), ("console", &["Off", "Null"][..])] {
        if let Some(c) = obj.get(key).filter(|c| !c.is_null()) {
            let mode = c.get("mode").and_then(|m| m.as_str()).unwrap_or("Off");
            if !modes.contains(&mode) {
                anyhow::bail!("{key}.mode {mode:?} is not allowed");
            }
        }
    }
    if let Some(c) = obj.get("debug_console").filter(|c| !c.is_null()) {
        if c.get("mode").and_then(|m| m.as_str()).unwrap_or("Off") != "Off" {
            anyhow::bail!("debug_console is on");
        }
    }

    // rng: the host's urandom, not another file
    if let Some(r) = obj.get("rng").and_then(|r| r.as_object()) {
        if let Some(src) = r.get("src").and_then(|s| s.as_str()) {
            if src != "/dev/urandom" {
                anyhow::bail!("rng.src {src:?} is not /dev/urandom");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> DaemonConfig {
        serde_json::from_str(r#"{"kernel_path":"/k","rootfs_path":"/r"}"#).unwrap()
    }

    #[test]
    fn identifiers_are_plain_words() {
        for ok in ["a", "pool-1-4", "warm-0123abcd-7", "shadow_ctr", &"a".repeat(64)] {
            assert!(is_id(ok), "{ok}");
        }
        for bad in ["", "..", "../x", "/var", "a/b", "a.b", "a b", "a\nb", "a\0b", "é", "コンテナ", &"a".repeat(65), "-"] {
            // "-" is a legal character; only the others are tested as bad
            if bad == "-" {
                assert!(is_id(bad));
                continue;
            }
            assert!(!is_id(bad), "{bad:?}");
        }
        assert!(check_id("vm_id", "../..").is_err());
    }

    #[test]
    fn sizes_above_the_limits_are_refused() {
        let c = config();
        assert!(check_size(&c, 0, 0).is_ok());
        assert!(check_size(&c, 4096, 4).is_ok());
        assert!(check_size(&c, c.max_memory_mb, c.max_vcpus as u64).is_ok());
        assert!(check_size(&c, c.max_memory_mb + 1, 1).is_err());
        assert!(check_size(&c, 1, c.max_vcpus as u64 + 1).is_err());
        assert!(check_size(&c, 1, u64::MAX).is_err(), "no truncation to u32");
        assert!(check_size(&c, u64::MAX, 1).is_err());
        let mut big = config();
        big.max_memory_mb = u64::MAX;
        assert!(check_size(&big, u64::MAX, 1).is_err(), "the byte count overflows");
    }

    #[test]
    fn a_netns_is_one_of_the_runtimes_or_a_process() {
        for ok in ["/var/run/netns/cni-1234-abcd", "/run/netns/x", "/proc/4242/ns/net"] {
            assert!(netns_path_allowed(ok), "{ok}");
        }
        for bad in [
            "",
            "/proc/1/ns/mnt",
            "/proc/self/ns/net",
            "/proc/1/../1/ns/net",
            "/var/run/netns/",
            "/var/run/netns",
            "/var/run/netns/a/b",
            "/var/run/netns/../../etc/shadow",
            "/etc/shadow",
            "relative/netns",
            "/var/run/netns/a\nb",
            "/proc/12x/ns/net",
            "/proc//ns/net",
            "/run/netns2/x",
        ] {
            assert!(!netns_path_allowed(bad), "{bad:?}");
        }
    }

    #[test]
    fn an_image_is_a_plain_erofs_file_in_the_cache() {
        let root = "/run/cloudhv/erofs-cache";
        assert!(erofs_path_allowed("/run/cloudhv/erofs-cache/0123abcd.erofs", root));
        for bad in [
            "/run/cloudhv/erofs-cache/0123abcd",
            "/run/cloudhv/erofs-cache/.erofs",
            "/run/cloudhv/erofs-cache/../../../etc/shadow.erofs",
            "/run/cloudhv/erofs-cache/sub/x.erofs",
            "/run/cloudhv/erofs-cache2/x.erofs",
            "/etc/shadow",
            "/run/cloudhv/erofs-cache/a\nb.erofs",
            "x.erofs",
            "",
        ] {
            assert!(!erofs_path_allowed(bad, root), "{bad:?}");
        }
    }

    fn good_config() -> serde_json::Value {
        json!({
            "cpus": {"boot_vcpus": 1, "max_vcpus": 1},
            "memory": {"size": 134217728, "shared": true, "zones": null},
            "payload": {"kernel": "/k", "cmdline": "console=ttyS0", "initramfs": null, "firmware": null},
            "disks": [
                {"path": "/r", "readonly": true, "id": "_disk0"},
                {"path": "/cache/x.erofs", "readonly": true, "id": "ctr-rootfs", "vhost_user": false, "vhost_socket": null}
            ],
            "net": [], "fs": null, "pmem": null, "devices": null, "vdpa": null, "iommu": false, "numa": null,
            "serial": {"mode": "File", "file": "/x/console.log"},
            "console": {"mode": "Off"},
            "debug_console": {"mode": "Off"},
            "rng": {"src": "/dev/urandom"},
            "vsock": {"cid": 3, "socket": "/x/vsock.sock"},
            "balloon": null, "watchdog": false
        })
    }

    #[test]
    fn the_configuration_of_a_good_snapshot_passes() {
        assert!(validate_snapshot_config(&good_config(), "/k", "/r", Some("/cache/x.erofs")).is_ok());
        assert!(validate_snapshot_config(&good_config(), "/k", "/r", Some("/cache/y.erofs")).is_err(), "another image");
    }

    #[test]
    fn a_snapshot_that_reaches_for_the_host_is_refused() {
        let cases: Vec<(&str, Box<dyn Fn(&mut serde_json::Value)>)> = vec![
            ("a shared directory", Box::new(|c| c["fs"] = json!([{"tag": "h", "socket": "/x"}]))),
            ("a host device", Box::new(|c| c["devices"] = json!([{"path": "/sys/bus/pci/devices/0000:01:00.0"}]))),
            ("user devices", Box::new(|c| c["user_devices"] = json!([{"socket": "/x"}]))),
            ("pmem", Box::new(|c| c["pmem"] = json!([{"file": "/etc/shadow"}]))),
            ("a network device", Box::new(|c| c["net"] = json!([{"tap": "tap0"}]))),
            ("another disk", Box::new(|c| c["disks"][1]["path"] = json!("/etc/shadow"))),
            ("a writable disk", Box::new(|c| c["disks"][1]["readonly"] = json!(false))),
            ("a vhost-user disk", Box::new(|c| c["disks"][1]["vhost_user"] = json!(true))),
            ("another kernel", Box::new(|c| c["payload"]["kernel"] = json!("/boot/vmlinuz"))),
            ("an initramfs", Box::new(|c| c["payload"]["initramfs"] = json!("/x"))),
            ("memory zones", Box::new(|c| c["memory"]["zones"] = json!([{"id": "z", "file": "/dev/mem"}]))),
            ("a serial socket", Box::new(|c| c["serial"] = json!({"mode": "Socket", "socket": "/x"}))),
            ("a tty console", Box::new(|c| c["console"] = json!({"mode": "Tty"}))),
            ("a debug console", Box::new(|c| c["debug_console"] = json!({"mode": "File", "file": "/x"}))),
            ("another entropy source", Box::new(|c| c["rng"]["src"] = json!("/dev/zero"))),
            ("an unknown key", Box::new(|c| c["something_new"] = json!(1))),
            ("preserved fds", Box::new(|c| c["preserved_fds"] = json!([3]))),
            ("a gdb stub", Box::new(|c| c["gdb"] = json!(true))),
            ("shared memory with the host", Box::new(|c| c["ivshmem"] = json!({"path": "/dev/shm/x", "size": 1}))),
            ("fw_cfg files", Box::new(|c| c["payload"]["fw_cfg_config"] = json!({"items": [{"name": "x", "file": "/etc/shadow"}]}))),
            ("backing files", Box::new(|c| c["disks"][1]["backing_files"] = json!(true))),
        ];
        for (what, mutate) in cases {
            let mut c = good_config();
            mutate(&mut c);
            assert!(
                validate_snapshot_config(&c, "/k", "/r", Some("/cache/x.erofs")).is_err(),
                "{what} must be refused"
            );
        }
        assert!(validate_snapshot_config(&json!([]), "/k", "/r", None).is_err());
    }
}
