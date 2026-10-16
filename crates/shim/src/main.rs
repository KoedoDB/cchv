#![deny(dead_code)]

use containerd_shimkit::sandbox;

mod cachekey;
mod config;
mod daemon_client;
mod dockermode;
mod exec;
mod erofs_cache;
mod instance;
mod netns;
mod vmsize;
mod volumes;

use instance::CloudHvInstance;

fn main() {
    // The network namespace holder (see dockermode.rs) is this binary run for that one job.
    if std::env::var_os(dockermode::HOLDER_ENV).is_some() {
        dockermode::holder_main();
    }
    sandbox::cli::shim_main::<CloudHvInstance>(
        "io.containerd.cloudhv.v1",
        sandbox::cli::Version {
            version: env!("CARGO_PKG_VERSION"),
            revision: "dev",
        },
        None,
    );
}
