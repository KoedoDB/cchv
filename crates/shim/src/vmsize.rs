//! How big a pod's VM is: from the limits of the pod, which containerd puts on the sandbox's OCI spec as annotations
//! (`io.kubernetes.cri.sandbox-memory` in bytes, `io.kubernetes.cri.sandbox-cpu-quota` and `-period` in microseconds).
//!
//! Until this existed every VM had the daemon's default size (128 MiB, 1 vCPU) whatever the pod asked for: a pod with a limit
//! of 512 MiB and 2 CPUs saw 112 MB and one CPU.

/// What the guest needs besides the pod's containers: its kernel, the agent, crun, the page cache of the rootfs. Added to the pod's
/// memory limit. (An empty guest uses about 40 MiB; the rest is a margin.)
pub const GUEST_OVERHEAD_MB: u64 = 64;

/// The size of a pod's VM. A field of 0 means "the daemon's default".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VmSize {
    pub memory_mb: u64,
    pub vcpus: u32,
}

/// The limits found on the sandbox spec, as containerd wrote them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PodLimits {
    /// Memory limit of the pod in bytes (None or 0: no limit).
    pub memory_bytes: Option<u64>,
    /// CPU quota in microseconds per period (None or 0 or negative: no limit).
    pub cpu_quota: Option<i64>,
    pub cpu_period: Option<u64>,
}

/// The VM for a pod with these limits. `default_memory_mb` and `default_vcpus` are what a pod without limits gets (and the least a pod
/// with small limits gets); `max_vcpus` caps the CPUs (the host's). A pod whose size equals the default returns the default, so the
/// daemon can serve it from its pool.
pub fn vm_size(
    limits: PodLimits,
    default_memory_mb: u64,
    default_vcpus: u32,
    max_vcpus: u32,
) -> VmSize {
    let memory_mb = match limits.memory_bytes {
        Some(bytes) if bytes > 0 => {
            let pod_mb = bytes.div_ceil(1024 * 1024);
            pod_mb
                .saturating_add(GUEST_OVERHEAD_MB)
                .max(default_memory_mb)
        }
        _ => default_memory_mb,
    };
    let vcpus = match (limits.cpu_quota, limits.cpu_period) {
        (Some(quota), Some(period)) if quota > 0 && period > 0 => {
            // 1500m is two CPUs: the quota is a share of time, and the guest has whole CPUs.
            (quota as u64).div_ceil(period) as u32
        }
        _ => default_vcpus,
    };
    let vcpus = vcpus
        .max(1)
        .min(max_vcpus.max(1))
        .max(default_vcpus.min(max_vcpus.max(1)));
    VmSize { memory_mb, vcpus }
}

/// A pod's own limit sizes its VM, and the scheduler counts only a pod's requests, so a pod can ask for a VM of any size (a limit of
/// 4 TiB with a request of 10 MiB). This is the shim's last say: a VM bigger than the host allows is not started.
pub fn check_size(size: VmSize, max_memory_mb: u64) -> Result<(), String> {
    if size.memory_mb > max_memory_mb {
        return Err(format!(
            "the pod's memory limit needs a VM of {} MiB, more than the {} MiB this host allows (max_memory_mb)",
            size.memory_mb, max_memory_mb
        ));
    }
    Ok(())
}

