#![allow(dead_code, unused_imports, unknown_lints)]

// The same module tree as crates/daemon/src/main.rs.
#[path = "../../../crates/daemon/src/api.rs"]
pub mod api;
#[path = "../../../crates/daemon/src/cgroup.rs"]
pub mod cgroup;
#[path = "../../../crates/daemon/src/config.rs"]
pub mod config;
#[path = "../../../crates/daemon/src/confine.rs"]
pub mod confine;
#[path = "../../../crates/daemon/src/ctl.rs"]
pub mod ctl;
#[path = "../../../crates/daemon/src/exec.rs"]
pub mod exec;
#[path = "../../../crates/daemon/src/fsutil.rs"]
pub mod fsutil;
#[path = "../../../crates/daemon/src/imagekey.rs"]
pub mod imagekey;
#[path = "../../../crates/daemon/src/netns.rs"]
pub mod netns;
#[path = "../../../crates/daemon/src/pool.rs"]
pub mod pool;
#[path = "../../../crates/daemon/src/rootless.rs"]
pub mod rootless;
#[path = "../../../crates/daemon/src/shadow.rs"]
pub mod shadow;
#[path = "../../../crates/daemon/src/validate.rs"]
pub mod validate;
#[path = "../../../crates/daemon/src/vm_lifecycle.rs"]
pub mod vm_lifecycle;

pub mod entry;
