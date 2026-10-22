//! The logical volumes of persistent block volumes: made, zeroed, tagged and checked here, never by the pod's request.
//!
//! The daemon (`blockvol.rs`) does not run LVM. Making a logical volume needs the device-mapper control device, write access to LVM's own
//! directories and a tool that parses a lot, none of which the confined daemon has or should have. So the work is done by a small second
//! process, `cloudhv-sandbox-daemon --volume-helper`, a unit of its own (`cloudhv-volume-helper.service`), which listens on a Unix socket
//! (0600, root's) and understands two requests:
//!
//! - `ensure {namespace, volume, size_mib}`: the logical volume of that Kubernetes namespace and volume name, made if it is not there.
//! - `tags {major, minor}`: what the helper knows of the logical volume with those device numbers (its volume group, name, size and tags).
//!
//! What it guarantees:
//!
//! - **The name is derived, never given**: `pv-<namespace>_<volume>` in the one volume group of the configuration (`volume_block_vg`). The
//!   annotation of a pod has a volume name and a size; it cannot name a logical volume, so it cannot name another pod's. `_` cannot be in
//!   a namespace or a volume name, so two pairs never make the same name (`a-b` + `c` and `a` + `b-c` do not meet).
//! - **Every logical volume it makes carries `cloudhv-ns=<namespace>`** from the moment it exists (`lvcreate --addtag`). A logical volume
//!   without that tag, or with another namespace's, is never adopted, extended or handed out.
//! - **It is zeroed whole before it is ready.** LVM does not zero a new volume: it holds what the extents held before, which may be another
//!   customer's data. After `lvcreate` the helper writes zeros over all of it and only then adds `cloudhv-ready`. A volume with the namespace
//!   tag and without `cloudhv-ready` is a creation that did not finish (a crash, a full disk): it is zeroed again, and nothing sees it before.
//! - **Never shrunk, never deleted, never formatted.** A larger `size_mib` extends the logical volume (the file system is not grown: follow-up),
//!   and the new extents are zeroed as well (LVM does not zero them either, and the guest can read the whole device): `cloudhv-extending=<old MiB>`
//!   is on the volume from before `lvextend` until the new part is zeroed, so a crash in between is finished by the next request, and it is
//!   the new part only that is zeroed again, never the data. Deleting a Project's volume is an operator's action. **`cloudhv-ready` must
//!   not be removed by hand**: a volume without it is zeroed whole.
//! - **A cap**: `volume_block_max_mib` is the most any one volume may be, made or extended to. 0 (the default) refuses every creation.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::{info, warn};
use serde::{Deserialize, Serialize};

/// The tag that names the namespace a logical volume belongs to: `cloudhv-ns=<namespace>`.
pub const NS_TAG_KEY: &str = "cloudhv-ns=";
/// The tag of a logical volume that was zeroed whole and is fit to be given to a VM.
pub const READY_TAG: &str = "cloudhv-ready";
/// The tag of a volume that is being extended: `cloudhv-extending=<MiB the volume had>`; the part from there on is not zeroed yet.
pub const EXTENDING_KEY: &str = "cloudhv-extending=";
/// The longest a device-mapper name may be (`DM_NAME_LEN` is 128, with the terminator).
const DM_NAME_MAX: usize = 127;

pub fn ns_tag(namespace: &str) -> String {
    format!("{NS_TAG_KEY}{namespace}")
}

/// A Kubernetes namespace: a DNS label (lower-case letters, digits and `-`, 63 at most, not starting or ending with `-`).
pub fn valid_namespace(s: &str) -> bool {
    label(s, 63)
}

/// The name of a volume in a pod's annotation: the same kind of label, 32 at most.
pub fn valid_volume(s: &str) -> bool {
    label(s, 32)
}

fn label(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The name of the logical volume of a namespace's volume: `pv-<namespace>_<volume>`.
pub fn lv_name(namespace: &str, volume: &str) -> Result<String, String> {
    if !valid_namespace(namespace) {
        return Err(format!("{namespace:?} is not a namespace name"));
    }
    if !valid_volume(volume) {
        return Err(format!("{volume:?} is not a volume name"));
    }
    Ok(format!("pv-{namespace}_{volume}"))
}

/// The device-mapper name of a logical volume (`/sys/dev/block/<major>:<minor>/dm/name`): the names of the volume group and of the logical
/// volume joined by `-`, each with its own `-` written twice. The length is checked: the kernel refuses a longer one.
pub fn dm_name(vg: &str, lv: &str) -> Result<String, String> {
    let n = format!("{}-{}", vg.replace('-', "--"), lv.replace('-', "--"));
    if n.len() > DM_NAME_MAX {
        return Err(format!(
            "the device name of {vg}/{lv} would be {} characters (the most is {DM_NAME_MAX})",
            n.len()
        ));
    }
    Ok(n)
}

/// What is known of a logical volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LvInfo {
    pub vg: String,
    pub lv: String,
    pub size_mib: u64,
    pub tags: Vec<String>,
    /// The device numbers; 0 for both when the volume is not active.
    pub major: u32,
    pub minor: u32,
}

