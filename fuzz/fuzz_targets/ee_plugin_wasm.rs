#![no_main]

//! Fuzz the v2 plugin manifest and wire payload parsers.
//!
//! Inputs never need the network, a user home, or a real plugin binary. The
//! Wasm guest artifact is exercised by the ignored runtime load test instead;
//! this target owns the untrusted JSON surface that precedes instantiation.

use ee_fuzz::{PluginInput, run_plugin_input};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: PluginInput| {
    run_plugin_input(input);
});
