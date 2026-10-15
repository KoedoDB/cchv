//! The key under which a container's rootfs is cached as an erofs image (and under which the daemon keeps its warm snapshots).
//!
//! The key must say what is really mounted at the container's `rootfs`, not what the image's name pointed at when the container was
//! created: a tag that was moved, or an image pulled again between the two, would put one image's file system under another's key,
//! and every later pod of that key would get the wrong rootfs. So it is made from the mount itself (`/proc/self/mountinfo`):
//!
//! - an overlay mount (containerd's overlayfs snapshotter): the list of its `lowerdir`s, in the order they are stacked (the order
//!   decides which layer's file shows; it is not sorted). The directories are containerd's snapshots, whose numbers are never
//!   reused, so two pods of one image have one key and two images have two. The container's own `upperdir` and `workdir` are not part
//!   of it (they are different for every container and empty at its start);
//! - any other mount (a bind of a snapshot directory, another snapshotter): the file system, the device and the directory mounted;
//! - no mount at the rootfs (a plain directory): the directory's device, inode and times.
//!
//! If none of that can be read, there is no key and the container does not start. The image's digest, when containerd gives one that
//! looks like a digest, is added, so that the same layers under two different images are cached twice rather than shared by guess.

use std::path::Path;

use sha2::{Digest, Sha256};

/// `sha256:` and 64 lowercase hex digits.
pub fn valid_digest(s: &str) -> bool {
    s.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

/// The key of the rootfs at `rootfs`, as a 32 digit hex string.
pub fn rootfs_key(rootfs: &Path, digest: Option<&str>) -> Result<String, String> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let canonical = std::fs::canonicalize(rootfs).map_err(|e| format!("rootfs {}: {e}", rootfs.display()))?;
    let material = match material_from_mountinfo(&mountinfo, &canonical.to_string_lossy()) {
        Some(m) => m,
        None => material_from_directory(&canonical)?,
    };
    Ok(key_of(&material, digest))
}

