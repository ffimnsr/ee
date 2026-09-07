//! Delta tests: serde round trips.

use crate::rope::{Rope, RopeInfo};
use crate::{Delta, Interval};
use serde_json;

const TEST_STR: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

#[test]
fn delta_serde() {
    let d = Delta::simple_edit(Interval::new(10, 12), Rope::from("+"), TEST_STR.len());
    let ser = serde_json::to_value(d.clone()).expect("serialize failed");
    eprintln!("{:?}", ser);
    let de: Delta<RopeInfo> = serde_json::from_value(ser).expect("deserialize failed");
    assert_eq!(d.apply_to_string(TEST_STR), de.apply_to_string(TEST_STR));
}
