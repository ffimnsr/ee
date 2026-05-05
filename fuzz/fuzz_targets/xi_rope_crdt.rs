#![no_main]

use ee_fuzz::{run_rope_input, RopeInput};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: RopeInput| {
    run_rope_input(input);
});