fn key_of(material: &str, digest: Option<&str>) -> String {
    let mut h = Sha256::new();
    h.update(material.as_bytes());
    if let Some(d) = digest.filter(|d| valid_digest(d)) {
        h.update(b"\0digest\0");
        h.update(d.as_bytes());
    }
    h.finalize()[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// A path in `mountinfo` writes space, tab, newline and backslash as `\040`, `\011`, `\012`, `\134`.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() + 0 && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// What the mount at `rootfs` is made of, from the text of `/proc/self/mountinfo`; the topmost mount there if more than one.
fn material_from_mountinfo(text: &str, rootfs: &str) -> Option<String> {
    let mut found = None;
    for line in text.lines() {
        let Some((left, right)) = line.split_once(" - ") else { continue };
        let l: Vec<&str> = left.split(' ').collect();
        let r: Vec<&str> = right.split(' ').collect();
        if l.len() < 6 || r.len() < 3 || unescape(l[4]) != rootfs {
            continue;
        }
        let (fstype, super_opts) = (r[0], r[2]);
        found = Some(if fstype == "overlay" {
            // `lowerdir=a:b:c`, or (newer kernels) one `lowerdir+=a` after another; `datadir+=` layers are lower layers too.
            let mut lowers = Vec::new();
            for opt in super_opts.split(',') {
                if let Some(v) = opt.strip_prefix("lowerdir=") {
                    lowers.extend(v.split(':').map(unescape));
                } else if let Some(v) = opt.strip_prefix("lowerdir+=").or_else(|| opt.strip_prefix("datadir+=")) {
                    lowers.push(unescape(v));
                }
            }
            if lowers.is_empty() {
                return None;
            }
            format!("overlay\0{}", lowers.join("\n"))
        } else {
            format!("mount\0{fstype}\0{}\0{}\0{}", l[2], unescape(l[3]), unescape(r[1]))
        });
    }
    found
}

fn material_from_directory(dir: &Path) -> Result<String, String> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(dir).map_err(|e| format!("rootfs {}: {e}", dir.display()))?;
    Ok(format!(
        "dir\0{}\0{}\0{}.{}\0{}.{}\0{}",
        m.dev(),
        m.ino(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
        dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OVERLAY: &str = "\
36 25 0:30 / /run/containerd/io.containerd.runtime.v2.task/k8s.io/c1/rootfs rw,relatime - overlay overlay rw,lowerdir=/var/lib/containerd/snapshots/12/fs:/var/lib/containerd/snapshots/11/fs,upperdir=/var/lib/containerd/snapshots/20/fs,workdir=/var/lib/containerd/snapshots/20/work
37 25 0:31 / /run/containerd/io.containerd.runtime.v2.task/k8s.io/c2/rootfs rw,relatime - overlay overlay rw,lowerdir=/var/lib/containerd/snapshots/12/fs:/var/lib/containerd/snapshots/11/fs,upperdir=/var/lib/containerd/snapshots/21/fs,workdir=/var/lib/containerd/snapshots/21/work
38 25 0:32 / /run/containerd/io.containerd.runtime.v2.task/k8s.io/c3/rootfs rw,relatime - overlay overlay rw,lowerdir=/var/lib/containerd/snapshots/11/fs:/var/lib/containerd/snapshots/12/fs,upperdir=/var/lib/containerd/snapshots/22/fs,workdir=/var/lib/containerd/snapshots/22/work
39 25 0:33 / /run/containerd/io.containerd.runtime.v2.task/k8s.io/c4/rootfs rw,relatime - overlay overlay rw,lowerdir=/var/lib/containerd/snapshots/13/fs:/var/lib/containerd/snapshots/11/fs,upperdir=/var/lib/containerd/snapshots/23/fs,workdir=/var/lib/containerd/snapshots/23/work
40 25 259:1 /var/lib/containerd/snapshots/30/fs /b/rootfs rw - ext4 /dev/nvme0n1 rw
41 25 259:1 /var/lib/containerd/snapshots/31/fs /c/rootfs rw - ext4 /dev/nvme0n1 rw
";
    fn rootfs(c: &str) -> String {
        format!("/run/containerd/io.containerd.runtime.v2.task/k8s.io/{c}/rootfs")
    }

    #[test]
    fn pods_of_one_image_share_a_key_and_the_order_of_layers_matters() {
        let k = |c: &str| key_of(&material_from_mountinfo(OVERLAY, &rootfs(c)).unwrap(), None);
        assert_eq!(k("c1"), k("c2"), "same layers, other upperdir: the same image");
        assert_ne!(k("c1"), k("c3"), "the same layers in another order are another file system");
        assert_ne!(k("c1"), k("c4"), "another layer");
        assert_eq!(k("c1").len(), 32);
    }

    #[test]
    fn a_bind_of_a_snapshot_directory_is_told_apart_by_the_directory() {
        let k = |p: &str| key_of(&material_from_mountinfo(OVERLAY, p).unwrap(), None);
        assert_ne!(k("/b/rootfs"), k("/c/rootfs"));
        assert!(material_from_mountinfo(OVERLAY, "/nowhere").is_none());
    }

    #[test]
    fn the_digest_is_added_only_when_it_is_one() {
        let m = material_from_mountinfo(OVERLAY, &rootfs("c1")).unwrap();
        let d = format!("sha256:{}", "ab".repeat(32));
        assert_ne!(key_of(&m, Some(&d)), key_of(&m, None));
        assert_eq!(key_of(&m, Some("latest")), key_of(&m, None), "a tag is not a digest and is ignored");
        assert_eq!(key_of(&m, Some("sha256:XYZ")), key_of(&m, None));
        assert!(valid_digest(&d));
        assert!(!valid_digest(&d.to_uppercase()));
        assert!(!valid_digest("sha256:abc"));
    }

    #[test]
    fn newer_kernels_write_one_lowerdir_per_layer_and_paths_are_unescaped() {
        let line = "50 25 0:40 / /r/rootfs rw - overlay overlay rw,lowerdir+=/l/with\\040space/1,lowerdir+=/l/2,upperdir=/u,workdir=/w";
        let m = material_from_mountinfo(line, "/r/rootfs").unwrap();
        assert!(m.contains("/l/with space/1\n/l/2"), "{m:?}");
        assert_eq!(unescape("a\\040b\\134c"), "a b\\c");
    }

    #[test]
    fn the_topmost_mount_wins_and_an_overlay_without_layers_has_no_key() {
        let two = "1 0 0:1 / /r rw - overlay o rw,lowerdir=/a\n2 1 0:2 / /r rw - overlay o rw,lowerdir=/b\n";
        let m = material_from_mountinfo(two, "/r").unwrap();
        assert!(m.contains("/b") && !m.contains("/a"));
        assert!(material_from_mountinfo("1 0 0:1 / /r rw - overlay o rw,upperdir=/u", "/r").is_none());
    }

    #[test]
    fn a_plain_directory_gets_a_key_from_itself_and_a_missing_one_fails() {
        let d = std::env::temp_dir().join(format!("cloudhv-key-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let k1 = rootfs_key(&d, None).unwrap();
        assert_eq!(k1, rootfs_key(&d, None).unwrap());
        std::fs::create_dir_all(d.join("sub")).unwrap(); // the directory changed: another key
        assert_ne!(k1, rootfs_key(&d, None).unwrap());
        let _ = std::fs::remove_dir_all(&d);
        assert!(rootfs_key(&d, None).is_err());
    }
}
