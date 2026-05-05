#![no_main]

use ee_fuzz::{CoreTextInput, run_core_text_input};
use libfuzzer_sys::fuzz_target;
fuzz_target!(|input: CoreTextInput| run_core_text_input(input));
