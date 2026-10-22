//! Network namespace helpers for the daemon.
//!
//! Handles TAP device creation in the host netns, moving to the pod netns,
//! and setting up TC redirects for traffic between the veth and TAP.
//!
//! Flow: create TAP in host → CH binds it via vm.add-net → move TAP to pod
//! netns → set up TC redirects (veth↔TAP). CH's open fd remains valid across
//! the namespace move.

use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Context, Result};
use cloudhv_common::netlink::{create_tap, pick_veth, Netlink};

/// What the daemon remembers about a pod's network namespace, so that a container started again in the same pod finds it.
///
/// When a VM gets the pod's network (`activate_tap`) the pod's address is taken off the CNI's veth (`flush_addrs`): the guest has it. The
/// veth is then the only way into the namespace's network and it has no address, so the next start in the same pod (kubelet starts a
/// container again after one failed, or exited) could not find "the interface that has an address" and failed for good with `find veth:
/// not found`, though the pod itself was fine. The address, the gateway and the MAC are remembered from the first start. The key is the
/// namespace itself (device and inode of its file), not its path; and a remembered MAC that no interface of the namespace has is not used.
type NsKey = (u64, u64);

#[derive(Default)]
struct NetMemory {
    map: HashMap<NsKey, (PodNetInfo, String)>,
}

/// More entries than this and the ones whose namespace is gone are dropped (a pod's namespace goes with the pod).
const MEMORY_PRUNE_AT: usize = 256;

impl NetMemory {
    fn get(&self, key: NsKey) -> Option<PodNetInfo> {
        self.map.get(&key).map(|(i, _)| i.clone())
    }

    fn put(&mut self, key: NsKey, info: PodNetInfo, path: &str) {
        if self.map.len() >= MEMORY_PRUNE_AT {
            self.map
                .retain(|_, (_, p)| std::path::Path::new(p).exists());
        }
        self.map.insert(key, (info, path.to_string()));
    }
}

static MEMORY: std::sync::OnceLock<Mutex<NetMemory>> = std::sync::OnceLock::new();

