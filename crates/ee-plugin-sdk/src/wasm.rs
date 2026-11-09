//! Wasm guest SDK.
//!
//! Guests export a fixed ABI (`plugin_alloc`, `plugin_dealloc`, `plugin_apply`,
//! `plugin_event`, `plugin_command`, `plugin_contribution`) and reach the host
//! only through the `env` imports defined by `ee-plugin-runtime`. Imports are
//! linked only when the manifest declares the matching permission, so a guest
//! must tolerate a missing import at build time only if it never calls it.
//!
//! ## ABI
//!
//! Every host call takes a pointer/length pair into guest memory. Response
//! imports return a packed `i64` of `(ptr << 32) | len`; zero means "no result".
//! Exports return the same packed shape, and the host calls `plugin_dealloc` on
//! the returned buffer after copying it.

#![allow(unsafe_code)]

/// One guest plugin implementation.
///
/// Implementations are stored in a `OnceLock<Mutex<_>>` by
/// [`wasm_plugin!`](crate::wasm_plugin). Wasm guests are single-threaded, but a
/// mutex keeps the macro usable without `static mut`.
pub trait WasmPlugin: Default {
    /// Called once with the serialized `InitializeParams`.
    fn apply(&mut self, _input: &[u8]) -> Vec<u8> {
        Vec::new()
    }

    /// Called with serialized `EventParams` (or another host event payload).
    fn event(&mut self, _input: &[u8]) -> Vec<u8> {
        Vec::new()
    }

    /// Called with serialized `CommandParams`; return serialized `PluginResponse`.
    fn command(&mut self, _input: &[u8]) -> Vec<u8> {
        Vec::new()
    }

    /// Called with serialized `ContributionRequestParams`; return a serialized
    /// `ContributionEnvelope`.
    fn contribution(&mut self, _input: &[u8]) -> Vec<u8> {
        Vec::new()
    }
}

#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn host_event(ptr: *const u8, len: u32) -> i32;
    fn host_command(ptr: *const u8, len: u32) -> i64;
    fn host_service_call(ptr: *const u8, len: u32) -> i64;
    fn host_contribute(ptr: *const u8, len: u32) -> i32;
    fn host_health(ptr: *const u8, len: u32) -> i32;
}

/// Submit an `EffectEnvelope` to the host.
pub fn emit_effects(json: &str) -> Result<(), i32> {
    let code = unsafe { host_event(json.as_ptr(), json.len() as u32) };
    if code == 0 { Ok(()) } else { Err(code) }
}

/// Report health to the host.
pub fn report_health(json: &str) -> Result<(), i32> {
    let code = unsafe { host_health(json.as_ptr(), json.len() as u32) };
    if code == 0 { Ok(()) } else { Err(code) }
}

/// Submit a `ContributionEnvelope` to the host.
pub fn contribute(json: &str) -> Result<(), i32> {
    let code = unsafe { host_contribute(json.as_ptr(), json.len() as u32) };
    if code == 0 { Ok(()) } else { Err(code) }
}

/// Invoke an editor command through `editor.commands@1`.
pub fn invoke_command(json: &str) -> Option<String> {
    decode_response(unsafe { host_command(json.as_ptr(), json.len() as u32) })
}

/// Call any host service declared by the manifest.
pub fn call_service(json: &str) -> Option<String> {
    decode_response(unsafe { host_service_call(json.as_ptr(), json.len() as u32) })
}

/// Copy `bytes` out of guest memory for a guest export.
pub fn read_input(ptr: i32, len: i32) -> Vec<u8> {
    if ptr <= 0 || len <= 0 {
        return Vec::new();
    }
    // SAFETY: the host writes exactly `len` initialized bytes at `ptr` before
    // calling an export; the len is checked non-negative.
    unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize).to_vec() }
}

/// Pack `bytes` for a guest export.
///
/// The allocation is owned by the host after return; the host calls
/// `plugin_dealloc` once it has copied the bytes.
pub fn export_bytes(bytes: Vec<u8>) -> i64 {
    if bytes.is_empty() {
        return 0;
    }
    let boxed = bytes.into_boxed_slice();
    let len = boxed.len();
    // SAFETY: ownership moves to the host, which reconstructs the box in
    // `plugin_dealloc` using the same pointer and length.
    let ptr = Box::into_raw(boxed) as *mut u8;
    ((ptr as u64) << 32 | len as u64) as i64
}

fn decode_response(packed: i64) -> Option<String> {
    if packed == 0 {
        return None;
    }
    let ptr = (packed as u64 >> 32) as u32;
    let len = (packed as u64 as u32) as usize;
    if ptr == 0 || len == 0 {
        return None;
    }
    // SAFETY: the host allocates the response through `plugin_alloc`, which
    // returns a live buffer of at least `len` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Allocate guest memory for the host. The host writes exactly `len` bytes.
#[unsafe(no_mangle)]
pub extern "C" fn plugin_alloc(len: u32) -> i32 {
    let mut buffer = Vec::<u8>::with_capacity(len as usize);
    let ptr = buffer.as_mut_ptr();
    // The buffer is intentionally leaked: the host copies an export result and
    // then calls `plugin_dealloc`; input buffers are reclaimed with the guest
    // instance when the module is dropped.
    std::mem::forget(buffer);
    ptr as i32
}

/// Release a buffer previously returned by [`export_bytes`].
#[unsafe(no_mangle)]
pub extern "C" fn plugin_dealloc(ptr: i32, len: i32) {
    if ptr <= 0 || len <= 0 {
        return;
    }
    let raw = std::ptr::slice_from_raw_parts_mut(ptr as *mut u8, len as usize);
    // SAFETY: `export_bytes` created this boxed slice with the same pointer and
    // length; the host only calls this once per returned buffer.
    unsafe { drop(Box::from_raw(raw)) };
}

/// Export the guest ABI for a [`WasmPlugin`] implementation.
#[macro_export]
macro_rules! wasm_plugin {
    ($ty:ty) => {
        static PLUGIN: ::std::sync::OnceLock<::std::sync::Mutex<$ty>> =
            ::std::sync::OnceLock::new();

        fn plugin_instance() -> &'static ::std::sync::Mutex<$ty> {
            PLUGIN.get_or_init(|| ::std::sync::Mutex::new(<$ty>::default()))
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn plugin_apply(ptr: i32, len: i32) -> i64 {
            let input = $crate::wasm::read_input(ptr, len);
            let output = plugin_instance().lock().unwrap().apply(&input);
            $crate::wasm::export_bytes(output)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn plugin_event(ptr: i32, len: i32) -> i64 {
            let input = $crate::wasm::read_input(ptr, len);
            let output = plugin_instance().lock().unwrap().event(&input);
            $crate::wasm::export_bytes(output)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn plugin_command(ptr: i32, len: i32) -> i64 {
            let input = $crate::wasm::read_input(ptr, len);
            let output = plugin_instance().lock().unwrap().command(&input);
            $crate::wasm::export_bytes(output)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn plugin_contribution(ptr: i32, len: i32) -> i64 {
            let input = $crate::wasm::read_input(ptr, len);
            let output = plugin_instance().lock().unwrap().contribution(&input);
            $crate::wasm::export_bytes(output)
        }
    };
}
