#![no_main]

use ee_fuzz::{CompareInput, run_compare_input};
use libfuzzer_sys::fuzz_target;
fuzz_target!(|input: CompareInput| run_compare_input(input));
