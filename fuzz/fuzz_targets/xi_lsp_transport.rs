#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use std::io::Cursor;
use xi_lsp_lib::read_transport_message;

#[derive(Arbitrary, Debug)]
struct TransportInput {
    frame: Vec<u8>,
}

fuzz_target!(|input: TransportInput| {
    let mut reader = Cursor::new(input.frame);
    let _ = read_transport_message(&mut reader);
});
