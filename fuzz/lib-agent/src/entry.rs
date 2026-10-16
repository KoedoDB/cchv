//! Entry points of the agent's fuzz targets. Each takes what the fuzzer made, runs the production code on it, and asserts what the
//! code promises (`panic!` is what the fuzzer reports). The promises are the ones the code and its comments make; a violation is
//! either a bug in the code or a promise it does not keep.

use std::collections::HashSet;
use std::path::Path;

use arbitrary::Arbitrary;
use serde_json::{json, Value};

use crate::{container, disks, exec, seccomp, spec, volumes};

// ── the OCI spec the host sends ─────────────────────────────────────────────────────────────────────────────────────────────

fn cap_names(v: &Value) -> Vec<&str> {
    v.as_array().map(|a| a.iter().filter_map(|c| c.as_str()).collect()).unwrap_or_default()
}

fn assert_caps_allowed(caps: &Value, what: &str) {
    for set in ["bounding", "effective", "inheritable", "permitted", "ambient"] {
        for c in cap_names(&caps[set]) {
            assert!(spec::ALLOWED_CAPS.contains(&c), "{what}: {set} has {c}, which is not allowed");
        }
    }
}

fn allows(a: Option<&str>) -> bool {
    matches!(a, Some("SCMP_ACT_ALLOW") | Some("SCMP_ACT_LOG") | Some("SCMP_ACT_TRACE") | Some("SCMP_ACT_NOTIFY"))
}

/// Whether a profile can let `socket(AF_VSOCK)` through: a rule that allows `socket` for family 40 (or for every family) and no rule
/// that refuses it; a profile that allows by default lets it through unless a rule refuses it.
fn vsock_socket_possible(profile: &Value) -> bool {
    let default_allows = allows(profile["defaultAction"].as_str());
    let mut refused = false;
    let mut allowed = false;
    for r in profile["syscalls"].as_array().into_iter().flatten() {
        if !cap_names(&r["names"]).contains(&"socket") {
            continue;
        }
        let matches40 = match r["args"].as_array() {
            None => true,
            Some(a) if a.is_empty() => true,
            Some(a) if a.len() == 1 && a[0]["index"] == 0 => {
                let value = a[0]["value"].as_u64().unwrap_or(0);
                let two = a[0]["valueTwo"].as_u64().unwrap_or(0);
                match a[0]["op"].as_str() {
                    Some("SCMP_CMP_EQ") => value == 40,
                    Some("SCMP_CMP_NE") => value != 40,
                    Some("SCMP_CMP_MASKED_EQ") => 40 & value == two,
                    Some("SCMP_CMP_GT") => 40 > value,
                    Some("SCMP_CMP_GE") => 40 >= value,
                    Some("SCMP_CMP_LT") => 40 < value,
                    Some("SCMP_CMP_LE") => 40 <= value,
                    _ => false,
                }
            }
            Some(_) => false,
        };
        if !matches40 {
            continue;
        }
        if allows(r["action"].as_str()) {
            allowed = true;
        } else {
            refused = true;
        }
    }
    if default_allows {
        !refused
    } else {
        allowed && !refused
    }
}

fn assert_profile_safe(profile: &Value, what: &str) {
    let obj = profile.as_object().unwrap_or_else(|| panic!("{what}: the seccomp profile is not an object"));
    assert!(!obj.contains_key("listenerPath") && !obj.contains_key("listenerMetadata"), "{what}: a listener survived");
    assert!(obj.contains_key("defaultAction"), "{what}: no defaultAction");
    assert!(obj.get("syscalls").map(|s| s.is_array()).unwrap_or(false), "{what}: no syscalls list");
    assert!(!vsock_socket_possible(profile), "{what}: socket(AF_VSOCK) can be allowed: {profile}");
    // No rule that allows a refused syscall.
    for r in profile["syscalls"].as_array().into_iter().flatten() {
        if !allows(r["action"].as_str()) {
            continue;
        }
        for n in cap_names(&r["names"]) {
            assert!(!seccomp::DENIED.contains(&n) && n != "clone3", "{what}: {n} is allowed by a rule: {r}");
        }
    }
}

fn path_is_clean(p: &str) -> bool {
    p.starts_with('/') && !p.contains("..")
}

