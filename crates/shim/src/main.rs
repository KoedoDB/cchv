#![deny(dead_code)]

use containerd_shimkit::sandbox;

mod cachekey;
mod config;
mod daemon_client;
mod exec;
mod erofs_cache;
mod instance;
mod netns;
mod vmsize;
mod volumes;

use instance::CloudHvInstance;

fn main() {
    sandbox::cli::shim_main::<CloudHvInstance>(
        "io.containerd.cloudhv.v1",
        sandbox::cli::Version {
            version: env!("CARGO_PKG_VERSION"),
            revision: "dev",
        },
        None,
    );
}
