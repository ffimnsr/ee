//! Rope tests: equality and default metric.
use super::*;

#[test]
#[allow(clippy::eq_op)]
fn eq_small() {
    let a = Rope::from("a");
    let a2 = Rope::from("a");
    let b = Rope::from("b");
    let empty = Rope::from("");
    assert!(a == a2);
    assert!(a != b);
    assert!(a != empty);
    assert!(empty == empty);
    assert!(a.slice(0..0) == empty);
}

#[test]
fn eq_med() {
    let mut a = String::new();
    let mut b = String::new();
    let line_len = MAX_LEAF + MIN_LEAF - 1;
    for _ in 0..line_len {
        a.push('a');
        b.push('b');
    }
    a.push('\n');
    b.push('\n');
    let r = Rope::from(&a[..MAX_LEAF]);
    let r = r + Rope::from(String::from(&a[MAX_LEAF..]) + &b[..MIN_LEAF]);
    let r = r + Rope::from(&b[MIN_LEAF..]);

    let a_rope = Rope::from(&a);
    let b_rope = Rope::from(&b);
    assert!(r != a_rope);
    assert!(r.slice(..a.len()) == a_rope);
    assert!(r.slice(a.len()..) == b_rope);
    assert!(r == a_rope.clone() + b_rope.clone());
    assert!(r != b_rope + a_rope);
}

#[test]
fn line_offsets() {
    let rope = Rope::from("hi\ni'm\nfour\nlines");
    assert_eq!(rope.offset_of_line(0), 0);
    assert_eq!(rope.offset_of_line(1), 3);
    assert_eq!(rope.line_of_offset(0), 0);
    assert_eq!(rope.line_of_offset(3), 1);
    // interior of first line should be first line
    assert_eq!(rope.line_of_offset(1), 0);
    // interior of last line should be last line
    assert_eq!(rope.line_of_offset(15), 3);
    assert_eq!(rope.offset_of_line(4), rope.len());
}

#[test]
fn default_metric_test() {
    let rope = Rope::from("hi\ni'm\nfour\nlines\n");
    assert_eq!(
        rope.convert_metrics::<BaseMetric, LinesMetric>(rope.len()),
        rope.count::<LinesMetric>(rope.len())
    );
    assert_eq!(
        rope.convert_metrics::<LinesMetric, BaseMetric>(2),
        rope.count_base_units::<LinesMetric>(2)
    );
}
