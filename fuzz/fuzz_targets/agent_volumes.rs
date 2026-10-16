#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|case: fuzz_agent::entry::VolumeCase| {
    fuzz_agent::entry::volumes(case);
});
