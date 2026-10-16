#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|ops: Vec<fuzz_agent::entry::LogOp>| {
    fuzz_agent::entry::logs(ops);
});
