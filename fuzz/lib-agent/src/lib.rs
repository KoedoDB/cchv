#![allow(dead_code, unused_imports, unknown_lints)]

// The same module tree as crates/agent/src/main.rs.
#[allow(clippy::all)]
#[allow(non_snake_case)]
#[allow(non_camel_case_types)]
#[path = "../../../crates/agent/src/generated"]
pub mod proto {
    pub mod agent;
    pub mod agent_ttrpc;
}

#[path = "../../../crates/agent/src/container.rs"]
pub mod container;
#[path = "../../../crates/agent/src/disks.rs"]
pub mod disks;
#[path = "../../../crates/agent/src/exec.rs"]
pub mod exec;
#[path = "../../../crates/agent/src/mount.rs"]
pub mod mount;
#[path = "../../../crates/agent/src/reaper.rs"]
pub mod reaper;
#[path = "../../../crates/agent/src/seccomp.rs"]
pub mod seccomp;
#[path = "../../../crates/agent/src/server.rs"]
pub mod server;
#[path = "../../../crates/agent/src/spec.rs"]
pub mod spec;
#[path = "../../../crates/agent/src/volumes.rs"]
pub mod volumes;

pub mod entry;