/// Read the limits from the annotations of a sandbox's OCI spec.
pub fn limits_from_spec(spec: &serde_json::Value) -> PodLimits {
    let get = |key: &str| -> Option<&str> {
        spec.pointer(&format!("/annotations/{key}"))
            .and_then(|v| v.as_str())
    };
    PodLimits {
        memory_bytes: get("io.kubernetes.cri.sandbox-memory").and_then(|s| s.parse().ok()),
        cpu_quota: get("io.kubernetes.cri.sandbox-cpu-quota").and_then(|s| s.parse().ok()),
        cpu_period: get("io.kubernetes.cri.sandbox-cpu-period").and_then(|s| s.parse().ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(mem_mib: u64, quota: i64) -> PodLimits {
        PodLimits {
            memory_bytes: Some(mem_mib * 1024 * 1024),
            cpu_quota: Some(quota),
            cpu_period: Some(100_000),
        }
    }

    #[test]
    fn a_pod_with_no_limits_gets_the_default() {
        let s = vm_size(PodLimits::default(), 128, 1, 8);
        assert_eq!(
            s,
            VmSize {
                memory_mb: 128,
                vcpus: 1
            }
        );
        // containerd writes "0" for a limit that is not set.
        let zero = PodLimits {
            memory_bytes: Some(0),
            cpu_quota: Some(0),
            cpu_period: Some(100_000),
        };
        assert_eq!(vm_size(zero, 128, 1, 8), s);
    }

    #[test]
    fn the_vm_holds_the_pods_memory_limit_and_the_guests_own_needs() {
        // The pod asked for 512 MiB and 2 CPUs (the case that saw 112 MB and one CPU).
        let s = vm_size(limits(512, 200_000), 128, 1, 8);
        assert_eq!(
            s,
            VmSize {
                memory_mb: 512 + GUEST_OVERHEAD_MB,
                vcpus: 2
            }
        );
    }

    #[test]
    fn a_small_limit_is_not_made_smaller_than_the_default() {
        assert_eq!(
            vm_size(limits(32, 10_000), 128, 1, 8),
            VmSize {
                memory_mb: 128,
                vcpus: 1
            }
        );
    }

    #[test]
    fn a_part_of_a_cpu_is_rounded_up_to_a_whole_one_and_the_host_caps_them() {
        assert_eq!(vm_size(limits(256, 150_000), 128, 1, 8).vcpus, 2);
        assert_eq!(vm_size(limits(256, 50_000), 128, 1, 8).vcpus, 1);
        assert_eq!(
            vm_size(limits(256, 3_200_000), 128, 1, 8).vcpus,
            8,
            "32 CPUs asked on a host of 8"
        );
    }

    #[test]
    fn a_memory_limit_that_is_not_a_whole_mib_is_rounded_up() {
        let l = PodLimits {
            memory_bytes: Some(300 * 1024 * 1024 + 1),
            cpu_quota: None,
            cpu_period: None,
        };
        assert_eq!(vm_size(l, 128, 1, 8).memory_mb, 301 + GUEST_OVERHEAD_MB);
    }

    #[test]
    fn a_vm_bigger_than_the_host_allows_is_refused() {
        let ok = vm_size(limits(1024, 100_000), 128, 1, 8);
        assert!(check_size(ok, 16384).is_ok());
        // 4 TiB, and a limit so large that adding the guest's share would overflow.
        let tib = PodLimits {
            memory_bytes: Some(4 * 1024 * 1024 * 1024 * 1024),
            ..PodLimits::default()
        };
        let e = check_size(vm_size(tib, 128, 1, 8), 16384).unwrap_err();
        assert!(e.contains("max_memory_mb"), "{e}");
        let huge = PodLimits {
            memory_bytes: Some(u64::MAX),
            ..PodLimits::default()
        };
        assert!(check_size(vm_size(huge, 128, 1, 8), 16384).is_err());
        // exactly the maximum is allowed
        let edge = VmSize {
            memory_mb: 16384,
            vcpus: 1,
        };
        assert!(check_size(edge, 16384).is_ok());
        assert!(check_size(
            VmSize {
                memory_mb: 16385,
                vcpus: 1
            },
            16384
        )
        .is_err());
    }

    #[test]
    fn the_limits_are_read_from_the_annotations_containerd_writes() {
        let spec = serde_json::json!({"annotations": {
            "io.kubernetes.cri.sandbox-memory": "536870912",
            "io.kubernetes.cri.sandbox-cpu-quota": "200000",
            "io.kubernetes.cri.sandbox-cpu-period": "100000",
        }});
        assert_eq!(
            limits_from_spec(&spec),
            PodLimits {
                memory_bytes: Some(536_870_912),
                cpu_quota: Some(200_000),
                cpu_period: Some(100_000)
            }
        );
        assert_eq!(
            limits_from_spec(&serde_json::json!({})),
            PodLimits::default()
        );
    }
}