impl LvInfo {
    pub fn has_tag(&self, t: &str) -> bool {
        self.tags.iter().any(|x| x == t)
    }
    /// The size (MiB) the volume had when an extension began that has not ended, if one has not (the least, if several tags).
    pub fn extending_from(&self) -> Option<u64> {
        self.tags
            .iter()
            .filter_map(|t| t.strip_prefix(EXTENDING_KEY))
            .filter_map(|n| n.parse::<u64>().ok())
            .min()
    }
    /// The namespaces named by the `cloudhv-ns=` tags of the volume.
    pub fn namespaces(&self) -> Vec<&str> {
        self.tags
            .iter()
            .filter_map(|t| t.strip_prefix(NS_TAG_KEY))
            .collect()
    }
}

/// The primitives of LVM. `Lvm` runs the tools; the tests have a fake.
pub trait Backend {
    fn find(&self, vg: &str, lv: &str) -> Result<Option<LvInfo>>;
    fn find_dev(&self, major: u32, minor: u32) -> Result<Option<LvInfo>>;
    /// `lvcreate` of a thick volume of `size_mib` that carries `tag` from the start.
    fn create(&self, vg: &str, lv: &str, size_mib: u64, tag: &str) -> Result<()>;
    fn extend(&self, vg: &str, lv: &str, size_mib: u64) -> Result<()>;
    fn add_tag(&self, vg: &str, lv: &str, tag: &str) -> Result<()>;
    fn del_tag(&self, vg: &str, lv: &str, tag: &str) -> Result<()>;
    /// Make the volume active (it is, after `create`; after a reboot it may not be).
    fn activate(&self, vg: &str, lv: &str) -> Result<()>;
    /// Write zeros over the volume from `from_mib` to its end.
    fn zero(&self, info: &LvInfo, from_mib: u64) -> Result<()>;
}

/// What the helper may do.
#[derive(Debug, Clone)]
pub struct Policy {
    pub vg: String,
    /// The largest a volume may be made or extended to. 0: none may.
    pub max_mib: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ensured {
    pub info: LvInfo,
    pub created: bool,
    pub extended: bool,
}

/// The logical volume of `namespace`'s `volume`, ready to be given to a VM: made and zeroed if it is not there, finished if a creation did not end,
/// extended if `size_mib` is more than it is. An error is a refusal (a volume of another namespace or without an owner, a size over the cap).
pub fn ensure(
    b: &dyn Backend,
    p: &Policy,
    namespace: &str,
    volume: &str,
    size_mib: u64,
) -> Result<Ensured> {
    let lv = lv_name(namespace, volume).map_err(|e| anyhow::anyhow!(e))?;
    dm_name(&p.vg, &lv).map_err(|e| anyhow::anyhow!(e))?;
    if size_mib == 0 {
        anyhow::bail!("a volume of 0 MiB");
    }
    if size_mib > p.max_mib {
        anyhow::bail!(
            "a volume of {size_mib} MiB is more than volume_block_max_mib ({})",
            p.max_mib
        );
    }
    let tag = ns_tag(namespace);
    let mut created = false;
    let mut extended = false;
    let mut info = match b.find(&p.vg, &lv).context("look for the volume")? {
        Some(i) => {
            let owners = i.namespaces();
            if owners.is_empty() {
                anyhow::bail!(
                    "the logical volume {}/{lv} exists and has no owner tag: it is not adopted",
                    p.vg
                );
            }
            if owners != [namespace] {
                anyhow::bail!(
                    "the logical volume {}/{lv} belongs to another namespace",
                    p.vg
                );
            }
            i
        }
        None => {
            b.create(&p.vg, &lv, size_mib, &tag)
                .context("make the logical volume")?;
            created = true;
            b.find(&p.vg, &lv)
                .context("look for the volume just made")?
                .context("the logical volume just made is not there")?
        }
    };
    b.activate(&p.vg, &lv).context("activate the volume")?;
    if !info.has_tag(READY_TAG) {
        // New, or a creation that did not finish: whatever is on it is not this namespace's. Zero it all, and only then say it is ready.
        b.zero(&info, 0).context("zero the volume")?;
        b.add_tag(&p.vg, &lv, READY_TAG)
            .context("mark the volume ready")?;
    } else {
        // An extension that did not end: the part from where the volume was is not zeroed (never the data before it).
        if let Some(from) = info.extending_from() {
            b.zero(&info, from.min(info.size_mib))
                .context("zero the part added by an earlier extension")?;
            b.del_tag(&p.vg, &lv, &format!("{EXTENDING_KEY}{from}"))
                .context("finish the earlier extension")?;
            info = b.find(&p.vg, &lv)?.context("the volume is gone")?;
        }
        if size_mib > info.size_mib {
            let old = info.size_mib;
            let mark = format!("{EXTENDING_KEY}{old}");
            b.add_tag(&p.vg, &lv, &mark).context("mark the extension")?;
            b.extend(&p.vg, &lv, size_mib)
                .context("extend the volume")?;
            info = b.find(&p.vg, &lv)?.context("the volume is gone")?;
            b.zero(&info, old).context("zero the added part")?;
            b.del_tag(&p.vg, &lv, &mark)
                .context("finish the extension")?;
            extended = true;
        }
    }
    let info = b.find(&p.vg, &lv)?.context("the volume is gone")?;
    if !info.has_tag(READY_TAG) || (info.major == 0 && info.minor == 0) {
        anyhow::bail!(
            "the logical volume {}/{lv} is not ready or not active",
            p.vg
        );
    }
    Ok(Ensured {
        info,
        created,
        extended,
    })
}

// ---- the protocol -------------------------------------------------------------------------------------------------------------------------

/// A request is one line of JSON; the answer is one line.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    Ensure {
        namespace: String,
        volume: String,
        size_mib: u64,
    },
    Tags {
        major: u32,
        minor: u32,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// For `ensure` and `tags`: the volume (`tags`: none if there is no logical volume with those numbers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<LvInfo>,
    #[serde(default)]
    pub created: bool,
    #[serde(default)]
    pub extended: bool,
}

