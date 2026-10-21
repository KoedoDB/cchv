#![deny(dead_code)]

use containerd_shimkit::sandbox;

mod cachekey;
mod config;
mod daemon_client;
mod dockermode;
mod erofs_cache;
mod exec;
mod instance;
mod journal;
mod netns;
mod vmsize;
mod volumes;

use instance::CloudHvInstance;

fn main() {
    // The network namespace holder (see dockermode.rs) is this binary run for that one job.
    if std::env::var_os(dockermode::HOLDER_ENV).is_some() {
        dockermode::holder_main();
    }
    // The log goes to journald (see journal.rs), in every way containerd runs the shim (`start`, the server, `delete`); the library's own
    // logger, which sends it to a FIFO that nothing keeps, is turned off below.
    let args: Vec<String> = std::env::args().collect();
    journal::init(
        &journal::flag_value(&args, "id"),
        args.iter().any(|a| a == "-debug" || a == "--debug"),
    );
    // `-v`, `-version` and `--version` are answered by the shim's flag parser (and `-info` by containerd's own call), which print
    // these two. The revision carries what the binary was made from; it is built once, at start, and lives as long as the process
    // (the field is a `&'static str`; the build's `cargo:rustc-env` values are not visible outside cloudhv-common, so they are
    // read through its constants).
    use cloudhv_common::buildinfo as b;
    let revision: &'static str = Box::leak(
        format!(
            "{} (containerd-shimkit {}, containerd-shim {}; release bundles Cloud Hypervisor {})",
            b::COMMIT,
            b::SHIMKIT_VERSION,
            b::CONTAINERD_SHIM_VERSION,
            b::CH_VERSION
        )
        .into_boxed_str(),
    );
    sandbox::cli::shim_main::<CloudHvInstance>(
        "io.containerd.cloudhv.v1",
        sandbox::cli::Version {
            version: b::VERSION,
            revision,
        },
        Some(containerd_shimkit::Config {
            no_setup_logger: true,
            ..Default::default()
        }),
    );
}
