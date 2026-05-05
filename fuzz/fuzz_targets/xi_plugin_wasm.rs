#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use std::path::Path;
use xi_core_lib::ConfigTable;
use xi_plugin_lib::wasm::WasmPluginRuntime;
use xi_plugin_lib::{ChunkCache, CoreProxy, Plugin, View};
use xi_rope::RopeDelta;

struct NoopPlugin;

impl Plugin for NoopPlugin {
    type Cache = ChunkCache;

    fn initialize(&mut self, _core: CoreProxy) {}

    fn update(
        &mut self,
        _view: &mut View<Self::Cache>,
        _delta: Option<&RopeDelta>,
        _edit_type: String,
        _author: String,
    ) {
    }

    fn did_save(&mut self, _view: &mut View<Self::Cache>, _old_path: Option<&Path>) {}

    fn did_close(&mut self, _view: &View<Self::Cache>) {}

    fn new_view(&mut self, _view: &mut View<Self::Cache>) {}

    fn config_changed(&mut self, _view: &mut View<Self::Cache>, _changes: &ConfigTable) {}
}

#[derive(Arbitrary, Debug)]
struct WasmInput {
    notification: Vec<u8>,
    request: Vec<u8>,
    guest_bytes: Vec<u8>,
}

#[cfg(target_arch = "wasm32")]
fn roundtrip_guest_bytes(bytes: &[u8]) -> Vec<u8> {
    use xi_plugin_lib::wasm::{alloc, dealloc, read_input};

    let ptr = alloc(bytes.len());
    unsafe {
        if !bytes.is_empty() {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        }
        let roundtrip = read_input(ptr, bytes.len() as u32).to_vec();
        dealloc(ptr, bytes.len());
        roundtrip
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn roundtrip_guest_bytes(bytes: &[u8]) -> Vec<u8> {
    bytes.to_vec()
}

fuzz_target!(|input: WasmInput| {
    let runtime = WasmPluginRuntime::new(NoopPlugin);
    let notification = if input.notification.len() > 4096 {
        &input.notification[..4096]
    } else {
        &input.notification
    };
    let request = if input.request.len() > 4096 { &input.request[..4096] } else { &input.request };
    let guest_bytes = if input.guest_bytes.len() > 4096 {
        &input.guest_bytes[..4096]
    } else {
        &input.guest_bytes
    };

    let _ = runtime.handle_notification(notification);
    let _ = runtime.handle_request(request);

    let roundtrip = roundtrip_guest_bytes(guest_bytes);
    assert_eq!(roundtrip, guest_bytes);
});