/// The longest request line the helper reads.
const MAX_LINE: u64 = 4096;

pub fn handle(b: &dyn Backend, p: &Policy, req: &Request) -> Response {
    match req {
        Request::Ensure {
            namespace,
            volume,
            size_mib,
        } => match ensure(b, p, namespace, volume, *size_mib) {
            Ok(e) => Response {
                ok: true,
                info: Some(e.info),
                created: e.created,
                extended: e.extended,
                ..Default::default()
            },
            Err(e) => Response {
                ok: false,
                error: Some(format!("{e:#}")),
                ..Default::default()
            },
        },
        Request::Tags { major, minor } => match b.find_dev(*major, *minor) {
            // Only the volume group of the configuration: the helper says nothing of other volumes of the host.
            Ok(Some(i)) if i.vg == p.vg => Response {
                ok: true,
                info: Some(i),
                ..Default::default()
            },
            Ok(_) => Response {
                ok: true,
                ..Default::default()
            },
            Err(e) => Response {
                ok: false,
                error: Some(format!("{e:#}")),
                ..Default::default()
            },
        },
    }
}

/// Serve `listener` for ever, one connection at a time.
pub fn serve(listener: UnixListener, b: &dyn Backend, p: &Policy) -> ! {
    loop {
        let (mut conn, _) = match listener.accept() {
            Ok(c) => c,
            Err(e) => {
                warn!("accept: {e}");
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        let _ = conn.set_read_timeout(Some(Duration::from_secs(10)));
        let _ = conn.set_write_timeout(Some(Duration::from_secs(10)));
        let mut line = String::new();
        let read = BufReader::new((&conn).take(MAX_LINE)).read_line(&mut line);
        let resp = match read {
            Ok(n) if n > 0 && line.ends_with('\n') => {
                match serde_json::from_str::<Request>(&line) {
                    Ok(req) => {
                        info!("request: {}", line.trim());
                        handle(b, p, &req)
                    }
                    Err(e) => Response {
                        ok: false,
                        error: Some(format!("not a request: {e}")),
                        ..Default::default()
                    },
                }
            }
            _ => Response {
                ok: false,
                error: Some("no request line".into()),
                ..Default::default()
            },
        };
        if let Ok(mut text) = serde_json::to_string(&resp) {
            text.push('\n');
            let _ = conn.write_all(text.as_bytes());
        }
    }
}

/// The client of the daemon: one connection per request.
#[derive(Debug, Clone)]
pub struct Client {
    pub socket: PathBuf,
}

impl Client {
    fn call(&self, req: &Request, timeout: Duration) -> Result<Response> {
        let mut conn = UnixStream::connect(&self.socket).with_context(|| {
            format!(
                "connect to the volume helper at {} (is cloudhv-volume-helper running?)",
                self.socket.display()
            )
        })?;
        conn.set_read_timeout(Some(timeout))?;
        conn.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut text = serde_json::to_string(req)?;
        text.push('\n');
        conn.write_all(text.as_bytes())?;
        let mut answer = String::new();
        BufReader::new((&conn).take(1 << 20))
            .read_line(&mut answer)
            .context("read the helper's answer")?;
        let resp: Response = serde_json::from_str(&answer).context("the helper's answer")?;
        if !resp.ok {
            anyhow::bail!(
                "{}",
                resp.error
                    .unwrap_or_else(|| "the volume helper refused".into())
            );
        }
        Ok(resp)
    }

    /// How long to wait for `ensure`: zeroing is a write of the whole volume (50 MiB/s is assumed as a floor).
    pub fn ensure_timeout(size_mib: u64) -> Duration {
        Duration::from_secs((120 + size_mib / 50).min(3600))
    }

    pub fn ensure(&self, namespace: &str, volume: &str, size_mib: u64) -> Result<Ensured> {
        let r = self.call(
            &Request::Ensure {
                namespace: namespace.to_string(),
                volume: volume.to_string(),
                size_mib,
            },
            Self::ensure_timeout(size_mib),
        )?;
        Ok(Ensured {
            info: r.info.context("the helper's answer has no volume")?,
            created: r.created,
            extended: r.extended,
        })
    }

    /// The logical volume with these device numbers, if the helper knows one (in its volume group).
    pub fn tags(&self, major: u32, minor: u32) -> Result<Option<LvInfo>> {
        Ok(self
            .call(&Request::Tags { major, minor }, Duration::from_secs(30))?
            .info)
    }
}

/// The helper process: `cloudhv-sandbox-daemon --volume-helper [daemon.json]`. Listens on `volume_helper_socket` (0600, root's) and never returns.
pub fn run_helper(config: &crate::config::DaemonConfig) -> Result<()> {
    if config.volume_block_vg.is_empty() {
        anyhow::bail!("volume_block_vg is empty: there is nothing for the volume helper to do");
    }
    let socket = Path::new(&config.volume_helper_socket);
    let _ = std::fs::remove_file(socket);
    // The socket is made 0600 from the start (the umask of this process while it binds).
    let old = unsafe { libc::umask(0o177) };
    let listener =
        UnixListener::bind(socket).with_context(|| format!("listen on {}", socket.display()));
    unsafe { libc::umask(old) };
    let listener = listener?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    let backend = Lvm {
        bin_dir: PathBuf::from(&config.lvm_bin_dir),
        node_dir: socket
            .parent()
            .unwrap_or(Path::new("/run/cloudhv"))
            .join("volume-helper-nodes"),
    };
    let policy = Policy {
        vg: config.volume_block_vg.clone(),
        max_mib: config.volume_block_max_mib,
    };
    info!(
        "volume helper on {} for the volume group {} (volumes up to {} MiB)",
        socket.display(),
        policy.vg,
        policy.max_mib
    );
    serve(listener, &backend, &policy)
}

// ---- the real LVM ---------------------------------------------------------------------------------------------------------------------------

/// LVM by its tools.
pub struct Lvm {
    /// Where `lvs`, `lvcreate`, `lvchange`, `lvextend` and `blkdiscard` are.
    pub bin_dir: PathBuf,
    /// Where a node is made for a volume that has none in `/dev/mapper` (no udev, or not yet).
    pub node_dir: PathBuf,
}

const LV_FIELDS: &str = "vg_name,lv_name,lv_size,lv_tags,lv_kernel_major,lv_kernel_minor";

impl Lvm {
    fn run(&self, tool: &str, args: &[&str], timeout: Duration) -> Result<String> {
        let mut cmd = Command::new(self.bin_dir.join(tool));
        cmd.args(args)
            .env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .env("LVM_SUPPRESS_FD_WARNINGS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().with_context(|| format!("run {tool}"))?;
        let start = Instant::now();
        // The output is read after the process ends: these tools write a line or two.
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break s;
            }
            if start.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("{tool} took longer than {}s", timeout.as_secs());
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut out = String::new();
        let mut err = String::new();
        if let Some(mut o) = child.stdout.take() {
            let _ = o.read_to_string(&mut out);
        }
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut err);
        }
        if !status.success() {
            anyhow::bail!(
                "{tool} failed ({status}): {}",
                err.chars().take(300).collect::<String>().trim()
            );
        }
        Ok(out)
    }

    fn lvs(&self, extra: &[&str]) -> Result<Vec<LvInfo>> {
        let mut args = vec![
            "--noheadings",
            "--nosuffix",
            "--units",
            "b",
            "--separator",
            "|",
            "-o",
            LV_FIELDS,
        ];
        args.extend_from_slice(extra);
        let out = self.run("lvs", &args, Duration::from_secs(60))?;
        out.lines()
            .filter(|l| !l.trim().is_empty())
            .map(parse_lvs_line)
            .collect()
    }

    fn node_of(&self, info: &LvInfo) -> Result<PathBuf> {
        let name = dm_name(&info.vg, &info.lv).map_err(|e| anyhow::anyhow!(e))?;
        let mapper = Path::new("/dev/mapper").join(&name);
        if mapper.exists() {
            return Ok(mapper);
        }
        // udev has not made it (or there is none): this process makes its own node.
        std::fs::create_dir_all(&self.node_dir)?;
        std::fs::set_permissions(&self.node_dir, std::fs::Permissions::from_mode(0o700))?;
        let node = self.node_dir.join(&name);
        let _ = std::fs::remove_file(&node);
        let c = std::ffi::CString::new(node.as_os_str().as_encoded_bytes())?;
        if unsafe {
            libc::mknod(
                c.as_ptr(),
                libc::S_IFBLK | 0o600,
                libc::makedev(info.major, info.minor),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error()).context("make a node for the volume");
        }
        Ok(node)
    }
}

