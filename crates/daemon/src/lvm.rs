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
//!
//! **Thin volumes** (a branch is an empty database, so the disk may be over-committed; the main database keeps a thick volume): `ensure` with
//! `thin: true` makes the logical volume in the thin pool `volume_thin_pool` of the same volume group (`lvcreate -T vg/pool -V <size>`; the
//! helper never makes the pool: Ansible does). The rules above stay: the derived name, the owner tag (and `cloudhv-thin`), ready only at the
//! end, never shrunk, never deleted. What differs:
//!
//! - **No zeroing.** A block of a thin volume that has not been written is not backed by anything and reads as zeros, so a new thin volume cannot
//!   show another customer's data. That holds only if the pool zeroes the blocks it provisions (`lvcreate --zero y`, the default): with zeroing off
//!   a first write of less than a chunk would leave the rest of the chunk as it was. The helper refuses a pool that does not zero.
//! - **A full pool breaks every thin volume in it**, so creation (and extension) stops before: with the pool's data or metadata use over
//!   `volume_thin_max_percent` (90 by default) the request is refused, and so is a pool that is not there.
//! - **A volume is thick or thin for good**: a request that says thin for a volume that exists thick (or the reverse) is refused.

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
/// The tag of a thin volume the helper made (the truth is `LvInfo::is_thin`, which is LVM's; the tag is for the operator reading `lvs`).
pub const THIN_TAG: &str = "cloudhv-thin";
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
    /// The thin pool the volume is in (`lvs -o pool_lv`); empty for a thick volume. This is what LVM says, not a tag.
    #[serde(default)]
    pub pool: String,
}

