//! Writing to the pipes containerd made for a container's stdout and stderr.
//!
//! containerd's pipes are FIFOs, which the shim opens read-write (so that opening does not wait for a reader). A write that is given up on
//! (a timeout, or the instance being deleted) must close the pipe at once: containerd waits for the end of the stream (EOF) before it
//! goes on, and it only comes when every writer has closed. `tokio::fs::File` does its writes on a thread of the blocking pool, which
//! holds the descriptor until the write has returned: a write to a full pipe that nobody reads (the shim holds the read end itself, as
//! it opened the FIFO read-write) never returns, so dropping the future did not close the pipe, and the stream never ended. A FIFO
//! is therefore written through tokio's non-blocking pipe `Sender`: dropping it closes the descriptor at that moment.

use std::fs::File;
use std::os::unix::fs::FileTypeExt;

use tokio::io::AsyncWrite;
use tokio::net::unix::pipe::Sender;

pub type PipeOut = Box<dyn AsyncWrite + Unpin + Send>;

/// The file as something to write to asynchronously. `None` if it cannot be done (the file is dropped, which closes it).
pub fn writer(file: File) -> Option<PipeOut> {
    let is_fifo = file.metadata().ok()?.file_type().is_fifo();
    if is_fifo {
        Sender::from_file(file).ok().map(|s| Box::new(s) as PipeOut)
    } else {
        // A regular file or a device (a test, `/dev/null`): writes to these do not wait for a reader.
        Some(Box::new(tokio::fs::File::from_std(file)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    fn fifo_rdwr(dir: &std::path::Path) -> File {
        let p = dir.join("f");
        nix::unistd::mkfifo(&p, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        std::fs::OpenOptions::new().read(true).write(true).open(&p).unwrap()
    }

    /// The number of descriptors this process has open on the FIFO's inode.
    fn open_on(ino: u64) -> usize {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|e| std::fs::metadata(e.ok()?.path()).ok())
            .filter(|m| std::os::unix::fs::MetadataExt::ino(m) == ino)
            .count()
    }

    #[tokio::test]
    async fn a_write_that_times_out_on_a_full_pipe_closes_the_pipe() {
        let tmp = tempfile::tempdir().unwrap();
        let file = fifo_rdwr(tmp.path());
        let ino = std::os::unix::fs::MetadataExt::ino(&file.metadata().unwrap());
        let _ = file.as_raw_fd();
        let mut w = writer(file).expect("a FIFO");
        // Nobody reads: the pipe (64 KiB) fills, the write waits, the timeout drops it.
        let big = vec![b'x'; 1 << 20];
        let r = tokio::time::timeout(Duration::from_millis(300), w.write_all(&big)).await;
        assert!(r.is_err(), "the write must be waiting on a full pipe");
        drop(w);
        // The descriptor is closed now, not when some thread's write returns.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(open_on(ino), 0, "the FIFO is still open in this process");
    }

    #[tokio::test]
    async fn a_regular_file_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("out");
        let file = std::fs::OpenOptions::new().create(true).write(true).open(&p).unwrap();
        let mut w = writer(file).unwrap();
        w.write_all(b"abc").await.unwrap();
        w.flush().await.unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"abc");
    }
}
