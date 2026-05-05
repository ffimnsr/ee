#![no_main]

use ee_fuzz::{RopeInput, run_rope_input};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: RopeInput| {
    run_rope_input(input);
});
