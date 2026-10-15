//! Things that touch the system: the named pipes containerd makes for a container, and the (empty) metrics.

pub mod metrics {
    use anyhow::Result;
    use protobuf::well_known_types::any::Any;

    pub fn get_metrics(_pid: u32) -> Result<Any> {
        // The shim delegates VM lifecycle to the sandbox daemon. The PID returned
        // from start() is the in-guest container PID, not a host process we can
        // collect cgroup metrics for. Return empty metrics to avoid:
        // 1. "read cgroup error: No such file or directory" (PID not in a cgroup)
        // 2. "can't unmarshal v1.Metrics to v2.Metrics" (wrong protobuf type on v2 systems)
        //
        // Real resource accounting is done by placing the CH process in the
        // pod's sandbox cgroup — kubelet tracks that cgroup directly.
        let mut any = Any::new();

        // Detect cgroup version and return the correct empty type
        if std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
            any.type_url = "io.containerd.cgroups.v2.Metrics".to_string();
        } else {
            any.type_url = "io.containerd.cgroups.v1.Metrics".to_string();
        }
        any.value = Vec::new();

        Ok(any)
    }
}

pub mod stdio {
    use std::fs::{File, OpenOptions};
    use std::io::Result;
    use std::path::Path;

    /// Open a named pipe read-write: that does not wait for the other end.
    pub fn open(path: impl AsRef<Path>) -> Result<File> {
        OpenOptions::new().read(true).write(true).open(path)
    }
}