fn memory() -> std::sync::MutexGuard<'static, NetMemory> {
    MEMORY
        .get_or_init(|| Mutex::new(NetMemory::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// The identity of a network namespace: device and inode of its file.
fn ns_key(path: &str) -> Result<NsKey> {
    let fd = open_checked(path)?;
    let st = stat_of(fd.as_raw_fd())?;
    Ok((st.st_dev as u64, st.st_ino as u64))
}

/// Network info discovered from the pod's network namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodNetInfo {
    pub ip_cidr: String,
    pub gateway: String,
    pub mac: String,
}

/// Phase 1: Discover veth info from pod netns, then create a persistent TAP
/// in the host (daemon) namespace. Must be called from a blocking context.
///
/// With `hand_over` (rootless Cloud Hypervisor, which cannot open and enable the device itself) the descriptor is returned too, to be
/// passed to Cloud Hypervisor.
pub fn prepare_tap(
    netns_path: &str,
    tap_name: &str,
    hand_over: bool,
) -> Result<(PodNetInfo, Option<OwnedFd>)> {
    check_netns(netns_path)?;
    let key = ns_key(netns_path)?;
    let remembered = memory().get(key);

    // Discover veth info (IP, MAC, gateway) from pod netns
    let info = in_netns(netns_path, || {
        let nl = Netlink::open().context("netlink")?;
        let mut from_memory = false;
        let veth = retry(20, 100, || {
            let links = nl.dump_links()?;
            for (idx, name, mac) in &links {
                if name == "lo" || name.is_empty() {
                    continue;
                }
                if let Some(cidr) = nl.get_ipv4(*idx)? {
                    return Ok(Some((cidr, mac.clone())));
                }
            }
            // No interface has an address: an earlier start in this pod gave it to its VM. If the interface we remembered is still
            // here (same MAC), the pod is as it was.
            if let Some(r) = &remembered {
                if pick_veth(&links, "", Some(&r.mac), &|_| None).is_some() {
                    from_memory = true;
                    return Ok(Some((r.ip_cidr.clone(), r.mac.clone())));
                }
            }
            Ok(None)
        })
        .context("find veth")?;
        if from_memory {
            log::info!(
                "the pod's interface has no address (an earlier start gave it to its VM): using the remembered {} for {}",
                veth.0,
                veth.1
            );
            if let Some(r) = &remembered {
                return Ok(r.clone());
            }
        }
        // Gateway is optional — some CNI configs (e.g. bridge without
        // explicit routes) don't add a default route in the pod netns.
        // When missing, infer the gateway as the first IP in the subnet
        // (e.g. 10.88.0.2/16 → 10.88.0.1), which matches what bridge
        // CNI assigns to the cni0 bridge with isGateway: true.
        let gw = match nl.get_default_gw() {
            Ok(Some(g)) => g,
            _ => {
                let inferred = infer_gateway(&veth.0);
                log::info!(
                    "no default gateway in pod netns, inferred {} from {}",
                    inferred.as_deref().unwrap_or("none"),
                    veth.0
                );
                inferred.unwrap_or_default()
            }
        };
        Ok(PodNetInfo {
            ip_cidr: veth.0,
            mac: veth.1,
            gateway: gw,
        })
    })?;

    memory().put(key, info.clone(), netns_path);

    // Back in host netns — clean up stale TAP and create fresh one
    {
        let nl = Netlink::open().context("netlink")?;
        let _ = nl.del_link(tap_name);
    }
    let fd = create_tap(tap_name, hand_over).context("create TAP")?;

    Ok((info, if hand_over { Some(fd) } else { None }))
}

/// Phase 3: Move TAP from host netns to pod netns and set up TC redirects.
///
/// Must be called after CH has opened the TAP (via vm.add-net). The fd CH
/// holds remains valid across the namespace move.
pub fn activate_tap(netns_path: &str, tap_name: &str) -> Result<()> {
    // The MAC of the pod's interface, from `prepare_tap` (the interface has no address any more on a start after the first).
    let mac = ns_key(netns_path)
        .ok()
        .and_then(|k| memory().get(k))
        .map(|i| i.mac);

    // Move TAP interface to pod netns via IFLA_NET_NS_FD
    {
        let nl = Netlink::open().context("netlink")?;
        let tap_idx = nl.get_link_index(tap_name).context("TAP index")?;
        let ns_fd = open_checked(netns_path)?;
        nl.set_link_netns_fd(tap_idx, ns_fd.as_raw_fd())
            .context("move TAP to pod netns")?;
    }

    // Enter pod netns and wire up TC redirects
    in_netns(netns_path, || {
        let nl = Netlink::open().context("netlink")?;
        let tap_idx = nl.get_link_index(tap_name).context("TAP index")?;
        nl.set_link_up(tap_idx).context("TAP up")?;
        let (_, veth_idx, _, _) =
            retry(20, 100, || nl.find_veth_with(tap_name, mac.as_deref())).context("find veth")?;
        // The veth may carry the filters of an earlier start in this pod (it was torn down, the pod was not): they point at a TAP that is
        // gone, and would be first in line. Start from none.
        let _ = nl.del_ingress_qdisc(veth_idx);
        nl.add_ingress_qdisc(veth_idx).context("ingress veth")?;
        nl.add_redirect_filter(veth_idx, tap_idx)
            .context("redir veth→tap")?;
        nl.add_ingress_qdisc(tap_idx).context("ingress tap")?;
        nl.add_redirect_filter(tap_idx, veth_idx)
            .context("redir tap→veth")?;
        if let Err(e) = nl.flush_addrs(veth_idx) {
            log::warn!("best-effort IP flush: {e:#}");
        }
        log::info!("TAP {tap_name}: TC redirects active in pod netns");
        Ok(())
    })
}

// ── Netns helpers ───────────────────────────────────────────────────────────

const NSFS_MAGIC: i64 = 0x6e73_6673;
/// `ioctl(NS_GET_NSTYPE)` on a namespace file returns the `CLONE_NEW*` flag of its kind.
const NS_GET_NSTYPE: libc::c_ulong = 0xb703;

/// Open a network namespace file and make sure it is one: on `nsfs`, of the network kind, and not the daemon's own (which is the host's;
/// a request that names it would have the guest bridged to the host's interfaces). `path` must be one `validate::netns_path_allowed`
/// accepts.
fn open_checked(path: &str) -> Result<OwnedFd> {
    if !crate::validate::netns_path_allowed(path) {
        anyhow::bail!("{path} is not a network namespace the daemon may enter");
    }
    let fd = open_ns(path)?;
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(fd.as_raw_fd(), &mut sfs) } != 0 {
        return Err(std::io::Error::last_os_error()).context("fstatfs");
    }
    if sfs.f_type as i64 != NSFS_MAGIC {
        anyhow::bail!("{path} is not a namespace file");
    }
    let kind = unsafe { libc::ioctl(fd.as_raw_fd(), NS_GET_NSTYPE as _) };
    if kind != libc::CLONE_NEWNET {
        anyhow::bail!("{path} is not a network namespace");
    }
    let own = open_ns("/proc/self/ns/net")?;
    let (a, b) = (stat_of(fd.as_raw_fd())?, stat_of(own.as_raw_fd())?);
    if (a.st_dev, a.st_ino) == (b.st_dev, b.st_ino) {
        anyhow::bail!("{path} is the daemon's own network namespace");
    }
    Ok(fd)
}

