//! The key of a warm snapshot: a hash of the image it was made from.
//!
//! The shim sends an `image_key`, and a warm snapshot used to be found by it. A key the caller chooses lets a pod be given the warm
//! guest of another pod's image (the snapshot holds that image's files in memory). The daemon therefore makes the key itself, from
//! the *content* of the erofs image the pod will run: two pods get the same snapshot only if their images are the same bytes.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::fsutil;

/// (path, size, mtime in ns, inode, device) → key. A file that is replaced or changed has another size, time or inode, so it is read
/// again; one that is not, is not read again for every pod.
type CacheKey = (String, u64, i64, u64, u64);
static CACHE: Mutex<Option<HashMap<CacheKey, String>>> = Mutex::new(None);
const CACHE_MAX: usize = 4096;

/// 32 lower-case hex digits (128 bits of SHA-256): a valid identifier, and short enough for a VM name.
pub async fn content_key(path: &str) -> Result<String> {
    let path = path.to_string();
    tokio::task::spawn_blocking(move || content_key_blocking(Path::new(&path)))
        .await
        .context("hashing the image panicked")?
}

fn content_key_blocking(path: &Path) -> Result<String> {
    let fd = fsutil::open_nosymlinks(path, libc::O_RDONLY | libc::O_NONBLOCK)
        .with_context(|| format!("open {}", path.display()))?;
    let st = fsutil::fstat(fd.as_raw_fd())?;
    if !fsutil::is_reg(&st) {
        anyhow::bail!("{} is not a regular file", path.display());
    }
    let key: CacheKey = (
        path.display().to_string(),
        st.st_size as u64,
        st.st_mtime * 1_000_000_000 + st.st_mtime_nsec,
        st.st_ino,
        st.st_dev,
    );
    if let Some(k) = CACHE.lock().unwrap().as_ref().and_then(|c| c.get(&key)) {
        return Ok(k.clone());
    }
    let mut file = std::fs::File::from(fd);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    let mut cache = CACHE.lock().unwrap();
    let map = cache.get_or_insert_with(HashMap::new);
    if map.len() >= CACHE_MAX {
        map.clear();
    }
    map.insert(key, hex.clone());
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_follows_the_content_not_the_name() {
        let d = std::env::temp_dir().join(format!("cloudhv-imagekey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.erofs"), b"the same bytes").unwrap();
        std::fs::write(d.join("b.erofs"), b"the same bytes").unwrap();
        std::fs::write(d.join("c.erofs"), b"other bytes").unwrap();
        let a = content_key_blocking(&d.join("a.erofs")).unwrap();
        let b = content_key_blocking(&d.join("b.erofs")).unwrap();
        let c = content_key_blocking(&d.join("c.erofs")).unwrap();
        assert_eq!(a, b, "two files with the same content have one key");
        assert_ne!(a, c);
        assert_eq!(a.len(), 32);
        assert!(crate::validate::is_id(&a));
        // a known SHA-256: "abc" -> ba7816bf8f01cfea414140de5dae2223...
        std::fs::write(d.join("abc.erofs"), b"abc").unwrap();
        assert_eq!(content_key_blocking(&d.join("abc.erofs")).unwrap(), "ba7816bf8f01cfea414140de5dae2223");
        // a file changed in place (another size) is read again
        std::fs::write(d.join("a.erofs"), b"changed bytes now").unwrap();
        assert_ne!(content_key_blocking(&d.join("a.erofs")).unwrap(), a);
        std::os::unix::fs::symlink(d.join("c.erofs"), d.join("link.erofs")).unwrap();
        assert!(content_key_blocking(&d.join("link.erofs")).is_err(), "a link is not followed");
        let _ = std::fs::remove_dir_all(&d);
    }
}
