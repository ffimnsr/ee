#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use xi_core_lib::config::ConfigManager;

#[derive(Arbitrary, Debug)]
struct ConfigInput {
    domain_case: u8,
    format_case: u8,
    language_name: String,
    payload: Vec<u8>,
}

fuzz_target!(|input: ConfigInput| {
    let payload = if input.payload.len() > 4096 { &input.payload[..4096] } else { &input.payload };
    ConfigManager::fuzz_apply_user_config_payload(
        input.domain_case,
        &input.language_name,
        input.format_case,
        payload,
    );
});