fn assert_spec_safe(out: &Value) {
    let obj = out.as_object().expect("the spec is an object");
    for k in obj.keys() {
        assert!(
            matches!(k.as_str(), "ociVersion" | "process" | "root" | "mounts" | "linux"),
            "the spec has the field {k}"
        );
    }
    assert_eq!(out["process"]["noNewPrivileges"], json!(true), "noNewPrivileges");
    assert_caps_allowed(&out["process"]["capabilities"], "process");
    assert_eq!(out["root"]["path"], json!("rootfs"));
    let ns: Vec<&str> = out["linux"]["namespaces"].as_array().unwrap().iter().filter_map(|n| n["type"].as_str()).collect();
    assert_eq!(ns, vec!["mount", "pid"], "namespaces");
    assert_profile_safe(&out["linux"]["seccomp"], "sanitize_spec");
    assert_eq!(out["linux"]["resources"]["devices"], spec::default_devices(), "the device rules");
    for key in ["maskedPaths", "readonlyPaths"] {
        for p in out["linux"][key].as_array().unwrap() {
            assert!(path_is_clean(p.as_str().unwrap()), "{key}: {p}");
        }
    }
    for l in out["process"]["rlimits"].as_array().into_iter().flatten() {
        assert!(l["type"].as_str().unwrap().starts_with("RLIMIT_"));
        if l["type"] == "RLIMIT_NOFILE" {
            assert!(l["hard"].as_u64().unwrap() <= 1_048_576 && l["soft"].as_u64().unwrap() <= 1_048_576, "NOFILE");
        }
    }
    for k in out["linux"].as_object().unwrap().keys() {
        assert!(
            matches!(k.as_str(), "namespaces" | "resources" | "cgroupsPath" | "maskedPaths" | "readonlyPaths" | "rootfsPropagation" | "seccomp"),
            "the linux section has the field {k}"
        );
    }
}

fn assert_exec_process_safe(p: &Value, container: Option<&Value>, terminal: bool) {
    assert_eq!(p["noNewPrivileges"], json!(true));
    assert_eq!(p["terminal"], json!(terminal));
    assert_caps_allowed(&p["capabilities"], "exec");
    // Never more than the container has (when the container's process section is the sanitised one).
    if let Some(c) = container.and_then(|c| c.get("capabilities")).and_then(|c| c.as_object()) {
        for set in ["bounding", "effective", "inheritable", "permitted", "ambient"] {
            let have = spec::filter_caps(c.get(set));
            for cap in cap_names(&p["capabilities"][set]) {
                assert!(have.iter().any(|h| h.as_str() == Some(cap)), "exec {set}: {cap} is more than the container has");
            }
        }
    }
    assert!(!p["args"].as_array().unwrap().is_empty());
    assert!(p["cwd"].as_str().unwrap().starts_with('/'));
    for a in p["args"].as_array().unwrap().iter().chain(p["env"].as_array().unwrap()) {
        assert!(!a.as_str().unwrap().contains('\0'));
    }
    for l in p["rlimits"].as_array().unwrap() {
        assert!(l["soft"].as_u64().unwrap() <= l["hard"].as_u64().unwrap(), "soft above hard");
    }
}

/// JSON text → every function that interprets an OCI spec or a process of it.
pub fn spec(data: &[u8]) {
    let Ok(v) = serde_json::from_slice::<Value>(data) else { return };

    // The container spec.
    if let Ok(out) = spec::sanitize_spec(&v, "cid", spec::base_mounts()) {
        assert_spec_safe(&out);
        // And the container's process section as exec sees it: the sanitised one.
        let container = Some(&out["process"]);
        for terminal in [false, true] {
            // The client's process: the spec's own `process`, and the value as a whole.
            for candidate in [v.get("process"), Some(&v)].into_iter().flatten() {
                if let Ok(p) = spec::sanitize_exec_process(candidate, container, terminal) {
                    assert_exec_process_safe(&p, container, terminal);
                }
            }
        }
    }
    for terminal in [false, true] {
        if let Ok(p) = spec::sanitize_exec_process(&v, None, terminal) {
            assert_exec_process_safe(&p, None, terminal);
        }
    }

    // Pieces on their own.
    assert_profile_safe(&seccomp::enforce(v.get("seccomp")), "enforce");
    assert_profile_safe(&seccomp::enforce(Some(&v)), "enforce(whole)");
    assert_profile_safe(&seccomp::enforce(v.pointer("/linux/seccomp")), "enforce(linux)");
    for c in spec::filter_caps(Some(&v)).iter().chain(spec::filter_caps(v.get("bounding")).iter()) {
        assert!(spec::ALLOWED_CAPS.contains(&c.as_str().unwrap()));
    }
    if let Some(res) = container::sanitize_resources(Some(&v)) {
        // A limit of 0 or an absurd one is not passed on (the cgroup of the container would get a 0-byte memory limit).
        if let Some(m) = res.pointer("/memory/limit").and_then(|m| m.as_i64()) {
            assert!(m > 0);
        }
        if let Some(q) = res.pointer("/cpu/quota").and_then(|m| m.as_i64()) {
            assert!(q > 0 && res.pointer("/cpu/period").and_then(|p| p.as_u64()).unwrap_or(0) > 0);
        }
        if let Some(p) = res.pointer("/pids/limit").and_then(|m| m.as_i64()) {
            assert!(p > 0);
        }
    }
    if let Some(h) = spec::hostname_of(&v) {
        assert!(spec::valid_hostname(&h));
    }
}

