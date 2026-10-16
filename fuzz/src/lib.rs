//! Entry points of the shim's fuzz targets (the agent's and the daemon's are in lib-agent and lib-daemon).

pub mod shim {
    use std::path::{Path, PathBuf};

    use arbitrary::Arbitrary;
    use base64::Engine;
    use containerd_shim_cloudhv::{cachekey, instance, vmsize, volumes};
    use fuzz_daemon::entry::{plant, Entry, Kind};
    use serde_json::{json, Value};

    #[derive(Arbitrary, Debug)]
    pub enum Src {
        /// The persistent volume root itself, a volume below it, a directory inside the volume.
        Root,
        Vol,
        Sub(u8),
        /// Outside every root: a directory and a file the shim must never read.
        Outside,
        OutsideSecret,
        Raw(String),
    }

    #[derive(Arbitrary, Debug)]
    pub struct MountSpec {
        pub src: Src,
        pub dest: String,
        pub bind: bool,
        pub ro: bool,
    }

    #[derive(Arbitrary, Debug)]
    pub struct VolCase {
        pub entries: Vec<Entry>,
        pub mounts: Vec<MountSpec>,
        /// Use these bytes as the spec instead (any JSON, or not JSON at all).
        pub raw: Vec<u8>,
        pub use_raw: bool,
    }

    const NAMES: [&str; 5] = ["a", "b", "c", "d", "e"];

    /// A tree and an OCI spec whose mounts name it (and what is outside it) → the volumes the shim would send to the VM. What must hold: nothing
    /// outside the volume roots is read, whatever links the tree has.
    pub fn volumes(case: VolCase) {
        let base = tempfile::tempdir().expect("tempdir");
        let pv = base.path().join("pv");
        let vol = pv.join("vol");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(&vol).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"SECRET-OUTSIDE-THE-ROOTS").unwrap();
        std::fs::write(outside.join("inner"), b"inner").unwrap();

        let mut made = Vec::new();
        for e in case.entries.iter().take(40) {
            // A hard link to the secret is the secret (the same file): not a link the shim could tell from the file, and not one a
            // customer can make to a file they cannot read.
            if matches!(e.kind, Kind::HardLinkOutside) {
                continue;
            }
            plant(&vol, &outside, e, &mut made);
        }

        let spec_bytes: Vec<u8> = if case.use_raw {
            case.raw.clone()
        } else {
            let mounts: Vec<Value> = case
                .mounts
                .iter()
                .take(8)
                .map(|m| {
                    let src: PathBuf = match &m.src {
                        Src::Root => pv.clone(),
                        Src::Vol => vol.clone(),
                        Src::Sub(i) => vol.join(NAMES[*i as usize % NAMES.len()]),
                        Src::Outside => outside.clone(),
                        Src::OutsideSecret => outside.join("secret"),
                        Src::Raw(s) => PathBuf::from(s),
                    };
                    let mut o = vec![json!("rbind")];
                    if m.ro {
                        o.push(json!("ro"));
                    }
                    json!({"destination": m.dest, "type": if m.bind { "bind" } else { "tmpfs" }, "source": src.to_string_lossy(), "options": o})
                })
                .collect();
            serde_json::to_vec(&json!({ "mounts": mounts })).unwrap()
        };

        let roots = vec![pv.to_string_lossy().to_string()];
        let started = std::time::Instant::now();
        let result = volumes::from_spec(&spec_bytes, &roots);
        assert!(started.elapsed().as_secs() < 25, "reading the volumes took {:?}", started.elapsed());
        if let Ok(vols) = result {
            for v in &vols {
                let source = v["source"].as_str().unwrap_or("");
                assert!(source.starts_with("/run/cloudhv-vols/") && !source["/run/cloudhv-vols/".len()..].contains('/'), "guest dir {source:?}");
                for f in v["files"].as_array().into_iter().flatten() {
                    let content = base64::engine::general_purpose::STANDARD.decode(f["content"].as_str().unwrap_or("")).unwrap();
                    assert!(!content.windows(6).any(|w| w == b"SECRET"), "a file from outside the roots was read: {v}");
                    let path = f["path"].as_str().unwrap_or("");
                    assert!(!path.starts_with('/') && !path.split('/').any(|c| c == ".." || c.is_empty()), "file path {path:?}");
                    assert!(f["mode"].as_u64().unwrap_or(0) <= 0o777, "mode");
                }
            }
        }
        let _ = Path::new("/");
    }

    /// Bytes → the small functions of the shim: digests, VM sizes, cgroup paths, the limits file.
    pub fn small(data: &[u8]) {
        let s = String::from_utf8_lossy(data);
        let s = s.as_ref();

        if cachekey::valid_digest(s) {
            assert!(s.len() == 71 && s.starts_with("sha256:") && s[7..].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        }
        if instance::valid_cgroups_path(s) {
            assert!(s.len() <= 1100 && !s.contains('\0') && !s.contains('\n'), "cgroups path {s:?}");
            assert!(s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'/' | b':')), "cgroups path {s:?}");
            // A path that reaches the cgroup tree: never an empty, `.` or `..` component (a name that merely contains dots is one).
            for c in s.trim_start_matches('/').split(|ch| ch == '/' || ch == ':') {
                assert!(!c.is_empty() && c != "." && c != "..", "component {c:?} of {s:?}");
            }
        }

        // VM sizes from arbitrary limits.
        let n = data.iter().take(24).fold(0u128, |a, b| (a << 8 | *b as u128) & ((1u128 << 120) - 1));
        let limits = vmsize::PodLimits {
            memory_bytes: if data.first().map(|b| b & 1 == 1).unwrap_or(false) { Some((n >> 40) as u64) } else { None },
            cpu_quota: if data.get(1).map(|b| b & 1 == 1).unwrap_or(false) { Some(n as i64) } else { None },
            cpu_period: if data.get(2).map(|b| b & 1 == 1).unwrap_or(false) { Some((n >> 16) as u64) } else { None },
        };
        let default_mem = (n >> 8) as u64 % 100_000;
        let default_cpus = (n >> 4) as u32 % 64;
        let max_cpus = (n >> 12) as u32 % 128;
        let size = vmsize::vm_size(limits, default_mem, default_cpus, max_cpus);
        assert!(size.vcpus >= 1, "a VM with no CPU");
        assert!(size.vcpus <= max_cpus.max(1).max(default_cpus.min(max_cpus.max(1))) .max(1), "more CPUs than the cap: {size:?}");
        assert!(size.memory_mb >= default_mem, "less memory than the default");
        let _ = vmsize::check_size(size, 16384);
        if let Ok(v) = serde_json::from_slice::<Value>(data) {
            let _ = vmsize::limits_from_spec(&v);
        }
        // The shim's own limits file.
        let _ = serde_json::from_slice::<containerd_shim_cloudhv::config::ShimLimits>(data);
    }
}
