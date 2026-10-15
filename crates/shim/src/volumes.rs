//! The volumes of a container.
//!
//! A pod's runtime (kubelet, through containerd) puts the pod's files into each container with bind mounts in the OCI
//! spec: `/etc/resolv.conf`, `/etc/hosts`, `/etc/hostname`, `/dev/termination-log`, the ConfigMaps, Secrets and the service
//! account token, the emptyDirs. The sources are paths on the host, and the VM cannot see the host's files. So the shim reads
//! each source here and sends its contents with the container to the guest agent, which writes them into the VM and binds
//! them from there.
//!
//! The contents are a copy taken when the container starts: a file the runtime updates later (a ConfigMap, a token that is
//! rotated) is not updated in the VM, and a file the container writes is not written back to the host. An emptyDir starts
//! empty, is shared by the containers of the pod (they are in one VM), and is lost with the VM.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use base64::Engine;
use log::warn;

/// Where in the guest the volumes are written. The agent accepts no other prefix.
const VOLUME_ROOT: &str = "/run/cloudhv-vols";

/// What one volume may hold. The agent's RPC message has a limit of 4 MiB in all.
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_FILES: usize = 512;
const MAX_DEPTH: usize = 8;

/// The volumes (JSON, as the daemon's API takes them) for the bind mounts of the OCI spec.
pub fn from_spec(config_json: &[u8]) -> Vec<serde_json::Value> {
    let spec: serde_json::Value = match serde_json::from_slice(config_json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mounts = match spec.get("mounts").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return Vec::new(),
    };

    let mut volumes = Vec::new();
    for m in mounts {
        if m.get("type").and_then(|t| t.as_str()) != Some("bind") {
            continue;
        }
        let (Some(dest), Some(source)) = (
            m.get("destination").and_then(|d| d.as_str()),
            m.get("source").and_then(|s| s.as_str()),
        ) else {
            continue;
        };
        let readonly = m
            .get("options")
            .and_then(|o| o.as_array())
            .map(|o| o.iter().any(|x| x.as_str() == Some("ro")))
            .unwrap_or(false);
        match read_source(Path::new(source), dest) {
            Ok(files) => {
                let files: Vec<serde_json::Value> = files
                    .into_iter()
                    .map(|(path, (content, mode))| {
                        serde_json::json!({
                            "path": path,
                            "content": base64::engine::general_purpose::STANDARD.encode(content),
                            "mode": mode,
                        })
                    })
                    .collect();
                volumes.push(serde_json::json!({
                    "destination": dest,
                    "source": format!("{VOLUME_ROOT}/{:016x}", fnv1a(source)),
                    "readonly": readonly,
                    "files": files,
                }));
            }
            Err(e) => warn!("volume {dest} (from {source}) is not passed to the VM: {e}"),
        }
    }
    volumes
}

/// The files of a bind source: a regular file is one file named like its destination; a directory is its tree.
fn read_source(source: &Path, dest: &str) -> Result<BTreeMap<String, (Vec<u8>, u32)>, String> {
    let meta = std::fs::metadata(source).map_err(|e| e.to_string())?;
    let mut files = BTreeMap::new();
    if meta.is_file() {
        if meta.len() as usize > MAX_BYTES {
            return Err(format!("file is larger than {MAX_BYTES} bytes"));
        }
        let name = Path::new(dest)
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("destination has no file name")?;
        let content = std::fs::read(source).map_err(|e| e.to_string())?;
        files.insert(name.to_string(), (content, meta.permissions().mode() & 0o777));
    } else if meta.is_dir() {
        let mut total = 0;
        walk(source, "", 0, &mut total, &mut files)?;
    } else {
        return Err("neither a file nor a directory".into());
    }
    Ok(files)
}

fn walk(
    dir: &Path,
    prefix: &str,
    depth: usize,
    total: &mut usize,
    files: &mut BTreeMap<String, (Vec<u8>, u32)>,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("directory is deeper than the limit".into());
    }
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().to_string();
        // The kubelet writes ConfigMaps and Secrets as `..data` -> `..<timestamp>/`, and each file is a link through
        // `..data`. The links are followed below, so the `..` entries are left out (they would be there twice).
        if name.starts_with("..") {
            continue;
        }
        let path = entry.path();
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue, // a dangling link
        };
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if meta.is_dir() {
            walk(&path, &rel, depth + 1, total, files)?;
        } else if meta.is_file() {
            *total += meta.len() as usize;
            if *total > MAX_BYTES || files.len() >= MAX_FILES {
                return Err(format!(
                    "more than {MAX_FILES} files or {MAX_BYTES} bytes"
                ));
            }
            let content = std::fs::read(&path).map_err(|e| e.to_string())?;
            files.insert(rel, (content, meta.permissions().mode() & 0o777));
        }
    }
    Ok(())
}

/// A stable name for a source path: the containers of a pod that mount the same host directory (an emptyDir) get the same
/// directory in the VM.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_and_directory_sources() {
        let dir = std::env::temp_dir().join(format!("cloudhv-vol-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("..2026_01_01")).unwrap();
        std::fs::write(dir.join("..2026_01_01/token"), b"secret").unwrap();
        std::os::unix::fs::symlink("..2026_01_01", dir.join("..data")).unwrap();
        std::os::unix::fs::symlink("..data/token", dir.join("token")).unwrap();
        let resolv = dir.join("resolv.conf");
        std::fs::write(&resolv, b"nameserver 10.45.0.10\n").unwrap();

        let spec = serde_json::json!({"mounts": [
            {"destination": "/etc/resolv.conf", "type": "bind", "source": resolv, "options": ["rbind", "ro"]},
            {"destination": "/var/run/secrets/x", "type": "bind", "source": dir, "options": ["rbind"]},
            {"destination": "/proc", "type": "proc", "source": "proc"},
            {"destination": "/gone", "type": "bind", "source": "/nonexistent/path"},
        ]});
        let v = from_spec(spec.to_string().as_bytes());
        assert_eq!(v.len(), 2);
        assert_eq!(v[0]["readonly"], true);
        assert_eq!(v[0]["files"][0]["path"], "resolv.conf");
        assert!(v[0]["source"].as_str().unwrap().starts_with(VOLUME_ROOT));
        let names: Vec<&str> = v[1]["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["resolv.conf", "token"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