fn normalised_first_component(dest: &str) -> Option<String> {
    // What the kernel makes of the path: empty and "." parts do nothing; ".." goes up.
    let mut stack: Vec<&str> = Vec::new();
    for part in dest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            p => stack.push(p),
        }
    }
    stack.first().map(|s| s.to_string())
}

/// Text → the small validators of the agent.
pub fn small(data: &[u8]) {
    let s = String::from_utf8_lossy(data);
    let s = s.as_ref();

    // Destinations: never inside the guest's own /proc or /sys, however the path is written.
    if spec::valid_destination(s) {
        let first = normalised_first_component(s);
        assert!(
            !matches!(first.as_deref(), Some("proc") | Some("sys") | None),
            "valid_destination({s:?}) allows a path that is {first:?} after normalisation"
        );
    }
    if spec::valid_hostname(s) {
        assert!(s.len() < 64 && !s.contains('/') && !s.contains('\0'));
    }
    if container::valid_container_id(s) {
        assert!(!s.contains('/') && !s.starts_with('.') && s.len() <= 128, "container id {s:?}");
    }
    if exec::valid_exec_id(s) {
        assert!(!s.contains('/') && !s.contains('.') && s.len() <= 64, "exec id {s:?}");
    }
    if let Some(pid) = exec::parse_pid(s) {
        assert!(pid > 1 && pid <= i32::MAX as u32);
    }
    if volumes::safe_component(s) {
        assert!(!s.contains('/') && s != "." && s != ".." && !s.starts_with('.'), "component {s:?}");
    }
    if let Some(parts) = volumes::safe_relative(s) {
        assert!(!parts.is_empty());
        for p in &parts {
            assert!(!p.is_empty() && !p.contains('/') && *p != "." && *p != "..", "part {p:?} of {s:?}");
        }
    }
    let _ = volumes::shared_name(s);
    let _ = volumes::legacy_name(s);
    let _ = volumes::file_mode(data.iter().take(4).fold(0u32, |a, b| a << 8 | *b as u32));
    if let Ok((root, name)) = volumes::location(s, Path::new("/run/containers/x")) {
        assert!(volumes::safe_component(&name), "volume name {name:?} from {s:?}");
        assert!(root.starts_with("/run/cloudhv-vols") || root.starts_with("/run/containers/x"), "root {root:?} from {s:?}");
    }
    // Options.
    let asked: Vec<String> = s.split(',').map(|x| x.to_string()).collect();
    let o = spec::volume_options(data.first().map(|b| b & 1 == 1).unwrap_or(false), &asked, &["rbind"]);
    assert!(o.iter().any(|x| x == "nosuid") && o.iter().any(|x| x == "nodev"), "options {o:?}");

    // Mount table text.
    let prefix = s.lines().next().unwrap_or("/");
    let below = container::mounts_under(s, prefix);
    for (i, a) in below.iter().enumerate() {
        for b in &below[i + 1..] {
            // A mount point below another is listed first (the order to unmount in).
            assert!(!b.starts_with(&format!("{a}/")), "{b} is below {a} but listed after it");
        }
    }

    // Terminal size, disks.
    let n = data.iter().take(8).fold(0u64, |a, b| a << 8 | *b as u64);
    let _ = exec::valid_terminal_size((n >> 32) as u32, n as u32);
    let _ = disks::vd_name((n % 100_000) as usize);
    let mut names: Vec<String> = s.split_whitespace().map(|x| x.to_string()).collect();
    disks::sort_vd_names(&mut names);
    let pairs: Vec<(String, String)> = names.iter().map(|x| (x.clone(), x.clone())).collect();
    let claimed: HashSet<String> = names.iter().take(2).cloned().collect();
    let _ = disks::find_by_serial(&pairs, s, &claimed);
    let _ = disks::unclaimed(&names, &claimed);
}

