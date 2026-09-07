//! Delta tests: harness.
use crate::delta::{Builder, Delta, DeltaElement, DeltaRegion, Transformer};
use crate::interval::Interval;
use crate::rope::{Rope, RopeInfo};
use crate::test_helpers::find_deletions;
use proptest::prelude::*;
use proptest::string::string_regex;

const TEST_STR: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

type RawEdit = (usize, usize, String);

fn normalized_interval(base_len: usize, raw_start: usize, raw_end: usize) -> Interval {
    let start = if base_len == 0 { 0 } else { raw_start % (base_len + 1) };
    let end = start + (raw_end % (base_len + 1 - start));
    Interval::new(start, end)
}

fn apply_simple_edit(base: &str, raw_start: usize, raw_end: usize, insert: &str) -> String {
    let interval = normalized_interval(base.len(), raw_start, raw_end);
    format!("{}{}{}", &base[..interval.start()], insert, &base[interval.end()..])
}

fn build_script_delta(base: &str, edits: &[RawEdit]) -> (Delta<RopeInfo>, String) {
    let mut builder = Builder::new(base.len());
    let mut expected = String::new();
    let mut cursor = 0;

    for (raw_gap, raw_del_len, insert) in edits {
        let remaining = base.len() - cursor;
        let start = cursor + (raw_gap % (remaining + 1));
        let delete_len = raw_del_len % (base.len() + 1 - start);
        let end = start + delete_len;

        expected.push_str(&base[cursor..start]);
        expected.push_str(insert);

        let interval = Interval::new(start, end);
        if insert.is_empty() {
            builder.delete(interval);
        } else {
            builder.replace(interval, Rope::from(insert.as_str()));
        }

        cursor = end;
    }

    expected.push_str(&base[cursor..]);
    (builder.build(), expected)
}

mod delta_tests;

#[cfg(all(test, feature = "serde"))]
mod serde_tests;