/// `vg|lv| 8589934592|tag1,tag2|252|3` (sizes in bytes, `-1` for the numbers of a volume that is not active).
fn parse_lvs_line(line: &str) -> Result<LvInfo> {
    let f: Vec<&str> = line.trim().split('|').collect();
    if f.len() != 6 {
        anyhow::bail!("lvs printed {line:?}");
    }
    let num = |s: &str| -> Result<i64> { s.trim().parse::<i64>().context("a number of lvs") };
    let bytes = num(f[2])?;
    let (major, minor) = (num(f[4])?, num(f[5])?);
    Ok(LvInfo {
        vg: f[0].trim().to_string(),
        lv: f[1].trim().to_string(),
        size_mib: u64::try_from(bytes).unwrap_or(0) / (1024 * 1024),
        tags: f[3]
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        major: u32::try_from(major).unwrap_or(0),
        minor: u32::try_from(minor).unwrap_or(0),
    })
}

impl Backend for Lvm {
    fn find(&self, vg: &str, lv: &str) -> Result<Option<LvInfo>> {
        let target = format!("{vg}/{lv}");
        match self.lvs(&[&target]) {
            Ok(mut v) => Ok(v.pop()),
            // `lvs` of a name that is not there exits with 5.
            Err(e) if format!("{e:#}").contains("Failed to find logical volume") => Ok(None),
            Err(e) if format!("{e:#}").contains("not found") => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn find_dev(&self, major: u32, minor: u32) -> Result<Option<LvInfo>> {
        let select = format!("lv_kernel_major={major} && lv_kernel_minor={minor}");
        Ok(self
            .lvs(&["--select", &select])?
            .into_iter()
            .find(|i| i.major == major && i.minor == minor))
    }

    fn create(&self, vg: &str, lv: &str, size_mib: u64, tag: &str) -> Result<()> {
        let size = format!("{size_mib}m");
        self.run(
            "lvcreate",
            &[
                "--yes", "--zero", "n", "--addtag", tag, "-L", &size, "-n", lv, vg,
            ],
            Duration::from_secs(120),
        )
        .map(|_| ())
    }

    fn extend(&self, vg: &str, lv: &str, size_mib: u64) -> Result<()> {
        let size = format!("{size_mib}m");
        let target = format!("{vg}/{lv}");
        self.run(
            "lvextend",
            &["--yes", "-L", &size, &target],
            Duration::from_secs(120),
        )
        .map(|_| ())
    }

    fn add_tag(&self, vg: &str, lv: &str, tag: &str) -> Result<()> {
        let target = format!("{vg}/{lv}");
        self.run(
            "lvchange",
            &["--addtag", tag, &target],
            Duration::from_secs(60),
        )
        .map(|_| ())
    }

    fn del_tag(&self, vg: &str, lv: &str, tag: &str) -> Result<()> {
        let target = format!("{vg}/{lv}");
        self.run(
            "lvchange",
            &["--deltag", tag, &target],
            Duration::from_secs(60),
        )
        .map(|_| ())
    }

    fn activate(&self, vg: &str, lv: &str) -> Result<()> {
        let target = format!("{vg}/{lv}");
        self.run("lvchange", &["-ay", &target], Duration::from_secs(60))
            .map(|_| ())
    }

    fn zero(&self, info: &LvInfo, from_mib: u64) -> Result<()> {
        if from_mib >= info.size_mib {
            return Ok(());
        }
        let node = self.node_of(info)?;
        let path = node.to_string_lossy().into_owned();
        let timeout = Duration::from_secs((300 + (info.size_mib - from_mib) / 20).min(7200));
        let offset = (from_mib * 1024 * 1024).to_string();
        let length = ((info.size_mib - from_mib) * 1024 * 1024).to_string();
        // BLKZEROOUT: the kernel asks the device to zero, or writes the zeros itself.
        let z = self.run(
            "blkdiscard",
            &["--zeroout", "--offset", &offset, "--length", &length, &path],
            timeout,
        );
        if let Err(e) = z {
            warn!("blkdiscard --zeroout of {path} failed ({e:#}): writing zeros with dd");
            let of = format!("of={path}");
            let seek = format!("seek={from_mib}");
            self.run(
                "dd",
                &[
                    "if=/dev/zero",
                    &of,
                    "bs=1M",
                    &seek,
                    "oflag=direct",
                    "conv=fsync",
                ],
                timeout,
            )
            .or_else(|e| {
                // dd ends with an error when it reaches the end of the device: that is the whole volume written.
                if format!("{e:#}").contains("No space left on device") {
                    Ok(String::new())
                } else {
                    Err(e)
                }
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A fake LVM that remembers what was done and in what order.
    #[derive(Default)]
    struct Fake {
        lvs: RefCell<Vec<LvInfo>>,
        log: RefCell<Vec<String>>,
        /// `zero` fails
        zero_fails: bool,
    }

    impl Backend for Fake {
        fn find(&self, vg: &str, lv: &str) -> Result<Option<LvInfo>> {
            Ok(self
                .lvs
                .borrow()
                .iter()
                .find(|i| i.vg == vg && i.lv == lv)
                .cloned())
        }
        fn find_dev(&self, major: u32, minor: u32) -> Result<Option<LvInfo>> {
            Ok(self
                .lvs
                .borrow()
                .iter()
                .find(|i| i.major == major && i.minor == minor)
                .cloned())
        }
        fn create(&self, vg: &str, lv: &str, size_mib: u64, tag: &str) -> Result<()> {
            self.log
                .borrow_mut()
                .push(format!("create {lv} {size_mib} {tag}"));
            let n = self.lvs.borrow().len() as u32;
            self.lvs.borrow_mut().push(LvInfo {
                vg: vg.into(),
                lv: lv.into(),
                size_mib,
                tags: vec![tag.into()],
                major: 252,
                minor: 10 + n,
            });
            Ok(())
        }
        fn extend(&self, _vg: &str, lv: &str, size_mib: u64) -> Result<()> {
            self.log
                .borrow_mut()
                .push(format!("extend {lv} {size_mib}"));
            for i in self.lvs.borrow_mut().iter_mut().filter(|i| i.lv == lv) {
                i.size_mib = size_mib;
            }
            Ok(())
        }
        fn add_tag(&self, _vg: &str, lv: &str, tag: &str) -> Result<()> {
            self.log.borrow_mut().push(format!("tag {lv} {tag}"));
            for i in self.lvs.borrow_mut().iter_mut().filter(|i| i.lv == lv) {
                i.tags.push(tag.into());
            }
            Ok(())
        }
        fn del_tag(&self, _vg: &str, lv: &str, tag: &str) -> Result<()> {
            self.log.borrow_mut().push(format!("untag {lv} {tag}"));
            for i in self.lvs.borrow_mut().iter_mut().filter(|i| i.lv == lv) {
                i.tags.retain(|t| t != tag);
            }
            Ok(())
        }
        fn activate(&self, _vg: &str, lv: &str) -> Result<()> {
            self.log.borrow_mut().push(format!("activate {lv}"));
            Ok(())
        }
        fn zero(&self, info: &LvInfo, from_mib: u64) -> Result<()> {
            self.log
                .borrow_mut()
                .push(format!("zero {} from {from_mib}", info.lv));
            if self.zero_fails {
                anyhow::bail!("no space");
            }
            Ok(())
        }
    }

    fn policy() -> Policy {
        Policy {
            vg: "vg".into(),
            max_mib: 10_000,
        }
    }

    fn other(lv: &str, tags: &[&str]) -> LvInfo {
        LvInfo {
            vg: "vg".into(),
            lv: lv.into(),
            size_mib: 100,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            major: 252,
            minor: 99,
        }
    }

    #[test]
    fn the_name_of_a_volume_is_derived_and_two_pairs_never_meet() {
        assert_eq!(lv_name("p-abc-db", "data").unwrap(), "pv-p-abc-db_data");
        assert_ne!(lv_name("a-b", "c").unwrap(), lv_name("a", "b-c").unwrap());
        for bad in [
            "",
            "-a",
            "a-",
            "A",
            "a_b",
            "a.b",
            "a/b",
            "a b",
            &"a".repeat(64),
        ] {
            assert!(lv_name(bad, "data").is_err(), "{bad:?}");
        }
        for bad in ["", "-a", "a-", "A", "a_b", "../x", &"a".repeat(33)] {
            assert!(lv_name("p-x-db", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_device_mapper_name_writes_each_dash_twice_and_is_length_checked() {
        assert_eq!(
            dm_name("vg", "pv-p-abc-db_data").unwrap(),
            "vg-pv--p--abc--db_data"
        );
        assert_eq!(dm_name("my-vg", "pv-x_y").unwrap(), "my--vg-pv--x_y");
        let ns = "a-".repeat(31) + "a";
        let lv = lv_name(&ns, &("b-".repeat(15) + "b")).unwrap();
        assert!(dm_name("vg", &lv).is_err(), "too long for the kernel");
    }

    #[test]
    fn a_new_volume_is_made_with_its_owner_tag_zeroed_and_only_then_ready() {
        let f = Fake::default();
        let e = ensure(&f, &policy(), "p-abc-db", "data", 8192).unwrap();
        assert!(e.created && !e.extended);
        assert!(e.info.has_tag(READY_TAG));
        assert_eq!(e.info.namespaces(), vec!["p-abc-db"]);
        assert_eq!(
            *f.log.borrow(),
            vec![
                "create pv-p-abc-db_data 8192 cloudhv-ns=p-abc-db",
                "activate pv-p-abc-db_data",
                "zero pv-p-abc-db_data from 0",
                "tag pv-p-abc-db_data cloudhv-ready",
            ]
        );
    }

    #[test]
    fn a_volume_that_is_ready_is_left_as_it_is() {
        let f = Fake::default();
        ensure(&f, &policy(), "p-abc-db", "data", 8192).unwrap();
        f.log.borrow_mut().clear();
        let e = ensure(&f, &policy(), "p-abc-db", "data", 8192).unwrap();
        assert!(!e.created && !e.extended);
        assert_eq!(
            *f.log.borrow(),
            vec!["activate pv-p-abc-db_data"],
            "no zero, no format"
        );
        // A smaller size does not shrink it.
        let e = ensure(&f, &policy(), "p-abc-db", "data", 1024).unwrap();
        assert_eq!(e.info.size_mib, 8192);
    }

    #[test]
    fn a_creation_that_did_not_finish_is_zeroed_again_before_it_is_ready() {
        let f = Fake::default();
        // A crash after lvcreate: the owner tag, no ready tag.
        f.lvs.borrow_mut().push(LvInfo {
            major: 252,
            minor: 5,
            ..other("pv-p-abc-db_data", &["cloudhv-ns=p-abc-db"])
        });
        let e = ensure(&f, &policy(), "p-abc-db", "data", 100).unwrap();
        assert!(!e.created);
        assert_eq!(
            *f.log.borrow(),
            vec![
                "activate pv-p-abc-db_data",
                "zero pv-p-abc-db_data from 0",
                "tag pv-p-abc-db_data cloudhv-ready"
            ]
        );
    }

    #[test]
    fn a_zeroing_that_fails_never_makes_the_volume_ready() {
        let f = Fake {
            zero_fails: true,
            ..Default::default()
        };
        assert!(ensure(&f, &policy(), "p-abc-db", "data", 100).is_err());
        assert!(!f.lvs.borrow()[0].has_tag(READY_TAG));
        // And the next request tries again, and does not hand it out.
        assert!(ensure(&f, &policy(), "p-abc-db", "data", 100).is_err());
    }

    #[test]
    fn a_volume_of_another_namespace_or_without_an_owner_is_never_taken() {
        let f = Fake::default();
        f.lvs.borrow_mut().push(other(
            "pv-p-abc-db_data",
            &["cloudhv-ns=p-zzz-db", READY_TAG],
        ));
        let e = ensure(&f, &policy(), "p-abc-db", "data", 100).unwrap_err();
        assert!(format!("{e:#}").contains("another namespace"), "{e:#}");
        let g = Fake::default();
        g.lvs
            .borrow_mut()
            .push(other("pv-p-abc-db_data", &[READY_TAG]));
        let e = ensure(&g, &policy(), "p-abc-db", "data", 100).unwrap_err();
        assert!(format!("{e:#}").contains("no owner tag"), "{e:#}");
        assert!(
            f.log.borrow().is_empty() && g.log.borrow().is_empty(),
            "nothing was done to them"
        );
        // Two owner tags is not one owner.
        let h = Fake::default();
        h.lvs.borrow_mut().push(other(
            "pv-p-abc-db_data",
            &["cloudhv-ns=p-abc-db", "cloudhv-ns=p-zzz-db", READY_TAG],
        ));
        assert!(ensure(&h, &policy(), "p-abc-db", "data", 100).is_err());
    }

    #[test]
    fn the_size_is_capped_for_a_new_volume_and_for_an_extension() {
        let f = Fake::default();
        assert!(ensure(&f, &policy(), "p-abc-db", "data", 10_001).is_err());
        assert!(f.log.borrow().is_empty());
        assert!(ensure(&f, &policy(), "p-abc-db", "data", 0).is_err());
        let none = Policy {
            vg: "vg".into(),
            max_mib: 0,
        };
        assert!(
            ensure(&f, &none, "p-abc-db", "data", 1).is_err(),
            "0 refuses every creation"
        );
        ensure(&f, &policy(), "p-abc-db", "data", 5000).unwrap();
        assert!(
            ensure(&f, &policy(), "p-abc-db", "data", 10_001).is_err(),
            "no extension over the cap"
        );
    }

    #[test]
    fn a_larger_size_extends_a_ready_volume() {
        let f = Fake::default();
        ensure(&f, &policy(), "p-abc-db", "data", 5000).unwrap();
        f.log.borrow_mut().clear();
        let e = ensure(&f, &policy(), "p-abc-db", "data", 8000).unwrap();
        assert!(e.extended && e.info.size_mib == 8000);
        // The new part is zeroed, and a crash in between leaves a tag that finishes it (below).
        assert_eq!(
            *f.log.borrow(),
            vec![
                "activate pv-p-abc-db_data",
                "tag pv-p-abc-db_data cloudhv-extending=5000",
                "extend pv-p-abc-db_data 8000",
                "zero pv-p-abc-db_data from 5000",
                "untag pv-p-abc-db_data cloudhv-extending=5000",
            ]
        );
        assert!(e.info.extending_from().is_none());
    }

    #[test]
    fn an_extension_that_did_not_end_zeroes_the_new_part_and_never_the_data() {
        let f = Fake::default();
        ensure(&f, &policy(), "p-abc-db", "data", 5000).unwrap();
        // A crash after lvextend, before the zeroing: the volume is 8000 now, the tag says where the data ended.
        {
            let mut v = f.lvs.borrow_mut();
            v[0].size_mib = 8000;
            v[0].tags.push("cloudhv-extending=5000".into());
        }
        f.log.borrow_mut().clear();
        let e = ensure(&f, &policy(), "p-abc-db", "data", 8000).unwrap();
        assert!(!e.extended);
        assert_eq!(
            *f.log.borrow(),
            vec![
                "activate pv-p-abc-db_data",
                "zero pv-p-abc-db_data from 5000",
                "untag pv-p-abc-db_data cloudhv-extending=5000",
            ],
            "from 5000, not from 0"
        );
        // A crash after the tag, before lvextend: nothing was added, nothing to zero.
        {
            let mut v = f.lvs.borrow_mut();
            v[0].tags.push("cloudhv-extending=8000".into());
        }
        f.log.borrow_mut().clear();
        ensure(&f, &policy(), "p-abc-db", "data", 8000).unwrap();
        assert!(f
            .log
            .borrow()
            .iter()
            .all(|l| !l.starts_with("zero") || l.ends_with("from 8000")));
    }

    #[test]
    fn the_helper_says_what_it_knows_only_of_its_own_volume_group() {
        let f = Fake::default();
        ensure(&f, &policy(), "p-abc-db", "data", 100).unwrap();
        let minor = f.lvs.borrow()[0].minor;
        let r = handle(&f, &policy(), &Request::Tags { major: 252, minor });
        assert!(r.ok && r.info.unwrap().lv == "pv-p-abc-db_data");
        let r = handle(
            &f,
            &policy(),
            &Request::Tags {
                major: 252,
                minor: 77,
            },
        );
        assert!(r.ok && r.info.is_none());
        let elsewhere = Policy {
            vg: "other".into(),
            max_mib: 1,
        };
        let r = handle(&f, &elsewhere, &Request::Tags { major: 252, minor });
        assert!(
            r.ok && r.info.is_none(),
            "a volume of another group is not described"
        );
    }

    #[test]
    fn a_request_is_one_line_of_json_and_a_bad_one_is_an_error() {
        let r: Request = serde_json::from_str(
            r#"{"op":"ensure","namespace":"p-x-db","volume":"data","size_mib":8192}"#,
        )
        .unwrap();
        assert!(matches!(r, Request::Ensure { size_mib: 8192, .. }));
        let r: Request = serde_json::from_str(r#"{"op":"tags","major":252,"minor":3}"#).unwrap();
        assert!(matches!(
            r,
            Request::Tags {
                major: 252,
                minor: 3
            }
        ));
        assert!(serde_json::from_str::<Request>(r#"{"op":"lvremove","name":"x"}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"op":"ensure","namespace":"x"}"#).is_err());
    }

    #[test]
    fn the_lvs_output_is_read() {
        let i =
            parse_lvs_line("  vg|pv-p-x-db_data|8589934592|cloudhv-ns=p-x-db,cloudhv-ready|252|3")
                .unwrap();
        assert_eq!((i.size_mib, i.major, i.minor), (8192, 252, 3));
        assert!(i.has_tag(READY_TAG) && i.namespaces() == vec!["p-x-db"]);
        let off = parse_lvs_line("vg|x|1048576||-1|-1").unwrap();
        assert_eq!((off.major, off.minor, off.tags.len()), (0, 0, 0));
        assert!(parse_lvs_line("vg|x|1").is_err());
    }
}