// ── the output buffer ───────────────────────────────────────────────────────────────────────────────────────────────────────

#[derive(Arbitrary, Debug)]
pub enum LogOp {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Read { stdout_offset: u64, stderr_offset: u64, max: u16 },
    Eof,
}

pub fn logs(ops: Vec<LogOp>) {
    let mut l = container::ContainerLogs::default();
    let (mut out_total, mut err_total) = (0u64, 0u64);
    for op in ops {
        match op {
            LogOp::Stdout(d) => {
                out_total += d.len() as u64;
                l.push_stdout(&d)
            }
            LogOp::Stderr(d) => {
                err_total += d.len() as u64;
                l.push_stderr(&d)
            }
            LogOp::Eof => l.eof = true,
            LogOp::Read { stdout_offset, stderr_offset, max } => {
                let max = max as usize;
                let r = l.read(stdout_offset, stderr_offset, max);
                assert!(r.stdout.len() <= max && r.stderr.len() <= max, "more than max returned");
                // The position after what was returned is within what has been written so far.
                assert!(r.stdout_next <= out_total && r.stderr_next <= err_total, "next beyond the stream");
                assert!(r.stdout_next >= r.stdout.len() as u64);
                if r.eof {
                    assert!(l.eof);
                }
            }
        }
        // The buffer is bounded; bytes are accounted for: what is held + what was dropped or taken = what was written.
        assert!(l.stdout.len() <= container::MAX_LOG_BUFFER && l.stderr.len() <= container::MAX_LOG_BUFFER, "unbounded buffer");
        assert_eq!(l.stdout_base + l.stdout.len() as u64, out_total, "stdout accounting");
        assert_eq!(l.stderr_base + l.stderr.len() as u64, err_total, "stderr accounting");
        assert!(l.stdout_dropped <= l.stdout_base && l.stderr_dropped <= l.stderr_base);
    }
}

// ── volumes written into the guest ─────────────────────────────────────────────────────────────────────────────────────────

#[derive(Arbitrary, Debug)]
pub enum Plant {
    /// A symbolic link in the volume to the outside file (absolute, or relative with `..`).
    LinkToOutside { at: String, relative: bool },
    LinkToOutsideDir { at: String },
    /// A link to another place in the volume.
    LinkInside { at: String, to: String },
    Dir { at: String },
    File { at: String },
    /// A hard link to the outside file.
    HardLinkToOutside { at: String },
    Fifo { at: String },
}

#[derive(Arbitrary, Debug)]
pub struct VolumeCase {
    pub name: String,
    pub files: Vec<(String, Vec<u8>, u32)>,
    pub destination: String,
    pub in_use: bool,
    pub plants: Vec<Plant>,
    pub twice: bool,
}

fn rel_ok(p: &str) -> Option<&str> {
    // Plants are made with plain std calls: keep them inside the volume (the plant is the test's, not what is tested).
    let p = p.trim_matches('/');
    if p.is_empty() || p.len() > 40 || p.contains('\0') || p.split('/').any(|c| c == ".." || c == "." || c.is_empty()) {
        None
    } else {
        Some(p)
    }
}

/// Make the parents of a plant, without following a link or going through a file: the plants are the test's own set-up and must not write
/// outside the volume by accident (and writing to a FIFO the test made blocks for ever). Returns whether the plant may be made.
fn parent_of_plant(vol: &Path, at: &str) -> bool {
    let mut cur = vol.to_path_buf();
    let comps: Vec<&str> = at.split('/').collect();
    for c in &comps[..comps.len() - 1] {
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return false,
            Err(_) => {
                if std::fs::create_dir(&cur).is_err() {
                    return false;
                }
            }
        }
    }
    std::fs::symlink_metadata(cur.join(comps[comps.len() - 1])).is_err()
}

