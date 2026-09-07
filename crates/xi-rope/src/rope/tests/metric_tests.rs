//! Rope tests: line/offset lookups and bounds errors.
use super::*;

#[test]
fn line_of_offset_small() {
    let a = Rope::from("a\nb\nc");
    assert_eq!(0, a.line_of_offset(0));
    assert_eq!(0, a.line_of_offset(1));
    assert_eq!(1, a.line_of_offset(2));
    assert_eq!(1, a.line_of_offset(3));
    assert_eq!(2, a.line_of_offset(4));
    assert_eq!(2, a.line_of_offset(5));
    let b = a.slice(2..4);
    assert_eq!(0, b.line_of_offset(0));
    assert_eq!(0, b.line_of_offset(1));
    assert_eq!(1, b.line_of_offset(2));
}

#[test]
fn offset_of_line_small() {
    let a = Rope::from("a\nb\nc");
    assert_eq!(0, a.offset_of_line(0));
    assert_eq!(2, a.offset_of_line(1));
    assert_eq!(4, a.offset_of_line(2));
    assert_eq!(5, a.offset_of_line(3));
    let b = a.slice(2..4);
    assert_eq!(0, b.offset_of_line(0));
    assert_eq!(2, b.offset_of_line(1));
}
#[test]
#[should_panic]
fn line_of_offset_panic() {
    let rope = Rope::from("hi\ni'm\nfour\nlines");
    rope.line_of_offset(20);
}

#[test]
#[should_panic]
fn offset_of_line_panic() {
    let rope = Rope::from("hi\ni'm\nfour\nlines");
    rope.offset_of_line(5);
}

#[test]
fn try_line_of_offset_reports_bounds_error() {
    let rope = Rope::from("hi\ni'm\nfour\nlines");
    assert_eq!(
        rope.try_line_of_offset(20),
        Err(RopeError::OffsetOutOfBounds { offset: 20, len: rope.len() })
    );
}

#[test]
fn try_offset_of_line_reports_bounds_error() {
    let rope = Rope::from("hi\ni'm\nfour\nlines");
    assert_eq!(
        rope.try_offset_of_line(5),
        Err(RopeError::LineOutOfBounds { line: 5, max_line: 4 })
    );
}

#[test]
fn try_slice_reports_bounds_error() {
    let rope = Rope::from("hello");
    assert_eq!(
        rope.try_slice(0..10),
        Err(RopeError::IntervalOutOfBounds { start: 0, end: 10, len: 5 })
    );
}