impl LvInfo {
    pub fn has_tag(&self, t: &str) -> bool {
        self.tags.iter().any(|x| x == t)
    }
    /// Whether the volume is a thin volume (it is in a thin pool).
    pub fn is_thin(&self) -> bool {
        !self.pool.is_empty()
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
    /// `lvcreate -T vg/pool -V` of a thin volume of `size_mib` (virtual size) that carries `tags` from the start.
    fn create_thin(
        &self,
        vg: &str,
        pool: &str,
        lv: &str,
        size_mib: u64,
        tags: &[&str],
    ) -> Result<()>;
    /// The state of the thin pool `vg/pool`: `None` if there is no thin pool of that name.
    fn pool_state(&self, vg: &str, pool: &str) -> Result<Option<PoolState>>;
    fn extend(&self, vg: &str, lv: &str, size_mib: u64) -> Result<()>;
    fn add_tag(&self, vg: &str, lv: &str, tag: &str) -> Result<()>;
    fn del_tag(&self, vg: &str, lv: &str, tag: &str) -> Result<()>;
    /// Make the volume active (it is, after `create`; after a reboot it may not be).
    fn activate(&self, vg: &str, lv: &str) -> Result<()>;
    /// Write zeros over the volume from `from_mib` to its end.
    fn zero(&self, info: &LvInfo, from_mib: u64) -> Result<()>;
}

/// What LVM says of a thin pool.
#[derive(Debug, Clone, PartialEq)]
pub struct PoolState {
    /// Percent of the pool's data space in use.
    pub data_percent: f64,
    /// Percent of the pool's metadata space in use.
    pub metadata_percent: f64,
    /// Whether the pool zeroes the blocks it provisions (`lvcreate --zero y`).
    pub zeroing: bool,
}

/// What the helper may do.
#[derive(Debug, Clone)]
pub struct Policy {
    pub vg: String,
    /// The largest a volume may be made or extended to. 0: none may.
    pub max_mib: u64,
    /// The thin pool (in `vg`) that thin volumes are made in. Empty: thin volumes are refused.
    pub thin_pool: String,
    /// The largest a THIN volume may be (its virtual size). 0: the same as `max_mib`.
    pub thin_max_mib: u64,
    /// A thin volume is not made or extended when the pool's data or metadata use is over this percent.
    pub thin_max_percent: u8,
}

impl Policy {
    /// A policy with no thin pool (thick volumes only).
    #[cfg(test)]
    pub fn thick_only(vg: &str, max_mib: u64) -> Policy {
        Policy {
            vg: vg.to_string(),
            max_mib,
            thin_pool: String::new(),
            thin_max_mib: 0,
            thin_max_percent: 90,
        }
    }
}

/// The pool must be there, zero what it provisions, and not be nearly full.
fn check_pool(b: &dyn Backend, p: &Policy) -> Result<()> {
    if p.thin_pool.is_empty() {
        anyhow::bail!("thin volumes are off (volume_thin_pool is empty)");
    }
    let Some(st) = b
        .pool_state(&p.vg, &p.thin_pool)
        .context("look at the thin pool")?
    else {
        anyhow::bail!(
            "the thin pool {}/{} is not there (volume_thin_pool)",
            p.vg,
            p.thin_pool
        );
    };
    if !st.zeroing {
        anyhow::bail!(
            "the thin pool {}/{} does not zero the blocks it provisions (made with --zero n): a first write of less than a chunk would show what the block held before",
            p.vg,
            p.thin_pool
        );
    }
    let limit = f64::from(p.thin_max_percent);
    if st.data_percent > limit || st.metadata_percent > limit {
        anyhow::bail!(
            "the thin pool {}/{} is {:.1}% full in data and {:.1}% in metadata (the limit is {}%, volume_thin_max_percent): a full pool breaks every thin volume, so no thin volume is made or extended",
            p.vg,
            p.thin_pool,
            st.data_percent,
            st.metadata_percent,
            p.thin_max_percent
        );
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ensured {
    pub info: LvInfo,
    pub created: bool,
    pub extended: bool,
}

/// The logical volume of `namespace`'s `volume`, ready to be given to a VM: made and zeroed if it is not there, finished if a creation did not end,
/// extended if `size_mib` is more than it is. An error is a refusal (a volume of another namespace or without an owner, a size over the cap).
/// `thin`: the volume is (or must be) a thin volume of the pool of the policy: no zeroing, and refused when the pool is nearly full.
pub fn ensure(
    b: &dyn Backend,
    p: &Policy,
    namespace: &str,
    volume: &str,
    size_mib: u64,
    thin: bool,
) -> Result<Ensured> {
    let lv = lv_name(namespace, volume).map_err(|e| anyhow::anyhow!(e))?;
    dm_name(&p.vg, &lv).map_err(|e| anyhow::anyhow!(e))?;
    if size_mib == 0 {
        anyhow::bail!("a volume of 0 MiB");
    }
    if thin {
        if p.thin_pool.is_empty() {
            anyhow::bail!("thin volumes are off (volume_thin_pool is empty)");
        }
        let cap = if p.thin_max_mib > 0 {
            p.thin_max_mib
        } else {
            p.max_mib
        };
        if size_mib > cap {
            anyhow::bail!(
                "a thin volume of {size_mib} MiB is more than volume_thin_max_virtual_mib ({cap})"
            );
        }
    } else if size_mib > p.max_mib {
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
            if i.is_thin() != thin {
                anyhow::bail!(
                    "the logical volume {}/{lv} is a {} volume and was asked for as a {} one: a volume is thick or thin for good",
                    p.vg,
                    if i.is_thin() { "thin" } else { "thick" },
                    if thin { "thin" } else { "thick" }
                );
            }
            i
        }
        None => {
            if thin {
                // A full pool breaks every thin volume: not one more is made when it is nearly full.
                check_pool(b, p)?;
                b.create_thin(&p.vg, &p.thin_pool, &lv, size_mib, &[&tag, THIN_TAG])
                    .context("make the thin volume")?;
            } else {
                b.create(&p.vg, &lv, size_mib, &tag)
                    .context("make the logical volume")?;
            }
            created = true;
            b.find(&p.vg, &lv)
                .context("look for the volume just made")?
                .context("the logical volume just made is not there")?
        }
    };
    b.activate(&p.vg, &lv).context("activate the volume")?;
    if !info.has_tag(READY_TAG) {
        // New, or a creation that did not finish: whatever is on a THICK volume is not this namespace's. Zero it all, and only then say it is ready.
        // A thin volume needs no zeroing: a block that was never written is not backed by anything and reads as zeros (and the pool zeroes the blocks
        // it provisions, which `check_pool` insists on), and nothing has been given the volume before it is ready.
        if !thin {
            b.zero(&info, 0).context("zero the volume")?;
        }
        b.add_tag(&p.vg, &lv, READY_TAG)
            .context("mark the volume ready")?;
    } else {
        // An extension that did not end: the part from where the volume was is not zeroed (never the data before it).
        if let Some(from) = info.extending_from() {
            if !thin {
                b.zero(&info, from.min(info.size_mib))
                    .context("zero the part added by an earlier extension")?;
            }
            b.del_tag(&p.vg, &lv, &format!("{EXTENDING_KEY}{from}"))
                .context("finish the earlier extension")?;
            info = b.find(&p.vg, &lv)?.context("the volume is gone")?;
        }
        if size_mib > info.size_mib {
            if thin {
                // A larger virtual size is more over-commitment: not when the pool is nearly full.
                check_pool(b, p)?;
            }
            let old = info.size_mib;
            let mark = format!("{EXTENDING_KEY}{old}");
            b.add_tag(&p.vg, &lv, &mark).context("mark the extension")?;
            b.extend(&p.vg, &lv, size_mib)
                .context("extend the volume")?;
            info = b.find(&p.vg, &lv)?.context("the volume is gone")?;
            if !thin {
                b.zero(&info, old).context("zero the added part")?;
            }
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
        /// A thin volume (default: thick).
        #[serde(default)]
        thin: bool,
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
            thin,
        } => match ensure(b, p, namespace, volume, *size_mib, *thin) {
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
        // A thin volume is not zeroed and answers at once, but the daemon does not know which a request is: the longer wait is only a ceiling.
        Duration::from_secs((120 + size_mib / 50).min(3600))
    }

    pub fn ensure(
        &self,
        namespace: &str,
        volume: &str,
        size_mib: u64,
        thin: bool,
    ) -> Result<Ensured> {
        let r = self.call(
            &Request::Ensure {
                namespace: namespace.to_string(),
                volume: volume.to_string(),
                size_mib,
                thin,
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
        thin_pool: config.volume_thin_pool.clone(),
        thin_max_mib: config.volume_thin_max_virtual_mib,
        thin_max_percent: config.volume_thin_max_percent,
    };
    info!(
        "volume helper on {} for the volume group {} (volumes up to {} MiB; thin pool {:?}, thin volumes up to {} MiB, refused over {}% of the pool)",
        socket.display(),
        policy.vg,
        policy.max_mib,
        policy.thin_pool,
        if policy.thin_max_mib > 0 { policy.thin_max_mib } else { policy.max_mib },
        policy.thin_max_percent
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

const LV_FIELDS: &str = "vg_name,lv_name,lv_size,lv_tags,lv_kernel_major,lv_kernel_minor,pool_lv";

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

/// `vg|lv| 8589934592|tag1,tag2|252|3|pool` (sizes in bytes, `-1` for the numbers of a volume that is not active, the pool empty for a thick volume).
fn parse_lvs_line(line: &str) -> Result<LvInfo> {
    let f: Vec<&str> = line.trim().split('|').collect();
    if f.len() != 7 {
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
        pool: f[6].trim().to_string(),
    })
}

/// `thin-pool attr|data %|metadata %|zero`: `t` is the first character of the attributes of a thin pool; the percents are empty for a pool that is not active.
fn parse_pool_line(line: &str) -> Result<Option<PoolState>> {
    let f: Vec<&str> = line.trim().split('|').collect();
    if f.len() != 4 {
        anyhow::bail!("lvs printed {line:?} for a thin pool");
    }
    if !f[0].trim().starts_with('t') {
        return Ok(None);
    }
    let pct = |s: &str| -> Result<f64> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(0.0);
        }
        s.parse::<f64>().context("a percent of lvs")
    };
    Ok(Some(PoolState {
        data_percent: pct(f[1])?,
        metadata_percent: pct(f[2])?,
        zeroing: f[3].trim().eq_ignore_ascii_case("zero"),
    }))
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

    fn create_thin(
        &self,
        vg: &str,
        pool: &str,
        lv: &str,
        size_mib: u64,
        tags: &[&str],
    ) -> Result<()> {
        let size = format!("{size_mib}m");
        let pool_path = format!("{vg}/{pool}");
        let mut args: Vec<&str> = vec!["--yes"];
        for t in tags {
            args.push("--addtag");
            args.push(t);
        }
        args.extend_from_slice(&["-T", &pool_path, "-V", &size, "-n", lv]);
        self.run("lvcreate", &args, Duration::from_secs(120))
            .map(|_| ())
    }

    fn pool_state(&self, vg: &str, pool: &str) -> Result<Option<PoolState>> {
        let target = format!("{vg}/{pool}");
        let out = self.run(
            "lvs",
            &[
                "--noheadings",
                "--separator",
                "|",
                "-o",
                "lv_attr,data_percent,metadata_percent,zero",
                &target,
            ],
            Duration::from_secs(60),
        );
        match out {
            Ok(text) => match text.lines().find(|l| !l.trim().is_empty()) {
                Some(l) => parse_pool_line(l),
                None => Ok(None),
            },
            Err(e)
                if format!("{e:#}").contains("Failed to find logical volume")
                    || format!("{e:#}").contains("not found") =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
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
        /// The thin pool, if there is one.
        pool: RefCell<Option<PoolState>>,
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
                pool: String::new(),
            });
            Ok(())
        }
        fn create_thin(
            &self,
            vg: &str,
            pool: &str,
            lv: &str,
            size_mib: u64,
            tags: &[&str],
        ) -> Result<()> {
            self.log.borrow_mut().push(format!(
                "create-thin {lv} {size_mib} in {pool} {}",
                tags.join(",")
            ));
            let n = self.lvs.borrow().len() as u32;
            self.lvs.borrow_mut().push(LvInfo {
                vg: vg.into(),
                lv: lv.into(),
                size_mib,
                tags: tags.iter().map(|t| t.to_string()).collect(),
                major: 252,
                minor: 10 + n,
                pool: pool.into(),
            });
            Ok(())
        }
        fn pool_state(&self, _vg: &str, _pool: &str) -> Result<Option<PoolState>> {
            Ok(self.pool.borrow().clone())
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
        Policy::thick_only("vg", 10_000)
    }

    /// A policy with a thin pool `branches`, volumes up to 4000 MiB virtual, refused over 90%.
    fn thin_policy() -> Policy {
        Policy {
            thin_pool: "branches".into(),
            thin_max_mib: 4000,
            ..policy()
        }
    }

    fn pool(data: f64, meta: f64, zeroing: bool) -> RefCell<Option<PoolState>> {
        RefCell::new(Some(PoolState {
            data_percent: data,
            metadata_percent: meta,
            zeroing,
        }))
    }

    fn other(lv: &str, tags: &[&str]) -> LvInfo {
        LvInfo {
            vg: "vg".into(),
            lv: lv.into(),
            size_mib: 100,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            major: 252,
            minor: 99,
            pool: String::new(),
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
        let e = ensure(&f, &policy(), "p-abc-db", "data", 8192, false).unwrap();
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
        ensure(&f, &policy(), "p-abc-db", "data", 8192, false).unwrap();
        f.log.borrow_mut().clear();
        let e = ensure(&f, &policy(), "p-abc-db", "data", 8192, false).unwrap();
        assert!(!e.created && !e.extended);
        assert_eq!(
            *f.log.borrow(),
            vec!["activate pv-p-abc-db_data"],
            "no zero, no format"
        );
        // A smaller size does not shrink it.
        let e = ensure(&f, &policy(), "p-abc-db", "data", 1024, false).unwrap();
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
        let e = ensure(&f, &policy(), "p-abc-db", "data", 100, false).unwrap();
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
        assert!(ensure(&f, &policy(), "p-abc-db", "data", 100, false).is_err());
        assert!(!f.lvs.borrow()[0].has_tag(READY_TAG));
        // And the next request tries again, and does not hand it out.
        assert!(ensure(&f, &policy(), "p-abc-db", "data", 100, false).is_err());
    }

    #[test]
    fn a_volume_of_another_namespace_or_without_an_owner_is_never_taken() {
        let f = Fake::default();
        f.lvs.borrow_mut().push(other(
            "pv-p-abc-db_data",
            &["cloudhv-ns=p-zzz-db", READY_TAG],
        ));
        let e = ensure(&f, &policy(), "p-abc-db", "data", 100, false).unwrap_err();
        assert!(format!("{e:#}").contains("another namespace"), "{e:#}");
        let g = Fake::default();
        g.lvs
            .borrow_mut()
            .push(other("pv-p-abc-db_data", &[READY_TAG]));
        let e = ensure(&g, &policy(), "p-abc-db", "data", 100, false).unwrap_err();
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
        assert!(ensure(&h, &policy(), "p-abc-db", "data", 100, false).is_err());
    }

    #[test]
    fn the_size_is_capped_for_a_new_volume_and_for_an_extension() {
        let f = Fake::default();
        assert!(ensure(&f, &policy(), "p-abc-db", "data", 10_001, false).is_err());
        assert!(f.log.borrow().is_empty());
        assert!(ensure(&f, &policy(), "p-abc-db", "data", 0, false).is_err());
        let none = Policy::thick_only("vg", 0);
        assert!(
            ensure(&f, &none, "p-abc-db", "data", 1, false).is_err(),
            "0 refuses every creation"
        );
        ensure(&f, &policy(), "p-abc-db", "data", 5000, false).unwrap();
        assert!(
            ensure(&f, &policy(), "p-abc-db", "data", 10_001, false).is_err(),
            "no extension over the cap"
        );
    }

    #[test]
    fn a_larger_size_extends_a_ready_volume() {
        let f = Fake::default();
        ensure(&f, &policy(), "p-abc-db", "data", 5000, false).unwrap();
        f.log.borrow_mut().clear();
        let e = ensure(&f, &policy(), "p-abc-db", "data", 8000, false).unwrap();
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
        ensure(&f, &policy(), "p-abc-db", "data", 5000, false).unwrap();
        // A crash after lvextend, before the zeroing: the volume is 8000 now, the tag says where the data ended.
        {
            let mut v = f.lvs.borrow_mut();
            v[0].size_mib = 8000;
            v[0].tags.push("cloudhv-extending=5000".into());
        }
        f.log.borrow_mut().clear();
        let e = ensure(&f, &policy(), "p-abc-db", "data", 8000, false).unwrap();
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
        ensure(&f, &policy(), "p-abc-db", "data", 8000, false).unwrap();
        assert!(f
            .log
            .borrow()
            .iter()
            .all(|l| !l.starts_with("zero") || l.ends_with("from 8000")));
    }

    #[test]
    fn the_helper_says_what_it_knows_only_of_its_own_volume_group() {
        let f = Fake::default();
        ensure(&f, &policy(), "p-abc-db", "data", 100, false).unwrap();
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
        let elsewhere = Policy::thick_only("other", 1);
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
        assert!(matches!(
            r,
            Request::Ensure {
                size_mib: 8192,
                thin: false,
                ..
            }
        ));
        let r: Request = serde_json::from_str(
            r#"{"op":"ensure","namespace":"p-x-db","volume":"data","size_mib":512,"thin":true}"#,
        )
        .unwrap();
        assert!(matches!(r, Request::Ensure { thin: true, .. }));
        assert!(serde_json::from_str::<Request>(
            r#"{"op":"ensure","namespace":"p-x-db","volume":"data","size_mib":512,"thin":"yes"}"#
        )
        .is_err());
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
            parse_lvs_line("  vg|pv-p-x-db_data|8589934592|cloudhv-ns=p-x-db,cloudhv-ready|252|3|")
                .unwrap();
        assert_eq!((i.size_mib, i.major, i.minor), (8192, 252, 3));
        assert!(i.has_tag(READY_TAG) && i.namespaces() == vec!["p-x-db"]);
        assert!(!i.is_thin());
        let off = parse_lvs_line("vg|x|1048576||-1|-1|").unwrap();
        assert_eq!((off.major, off.minor, off.tags.len()), (0, 0, 0));
        assert!(parse_lvs_line("vg|x|1").is_err());
        let thin = parse_lvs_line(
            "  vg|pv-p-x-db_data|2147483648|cloudhv-ns=p-x-db,cloudhv-thin,cloudhv-ready|252|7|branches",
        )
        .unwrap();
        assert!(thin.is_thin() && thin.pool == "branches");
    }

    #[test]
    fn the_lvs_output_of_a_thin_pool_is_read() {
        let st = parse_pool_line("  twi-aotz--|12.50|1.20|zero")
            .unwrap()
            .unwrap();
        assert_eq!(st.data_percent, 12.5);
        assert_eq!(st.metadata_percent, 1.2);
        assert!(st.zeroing);
        let no_zero = parse_pool_line("twi-a-tz--|0.00|0.50|").unwrap().unwrap();
        assert!(!no_zero.zeroing);
        // Not a thin pool (a plain volume's attributes begin with `-`): nothing.
        assert!(parse_pool_line("-wi-ao----|||").unwrap().is_none());
        // A pool that is not active has no percents: read as 0, not as a failure.
        assert_eq!(
            parse_pool_line("twi---tz--|||zero")
                .unwrap()
                .unwrap()
                .data_percent,
            0.0
        );
        assert!(parse_pool_line("x|y").is_err());
    }

    // ---- thin volumes ------------------------------------------------------------------------------------------------------------------

    fn thin_fake() -> Fake {
        Fake {
            pool: pool(10.0, 1.0, true),
            ..Default::default()
        }
    }

    #[test]
    fn a_new_thin_volume_is_made_in_the_pool_with_its_tags_and_is_not_zeroed() {
        let f = thin_fake();
        let e = ensure(&f, &thin_policy(), "p-abc-db", "data", 2048, true).unwrap();
        assert!(e.created && !e.extended);
        assert!(e.info.is_thin() && e.info.pool == "branches");
        assert!(e.info.has_tag(READY_TAG) && e.info.has_tag(THIN_TAG));
        assert_eq!(e.info.namespaces(), vec!["p-abc-db"]);
        assert_eq!(
            *f.log.borrow(),
            vec![
                "create-thin pv-p-abc-db_data 2048 in branches cloudhv-ns=p-abc-db,cloudhv-thin",
                "activate pv-p-abc-db_data",
                "tag pv-p-abc-db_data cloudhv-ready",
            ],
            "no zero: a block never written reads as zeros"
        );
        // A second request leaves it as it is.
        f.log.borrow_mut().clear();
        let e = ensure(&f, &thin_policy(), "p-abc-db", "data", 2048, true).unwrap();
        assert!(!e.created && !e.extended);
        assert_eq!(*f.log.borrow(), vec!["activate pv-p-abc-db_data"]);
    }

    #[test]
    fn a_thin_volume_is_refused_without_a_pool_with_a_missing_pool_or_one_that_does_not_zero() {
        // No volume_thin_pool.
        let f = thin_fake();
        let e = ensure(&f, &policy(), "p-abc-db", "data", 100, true).unwrap_err();
        assert!(format!("{e:#}").contains("thin volumes are off"), "{e:#}");
        assert!(f.log.borrow().is_empty());
        // The pool is named but is not there.
        let g = Fake::default();
        let e = ensure(&g, &thin_policy(), "p-abc-db", "data", 100, true).unwrap_err();
        assert!(format!("{e:#}").contains("is not there"), "{e:#}");
        assert!(g.log.borrow().is_empty(), "nothing was made");
        // A pool that does not zero what it provisions would show old data after a small first write.
        let h = Fake {
            pool: pool(1.0, 1.0, false),
            ..Default::default()
        };
        let e = ensure(&h, &thin_policy(), "p-abc-db", "data", 100, true).unwrap_err();
        assert!(format!("{e:#}").contains("does not zero"), "{e:#}");
        assert!(h.log.borrow().is_empty());
    }

    #[test]
    fn a_nearly_full_pool_stops_the_creation_of_thin_volumes() {
        for (data, meta, ok) in [
            (89.9, 1.0, true),
            (90.0, 90.0, true), // the limit itself is allowed: over it is not
            (90.1, 1.0, false),
            (10.0, 90.1, false), // the metadata counts as much as the data
            (100.0, 100.0, false),
        ] {
            let f = Fake {
                pool: pool(data, meta, true),
                ..Default::default()
            };
            let r = ensure(&f, &thin_policy(), "p-abc-db", "data", 100, true);
            assert_eq!(r.is_ok(), ok, "data {data}% metadata {meta}%");
            if !ok {
                let e = format!("{:#}", r.unwrap_err());
                assert!(e.contains("volume_thin_max_percent"), "{e}");
                assert!(f.log.borrow().is_empty(), "nothing was made");
            }
        }
        // The limit is the policy's.
        let strict = Policy {
            thin_max_percent: 50,
            ..thin_policy()
        };
        let f = Fake {
            pool: pool(60.0, 1.0, true),
            ..Default::default()
        };
        assert!(ensure(&f, &strict, "p-abc-db", "data", 100, true).is_err());
    }

    #[test]
    fn the_size_of_a_thin_volume_has_its_own_cap_and_a_thick_one_keeps_the_old_cap() {
        let f = thin_fake();
        assert!(ensure(&f, &thin_policy(), "p-abc-db", "data", 4001, true).is_err());
        assert!(f.log.borrow().is_empty());
        ensure(&f, &thin_policy(), "p-abc-db", "data", 4000, true).unwrap();
        // 0 for thin_max_mib: the cap is volume_block_max_mib.
        let same = Policy {
            thin_max_mib: 0,
            ..thin_policy()
        };
        let g = thin_fake();
        assert!(ensure(&g, &same, "p-abc-db", "data", 10_000, true).is_ok());
        assert!(ensure(&g, &same, "p-abc-dc", "data", 10_001, true).is_err());
        // The thick volume of the same policy may be larger than a thin one.
        assert!(ensure(&g, &thin_policy(), "p-abc-dd", "data", 9000, false).is_ok());
    }

    #[test]
    fn a_volume_is_thick_or_thin_for_good() {
        let f = thin_fake();
        ensure(&f, &thin_policy(), "p-abc-db", "data", 100, false).unwrap();
        f.log.borrow_mut().clear();
        let e = ensure(&f, &thin_policy(), "p-abc-db", "data", 100, true).unwrap_err();
        assert!(format!("{e:#}").contains("thick or thin for good"), "{e:#}");
        assert!(f.log.borrow().is_empty(), "nothing was done to it");
        let g = thin_fake();
        ensure(&g, &thin_policy(), "p-abc-db", "data", 100, true).unwrap();
        g.log.borrow_mut().clear();
        let e = ensure(&g, &thin_policy(), "p-abc-db", "data", 100, false).unwrap_err();
        assert!(format!("{e:#}").contains("is a thin volume"), "{e:#}");
        assert!(g.log.borrow().is_empty());
    }

    #[test]
    fn a_thin_creation_that_did_not_finish_is_made_ready_without_zeroing() {
        let f = thin_fake();
        // A crash after lvcreate: the owner tag, the thin tag, the pool, no ready tag.
        f.lvs.borrow_mut().push(LvInfo {
            major: 252,
            minor: 5,
            pool: "branches".into(),
            ..other("pv-p-abc-db_data", &["cloudhv-ns=p-abc-db", THIN_TAG])
        });
        let e = ensure(&f, &thin_policy(), "p-abc-db", "data", 100, true).unwrap();
        assert!(!e.created && e.info.has_tag(READY_TAG));
        assert_eq!(
            *f.log.borrow(),
            vec![
                "activate pv-p-abc-db_data",
                "tag pv-p-abc-db_data cloudhv-ready"
            ]
        );
    }

    #[test]
    fn a_thin_volume_of_another_namespace_or_without_an_owner_is_never_taken() {
        let f = thin_fake();
        f.lvs.borrow_mut().push(LvInfo {
            pool: "branches".into(),
            ..other("pv-p-abc-db_data", &["cloudhv-ns=p-zzz-db", READY_TAG])
        });
        let e = ensure(&f, &thin_policy(), "p-abc-db", "data", 100, true).unwrap_err();
        assert!(format!("{e:#}").contains("another namespace"), "{e:#}");
        let g = thin_fake();
        g.lvs.borrow_mut().push(LvInfo {
            pool: "branches".into(),
            ..other("pv-p-abc-db_data", &[READY_TAG])
        });
        let e = ensure(&g, &thin_policy(), "p-abc-db", "data", 100, true).unwrap_err();
        assert!(format!("{e:#}").contains("no owner tag"), "{e:#}");
        assert!(f.log.borrow().is_empty() && g.log.borrow().is_empty());
    }

    #[test]
    fn a_thin_volume_is_extended_without_zeroing_and_not_when_the_pool_is_nearly_full() {
        let f = thin_fake();
        ensure(&f, &thin_policy(), "p-abc-db", "data", 1000, true).unwrap();
        f.log.borrow_mut().clear();
        let e = ensure(&f, &thin_policy(), "p-abc-db", "data", 2000, true).unwrap();
        assert!(e.extended && e.info.size_mib == 2000);
        assert_eq!(
            *f.log.borrow(),
            vec![
                "activate pv-p-abc-db_data",
                "tag pv-p-abc-db_data cloudhv-extending=1000",
                "extend pv-p-abc-db_data 2000",
                "untag pv-p-abc-db_data cloudhv-extending=1000",
            ],
            "the new virtual blocks are zeros: no zero"
        );
        // The pool fills up: no more over-commitment.
        *f.pool.borrow_mut() = pool(95.0, 1.0, true).into_inner();
        f.log.borrow_mut().clear();
        let e = ensure(&f, &thin_policy(), "p-abc-db", "data", 3000, true).unwrap_err();
        assert!(
            format!("{e:#}").contains("volume_thin_max_percent"),
            "{e:#}"
        );
        assert_eq!(
            *f.log.borrow(),
            vec!["activate pv-p-abc-db_data"],
            "not extended, not tagged"
        );
        // The same size is no extension and needs no pool: it still answers.
        assert!(ensure(&f, &thin_policy(), "p-abc-db", "data", 2000, true).is_ok());
    }

    #[test]
    fn a_thin_extension_that_did_not_end_is_finished_without_zeroing() {
        let f = thin_fake();
        ensure(&f, &thin_policy(), "p-abc-db", "data", 1000, true).unwrap();
        {
            let mut v = f.lvs.borrow_mut();
            v[0].size_mib = 2000;
            v[0].tags.push("cloudhv-extending=1000".into());
        }
        f.log.borrow_mut().clear();
        let e = ensure(&f, &thin_policy(), "p-abc-db", "data", 2000, true).unwrap();
        assert!(!e.extended);
        assert_eq!(
            *f.log.borrow(),
            vec![
                "activate pv-p-abc-db_data",
                "untag pv-p-abc-db_data cloudhv-extending=1000"
            ]
        );
    }

    #[test]
    fn a_thin_request_is_answered_by_the_helper_and_a_refusal_is_an_error_line() {
        let f = thin_fake();
        let r = handle(
            &f,
            &thin_policy(),
            &Request::Ensure {
                namespace: "p-abc-db".into(),
                volume: "data".into(),
                size_mib: 512,
                thin: true,
            },
        );
        assert!(r.ok && r.created && r.info.unwrap().is_thin());
        let refused = handle(
            &f,
            &policy(),
            &Request::Ensure {
                namespace: "p-abc-dc".into(),
                volume: "data".into(),
                size_mib: 512,
                thin: true,
            },
        );
        assert!(!refused.ok && refused.error.unwrap().contains("thin volumes are off"));
    }
}