pub fn volumes(case: VolumeCase) {
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    let base = tempfile::tempdir().expect("tempdir");
    let root = base.path().join("root");
    let outside = base.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let secret = outside.join("secret");
    std::fs::write(&secret, b"SECRET").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    let secret_before = std::fs::metadata(&secret).unwrap();
    std::fs::write(outside.join("inner"), b"inner").unwrap();

    // A volume that already exists, with whatever a container could have put into it.
    if volumes::safe_component(&case.name) {
        let vol = root.join(&case.name);
        let _ = std::fs::create_dir_all(&vol);
        for p in case.plants.iter().take(12) {
            match p {
                Plant::LinkToOutside { at, relative } => {
                    if let Some(at) = rel_ok(at) {
                        let target = if *relative { std::path::PathBuf::from("../../outside/secret") } else { secret.clone() };
                        if parent_of_plant(&vol, at) {
                            let _ = symlink(target, vol.join(at));
                        }
                    }
                }
                Plant::LinkToOutsideDir { at } => {
                    if let Some(at) = rel_ok(at) {
                        if parent_of_plant(&vol, at) {
                            let _ = symlink(&outside, vol.join(at));
                        }
                    }
                }
                Plant::LinkInside { at, to } => {
                    if let (Some(at), Some(to)) = (rel_ok(at), rel_ok(to)) {
                        if parent_of_plant(&vol, at) {
                            let _ = symlink(vol.join(to), vol.join(at));
                        }
                    }
                }
                Plant::Dir { at } => {
                    if let Some(at) = rel_ok(at) {
                        if parent_of_plant(&vol, at) {
                            let _ = std::fs::create_dir(vol.join(at));
                        }
                    }
                }
                Plant::File { at } => {
                    if let Some(at) = rel_ok(at) {
                        if parent_of_plant(&vol, at) {
                            let _ = std::fs::write(vol.join(at), b"planted");
                        }
                    }
                }
                Plant::HardLinkToOutside { at } => {
                    if let Some(at) = rel_ok(at) {
                        if parent_of_plant(&vol, at) {
                            let _ = std::fs::hard_link(&secret, vol.join(at));
                        }
                    }
                }
                Plant::Fifo { at } => {
                    if let Some(at) = rel_ok(at) {
                        if parent_of_plant(&vol, at) {
                            let c = std::ffi::CString::new(vol.join(at).to_str().unwrap()).unwrap();
                            unsafe { libc::mkfifo(c.as_ptr(), 0o644) };
                        }
                    }
                }
            }
        }
    }

    let files: Vec<volumes::VolumeFile<'_>> = case
        .files
        .iter()
        .take(16)
        .map(|(p, c, m)| volumes::VolumeFile { path: p.as_str(), content: &c[..c.len().min(4096)], mode: *m })
        .collect();
    for _ in 0..(1 + case.twice as usize) {
        if let Ok(path) = volumes::materialize(&root, &case.name, &files, &case.destination, case.in_use) {
            assert!(path.starts_with(&root), "materialize returned {path:?}, outside the root");
        }
    }

    // Nothing outside the volume root was touched: the secret has its content, mode and owner; the outside holds its two files only.
    let after = std::fs::metadata(&secret).unwrap();
    assert_eq!(std::fs::read(&secret).unwrap(), b"SECRET", "the outside file was written");
    assert_eq!(after.mode(), secret_before.mode(), "the outside file's mode changed");
    assert_eq!(after.uid(), secret_before.uid());
    let mut names: Vec<_> = std::fs::read_dir(&outside).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    assert_eq!(names, vec!["inner".to_string(), "secret".to_string()], "something was made outside the volumes");
    assert_eq!(std::fs::read(outside.join("inner")).unwrap(), b"inner");
    let top: Vec<_> = std::fs::read_dir(base.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(top.len(), 2, "something was made next to the volume root: {top:?}");
    volumes::remove(&root, &case.name);
}

// ── decoding of the RPC messages ───────────────────────────────────────────────────────────────────────────────────────────

macro_rules! roundtrip {
    ($data:expr, $($t:ty),+ $(,)?) => {{
        use protobuf::Message;
        $(
            if let Ok(m) = <$t>::parse_from_bytes($data) {
                let bytes = m.write_to_bytes().expect("serialise");
                let again = <$t>::parse_from_bytes(&bytes).expect("a message we wrote must parse");
                assert_eq!(m, again, "{} does not survive a round trip", stringify!($t));
            }
        )+
    }};
}

pub fn proto(data: &[u8]) {
    use crate::proto::agent::*;
    roundtrip!(
        data,
        CreateContainerRequest, VolumeMount, InlineFile, StartContainerRequest, KillContainerRequest, DeleteContainerRequest,
        WaitContainerRequest, ExecProcessRequest, StateContainerRequest, GetContainerLogsRequest, ReadLogsRequest,
        ConfigureNetworkRequest, AdoptContainerRequest, ExecCreateRequest, ExecStartRequest, ExecKillRequest, ExecWaitRequest,
        ExecDeleteRequest, ExecWriteRequest, ExecCloseStdinRequest, ExecReadRequest, ExecResizeRequest,
    );
}
