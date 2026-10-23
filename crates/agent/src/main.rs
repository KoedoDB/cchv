#![allow(unknown_lints)]
#![deny(dead_code)]

use log::{error, info};
use nix::unistd::getpid;

// Generated ttrpc/protobuf code from agent.proto
#[allow(clippy::all)]
#[allow(non_snake_case)]
#[allow(non_camel_case_types)]
#[path = "generated"]
mod proto {
    pub mod agent;
    pub mod agent_ttrpc;
}

mod cgdelegate;
mod container;
mod disks;
mod exec;
mod mount;
mod reaper;
mod seccomp;
mod server;
mod spec;
mod volumes;

/// The guest agent runs as PID 1 (init) inside the Cloud Hypervisor VM.
fn main() {
    if cloudhv_common::buildinfo::wants_version(std::env::args().nth(1).as_deref()) {
        use cloudhv_common::buildinfo as b;
        print!(
            "{}",
            b::report(
                "cloudhv-agent",
                &[
                    ("runs crun", b::CRUN_VERSION),
                    ("guest kernel", b::KERNEL_VERSION)
                ]
            )
        );
        return;
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let pid = getpid();
    info!(
        "cloudhv-agent starting (version {}, pid={})",
        cloudhv_common::buildinfo::VERSION,
        pid
    );

    if pid.as_raw() == 1 {
        info!("running as PID 1 (init)");
        init_setup();
    } else {
        info!("running as non-init process (pid={})", pid);
        reaper::set_child_subreaper();
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    rt.block_on(async {
        if let Err(e) = run_agent().await {
            error!("agent failed: {}", e);
            std::process::exit(1);
        }
    });
}

/// PID 1 init setup: mount filesystems and configure the system.
fn init_setup() {
    // Without its mounts the VM cannot run a container safely (a /tmp or /dev/shm that is not locked down, no cgroups). As PID 1 an
    // exit stops the VM, which is the right thing then: a VM that is broken must not take pods.
    if let Err(e) = mount::mount_essential_filesystems() {
        error!("failed to mount essential filesystems: {:#}", e);
        std::process::exit(1);
    }
    mount::apply_sysctls();

    reaper::set_child_subreaper();

    info!("init setup complete");
}

async fn run_agent() -> anyhow::Result<()> {
    // NOTE: no waitpid(-1) loop — it interferes with crun's own child management (crun forks twice, and reaping its intermediate
    // children makes it hang). Orphaned zombies are reaped by reaper::reap_stale_zombies, which leaves alone every process the agent
    // started or may be waiting for.

    let server = server::AgentServer::new();
    server.run().await
}
