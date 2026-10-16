#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|case: cloudhv_fuzz::shim::VolCase| {
    cloudhv_fuzz::shim::volumes(case);
});