fn stat_of(fd: i32) -> Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(std::io::Error::last_os_error()).context("fstat");
    }
    Ok(st)
}

/// Whether a request's network namespace is one the daemon will enter (see `open_checked`).
pub fn check_netns(path: &str) -> Result<()> {
    open_checked(path).map(|_| ())
}

/// Run `f` inside the network namespace `path`. It runs on a thread of its own that ends with it: `setns` changes the namespace of the
/// calling thread, and a thread that could not come back (or was left inside by a panic) must never be a worker that other work runs
/// on. A thread that is gone cannot be found in the wrong namespace later, so nothing has to be put back.
fn in_netns<F, T>(path: &str, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send,
    T: Send,
{
    for attempt in 0..20 {
        if std::path::Path::new(path).exists() {
            if attempt > 0 {
                log::info!("netns appeared after {attempt} retries");
            }
            break;
        }
        if attempt == 19 {
            anyhow::bail!("netns {path} did not appear after 2s");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let target = open_checked(path)?;
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("netns".into())
            .spawn_scoped(s, move || {
                if unsafe { libc::setns(target.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
                    return Err(std::io::Error::last_os_error()).context("setns into target");
                }
                drop(target);
                f()
            })
            .context("spawn the netns thread")?
            .join()
            .map_err(|_| anyhow::anyhow!("the netns thread panicked"))?
    })
}

/// Delete a TAP device, wherever it is: in the daemon's namespace (before it was moved) or in the pod's (after). Best effort; for the
/// cleanup of a start that failed.
pub fn delete_tap(netns_path: &str, tap_name: &str) {
    if let Ok(nl) = Netlink::open() {
        let _ = nl.del_link(tap_name);
    }
    let _ = in_netns(netns_path, || {
        let nl = Netlink::open().context("netlink")?;
        let _ = nl.del_link(tap_name);
        Ok(())
    });
}

/// (Not `O_NOFOLLOW`: `/proc/<pid>/ns/net` is a link of the kernel's own. What the file is, is checked after, by `open_checked`.)
fn open_ns(path: &str) -> Result<OwnedFd> {
    let c = std::ffi::CString::new(path)?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("open {path}"));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn retry<T>(max: u32, ms: u64, mut f: impl FnMut() -> Result<Option<T>>) -> Result<T> {
    for i in 0..max {
        if let Some(v) = f()? {
            if i > 0 {
                log::info!("found after {i} retries");
            }
            return Ok(v);
        }
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
    anyhow::bail!("not found after {max} retries")
}

/// Infer a gateway from a CIDR address by using the first usable IP in the
/// subnet. For example, "10.88.0.2/16" → "10.88.0.1".
fn infer_gateway(ip_cidr: &str) -> Option<String> {
    let parts: Vec<&str> = ip_cidr.split('/').collect();
    let ip: std::net::Ipv4Addr = parts.first()?.parse().ok()?;
    let prefix: u32 = parts.get(1)?.parse().ok()?;
    if prefix > 30 {
        return None;
    }
    let mask = !((1u32 << (32 - prefix)) - 1);
    let network = u32::from(ip) & mask;
    let gateway = std::net::Ipv4Addr::from(network + 1);
    Some(gateway.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloudhv_common::netlink::TAP_PREFIX;

    fn info(ip: &str) -> PodNetInfo {
        PodNetInfo {
            ip_cidr: ip.into(),
            gateway: "10.0.0.1".into(),
            mac: "aa:bb:cc:00:00:01".into(),
        }
    }

    #[test]
    fn what_a_pod_had_is_remembered_by_its_namespace_not_its_path() {
        let mut m = NetMemory::default();
        assert!(m.get((1, 10)).is_none());
        m.put((1, 10), info("10.0.0.5/24"), "/run/netns/a");
        assert_eq!(m.get((1, 10)), Some(info("10.0.0.5/24")));
        // Another namespace (even at the same path, later) is another entry.
        assert!(m.get((1, 11)).is_none());
        // The same namespace seen again replaces what was there.
        m.put((1, 10), info("10.0.0.6/24"), "/run/netns/a");
        assert_eq!(m.get((1, 10)).unwrap().ip_cidr, "10.0.0.6/24");
    }

    #[test]
    fn entries_of_namespaces_that_are_gone_are_dropped_when_the_memory_is_full() {
        let dir = std::env::temp_dir().join(format!("cchv-netmem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let alive = dir.join("alive");
        std::fs::write(&alive, b"").unwrap();
        let mut m = NetMemory::default();
        m.put((9, 1), info("10.0.0.2/24"), alive.to_str().unwrap());
        for i in 0..(MEMORY_PRUNE_AT as u64 + 5) {
            m.put((8, i), info("10.0.1.2/24"), "/nonexistent/ns");
        }
        // The gone ones went at the prune; the live one stayed.
        assert!(m.map.len() < MEMORY_PRUNE_AT);
        assert!(m.get((9, 1)).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_remembered_mac_that_no_interface_has_is_not_used() {
        // The rule `prepare_tap` applies: the remembered address is used only if an interface of the namespace has the remembered MAC.
        let links = vec![
            (1u32, "lo".to_string(), String::new()),
            (2, "eth0".to_string(), "aa:bb:cc:00:00:99".to_string()),
        ];
        let none = |_: u32| None;
        assert!(pick_veth(&links, "", Some("aa:bb:cc:00:00:01"), &none).is_none());
        assert!(pick_veth(&links, "", Some("aa:bb:cc:00:00:99"), &none).is_some());
        // Its own TAP devices are not "the interface".
        let with_tap = vec![(
            7u32,
            format!("{TAP_PREFIX}0123456789a"),
            "aa:bb:cc:00:00:01".to_string(),
        )];
        assert!(pick_veth(&with_tap, "", Some("aa:bb:cc:00:00:01"), &none).is_none());
    }

    /// Needs root, `ip` and /dev/net/tun (a privileged container): `cargo test -p cloudhv-sandbox-daemon --bins -- --ignored netns_retry`.
    /// A pod's namespace with a veth that has an address: the first start finds it; the address then goes to a VM (flushed); the start in the
    /// same pod again must find the veth all the same, with the same address, gateway and MAC.
    #[test]
    #[ignore = "needs root, ip(8) and /dev/net/tun"]
    fn netns_retry_after_the_address_went_to_a_vm() {
        let ns = format!("cchv-t-{}", std::process::id());
        let path = format!("/run/netns/{ns}");
        let sh = |cmd: &str| {
            let st = std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .status()
                .unwrap();
            assert!(st.success(), "{cmd}");
        };
        sh(&format!(
            "ip netns add {ns} && ip link add vh{0} type veth peer name eth0 netns {ns} && ip -n {ns} addr add 10.200.0.2/24 dev eth0 && ip -n {ns} link set eth0 up && ip -n {ns} link set lo up && ip link set vh{0} up",
            std::process::id() % 100000
        ));
        let tap = "tap_00000000abc";
        let first = prepare_tap(&path, tap, false).map(|(i, _)| i);
        // The VM took the address (what activate_tap does).
        sh(&format!("ip -n {ns} addr flush dev eth0"));
        let second = prepare_tap(&path, tap, false).map(|(i, _)| i);
        // A namespace the daemon never saw, whose veth has no address, is still an error (nothing remembered).
        let ns2 = format!("{ns}b");
        sh(&format!(
            "ip netns add {ns2} && ip link add vi{0} type veth peer name eth0 netns {ns2}",
            std::process::id() % 100000
        ));
        let third = prepare_tap(&format!("/run/netns/{ns2}"), tap, false).map(|(i, _)| i);
        if let Ok(nl) = Netlink::open() {
            let _ = nl.del_link(tap);
        }
        sh(&format!("ip netns del {ns}; ip netns del {ns2}; ip link del vh{0} 2>/dev/null; ip link del vi{0} 2>/dev/null; true", std::process::id() % 100000));
        let first = first.expect("first start");
        assert_eq!(first.ip_cidr, "10.200.0.2/24");
        assert_eq!(
            second.expect("the start after the address went to the VM"),
            first
        );
        assert!(
            third.is_err(),
            "nothing was remembered for a namespace never seen"
        );
    }
}
