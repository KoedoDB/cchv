#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    containerd_shimkit::sandbox::shim::fuzz_entry::run(data);
});
