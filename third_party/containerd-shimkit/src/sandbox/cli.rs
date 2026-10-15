//! The entry point of a shim: `shim_main`.

use std::io::Write as _;
use std::path::PathBuf;

use containerd_shim::protos::protobuf::{Message, MessageField};
use containerd_shim::protos::types::introspection;
use containerd_shim::{Config, parse, run};

use crate::sandbox::{Instance, Shim};

/// The name and version the shim reports for `-version`.
pub struct Version {
    pub version: &'static str,
    pub revision: &'static str,
}

impl Default for Version {
    fn default() -> Self {
        Self {
            version: "<none>",
            revision: "<none>",
        }
    }
}

/// Main entry point for the shim.
///
/// containerd runs the shim binary three ways: `start` (make the server, print its address, exit), the server itself (the same binary
/// with `-socket`), and `delete`. `containerd_shim::run` tells them apart and, for the server, sets up the logger and serves the task
/// service of `Shim<I>`.
pub fn shim_main<I>(name: &str, version: Version, config: Option<Config>)
where
    I: 'static + Instance + Sync + Send,
{
    let os_args: Vec<_> = std::env::args_os().collect();

    let flags = parse(&os_args[1..]).unwrap();
    let argv0 = PathBuf::from(&os_args[0]);
    let argv0 = argv0.file_stem().unwrap_or_default().to_string_lossy();

    if flags.version {
        println!("{argv0}:");
        println!("  Runtime: {name}");
        println!("  Version: {}", version.version);
        println!("  Revision: {}", version.revision);
        println!();

        std::process::exit(0);
    }

    if flags.info {
        let info = introspection::RuntimeInfo {
            name: name.to_string(),
            version: MessageField::some(introspection::RuntimeVersion {
                version: version.version.to_string(),
                revision: version.revision.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let bytes = info
            .write_to_bytes()
            .expect("failed to serialize RuntimeInfo");
        std::io::stdout()
            .write_all(&bytes)
            .expect("failed to write RuntimeInfo");
        return;
    }

    run::<Shim<I>>(name, config);
}
