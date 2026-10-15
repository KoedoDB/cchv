//! Finding the virtio-blk disks the host hot-plugs.
//!
//! The host attaches a container's erofs image (and block volumes) to the running VM, and the guest names the new disk `/dev/vdX` in
//! the order the kernel found it. That order is not something to rely on when several containers are started at once, so a disk is
//! found by its serial number when the host gave one, and otherwise as a *new* disk nobody has claimed yet. A disk is claimed once,
//! under a lock; a container that cannot get its disk within the time allowed fails to start (it must not run on another's disk,
//! or on none).

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use log::info;

/// The kernel's name of the n-th virtio-blk disk (0 is `vda`): `vda`..`vdz`, `vdaa`..
pub fn vd_name(n: usize) -> String {
    let mut letters = Vec::new();
    let mut n = n + 1;
    while n > 0 {
        n -= 1;
        letters.push(b'a' + (n % 26) as u8);
        n /= 26;
    }
    letters.reverse();
    format!("vd{}", String::from_utf8(letters).unwrap_or_default())
}

/// Order disk names as the kernel hands them out (`vdb` before `vdc` before `vdaa`).
pub fn sort_vd_names(names: &mut [String]) {
    names.sort_by(|a, b| (a.len(), a).cmp(&(b.len(), b)));
}

/// The disk whose serial is `want`, among `(name, serial)` pairs read from /sys/block, and which is not claimed yet.
pub fn find_by_serial(disks: &[(String, String)], want: &str, claimed: &HashSet<String>) -> Option<String> {
    if want.is_empty() {
        return None;
    }
    disks
        .iter()
        .find(|(name, serial)| serial.trim() == want && !claimed.contains(name))
        .map(|(name, _)| name.clone())
}

/// The disks nobody has claimed, in the order the kernel numbers them.
pub fn unclaimed(present: &[String], claimed: &HashSet<String>) -> Vec<String> {
    let mut v: Vec<String> = present.iter().filter(|n| !claimed.contains(*n)).cloned().collect();
    sort_vd_names(&mut v);
    v
}

/// The disks of the VM and which of them are claimed.
pub struct Disks {
    claimed: Mutex<HashSet<String>>,
}

impl Disks {
    /// The disks present now (the boot disk) are claimed.
    pub fn new() -> Self {
        Self {
            claimed: Mutex::new(scan().into_iter().collect()),
        }
    }

    /// Claim a disk by name (the ones attached at boot, whose names are fixed).
    pub fn claim_named(&self, name: &str) {
        self.claimed.lock().unwrap().insert(name.to_string());
    }

    /// Claim the disk with this serial, waiting for it to appear. Fails at the deadline.
    pub async fn claim_by_serial(&self, serial: &str, deadline: Instant) -> Result<String> {
        loop {
            {
                let mut claimed = self.claimed.lock().unwrap();
                let disks = read_serials();
                if let Some(name) = find_by_serial(&disks, serial, &claimed) {
                    claimed.insert(name.clone());
                    info!("disk {name} has serial {serial}");
                    return Ok(name);
                }
            }
            if Instant::now() >= deadline {
                anyhow::bail!("no disk with serial {serial} appeared in time");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Claim the next disk that appears and nobody has claimed. Fails at the deadline.
    pub async fn claim_next(&self, deadline: Instant) -> Result<String> {
        loop {
            {
                let mut claimed = self.claimed.lock().unwrap();
                if let Some(name) = unclaimed(&scan(), &claimed).into_iter().next() {
                    claimed.insert(name.clone());
                    info!("claimed new disk {name}");
                    return Ok(name);
                }
            }
            if Instant::now() >= deadline {
                anyhow::bail!("no new disk appeared in time");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// The virtio-blk disks now present (names such as `vda`).
fn scan() -> Vec<String> {
    std::fs::read_dir("/sys/block")
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|name| name.starts_with("vd"))
                .collect()
        })
        .unwrap_or_default()
}

fn read_serials() -> Vec<(String, String)> {
    scan()
        .into_iter()
        .map(|name| {
            let serial = std::fs::read_to_string(format!("/sys/block/{name}/serial")).unwrap_or_default();
            (name, serial)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_names_follow_the_kernels_numbering() {
        assert_eq!(vd_name(0), "vda");
        assert_eq!(vd_name(1), "vdb");
        assert_eq!(vd_name(25), "vdz");
        assert_eq!(vd_name(26), "vdaa");
        assert_eq!(vd_name(27), "vdab");
        assert_eq!(vd_name(51), "vdaz");
        assert_eq!(vd_name(52), "vdba");
    }

    #[test]
    fn names_sort_as_the_kernel_hands_them_out() {
        let mut v = vec!["vdaa".to_string(), "vdc".to_string(), "vdb".to_string(), "vdz".to_string()];
        sort_vd_names(&mut v);
        assert_eq!(v, ["vdb", "vdc", "vdz", "vdaa"]);
    }

    #[test]
    fn a_disk_is_found_by_its_serial_and_only_once() {
        let disks = vec![
            ("vdb".to_string(), "layer-a\n".to_string()),
            ("vdc".to_string(), "layer-b\n".to_string()),
            ("vdd".to_string(), "".to_string()),
        ];
        let mut claimed = HashSet::new();
        assert_eq!(find_by_serial(&disks, "layer-b", &claimed), Some("vdc".to_string()));
        claimed.insert("vdc".to_string());
        assert_eq!(find_by_serial(&disks, "layer-b", &claimed), None, "already claimed");
        assert_eq!(find_by_serial(&disks, "layer-a", &claimed), Some("vdb".to_string()));
        assert_eq!(find_by_serial(&disks, "nope", &claimed), None);
        assert_eq!(find_by_serial(&disks, "", &claimed), None, "an empty serial matches nothing");
    }

    #[test]
    fn unclaimed_disks_come_in_kernel_order_and_two_claims_get_different_ones() {
        let present: Vec<String> = ["vda", "vdc", "vdb", "vdd"].iter().map(|s| s.to_string()).collect();
        let mut claimed: HashSet<String> = ["vda".to_string()].into();
        let first = unclaimed(&present, &claimed);
        assert_eq!(first, ["vdb", "vdc", "vdd"]);
        claimed.insert(first[0].clone());
        let second = unclaimed(&present, &claimed);
        assert_eq!(second[0], "vdc");
    }
}
