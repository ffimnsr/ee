#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use xi_rpc::WriteTransport;
use xi_rpc::test_utils::test_channel;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "method", content = "params")]
enum FuzzRequest {
    NewView { file_path: Option<String> },
    Save { view_id: String, file_path: String },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "method", content = "params")]
enum FuzzNotification {
    CloseView { view_id: String },
    Ping,
}

#[derive(Arbitrary, Debug)]
struct RpcInput {
    message: String,
}

fuzz_target!(|input: RpcInput| {
    let (mut writer, mut reader) = test_channel();
    if writer.write_message(input.message.as_bytes()).is_err() {
        return;
    }

    let Some(result) = reader.next_timeout(Duration::ZERO) else {
        return;
    };

    let Ok(object) = result else {
        return;
    };

    let _ = object.get_id();
    let _ = object.get_method();

    if object.is_response() {
        let _ = object.into_response();
        return;
    }

    let _ = object.into_rpc::<FuzzNotification, FuzzRequest>();
});
