#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|case: fuzz_daemon::entry::TreeCase| {
    fuzz_daemon::entry::tree(case);
});
