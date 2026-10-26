//! What a binary was built from, for `--version` (the values come from `build.rs`).

/// Our version: the release tag (`v0.3.1`) for a release build, `git describe` for any other.
pub const VERSION: &str = env!("CLOUDHV_BUILD_VERSION");
/// The commit that was built.
pub const COMMIT: &str = env!("CLOUDHV_BUILD_COMMIT");
/// The pinned `containerd-shimkit` (version and commit), from `Cargo.lock`. Only the shim uses it.
pub const SHIMKIT_VERSION: &str = env!("CLOUDHV_SHIMKIT_VERSION");
/// The `containerd-shim` crate, from `Cargo.lock`. Only the shim uses it.
pub const CONTAINERD_SHIM_VERSION: &str = env!("CLOUDHV_CONTAINERD_SHIM_VERSION");
/// The Cloud Hypervisor, crun and guest kernel the release bundles (`hacks/versions.env`). Not linked into the binary: they are
/// what the release was made with.
pub const CH_VERSION: &str = env!("CLOUDHV_CH_VERSION");
pub const CRUN_VERSION: &str = env!("CLOUDHV_CRUN_VERSION");
pub const KERNEL_VERSION: &str = env!("CLOUDHV_KERNEL_VERSION");

/// The text of `--version`: the component, our version and commit, then the lines the component names (`extra`).
pub fn report(component: &str, extra: &[(&str, &str)]) -> String {
    let mut s = format!("{component} {VERSION}\ncommit: {COMMIT}\n");
    for (k, v) in extra {
        s.push_str(&format!("{k}: {v}\n"));
    }
    s
}

/// True when the first argument asks for the version (`--version`, `-V`, `-version`). `-v` is not taken: the shim's parser has it.
pub fn wants_version(arg: Option<&str>) -> bool {
    matches!(arg, Some("--version") | Some("-V") | Some("-version"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_names_the_component_version_and_commit() {
        let r = report("cloudhv-x", &[("cloud-hypervisor", "v1")]);
        assert!(r.starts_with(&format!("cloudhv-x {VERSION}\ncommit: {COMMIT}\n")));
        assert!(r.ends_with("cloud-hypervisor: v1\n"));
    }

    #[test]
    fn the_pins_are_read() {
        // They are read from hacks/versions.env and Cargo.lock of this checkout.
        assert_ne!(CH_VERSION, "unknown");
        assert_ne!(CRUN_VERSION, "unknown");
        assert_ne!(KERNEL_VERSION, "unknown");
        assert_ne!(CONTAINERD_SHIM_VERSION, "unknown");
    }

    #[test]
    fn version_flags() {
        assert!(wants_version(Some("--version")));
        assert!(wants_version(Some("-V")));
        assert!(!wants_version(Some("/opt/cloudhv/daemon.json")));
        assert!(!wants_version(None));
    }
}
