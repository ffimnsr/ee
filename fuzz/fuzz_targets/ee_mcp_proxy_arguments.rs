#![no_main]

use ee_mcp::proxy::validate_tool_argument_size;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(serde_json::Value::Object(arguments)) = serde_json::from_slice(data) else {
        return;
    };
    let _ = validate_tool_argument_size(&arguments);
});
