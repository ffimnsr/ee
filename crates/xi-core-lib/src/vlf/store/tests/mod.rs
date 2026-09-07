//! VLF store tests: shared fixtures.
use std::io::Write;

use tempfile::NamedTempFile;

use super::super::pager::DEFAULT_MAX_READ_SIZE;
use super::*;
use crate::text_store::TextStore;
fn store_from(content: &[u8]) -> (VlfStore, NamedTempFile) {
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();
    let store = VlfStore::open_with_config(f.path(), 64, 1024 * 1024).unwrap();
    (store, f)
}
fn store_with_multibyte_at_boundary() -> (VlfStore, tempfile::NamedTempFile) {
    // '€' = 0xE2 0x82 0xAC (3 bytes)
    let content = b"abc\xE2\x82\xACxyz";
    let mut f = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut f, content).unwrap();
    std::io::Write::flush(&mut f).unwrap();
    // page_size=3 so page 0 = [0,3)="abc", page 1 = [3,6)="\xE2\x82\xAC", page 2 = [6,9)="xyz"
    let store = VlfStore::open_with_config(f.path(), 3, 1024 * 1024).unwrap();
    (store, f)
}

mod cache_tests;
mod edit_tests;
mod overlay_tests;
mod read_tests;
mod save_tests;
mod scan_tests;
mod viewport_tests;
