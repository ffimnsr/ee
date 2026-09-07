//! Rope tests: line iteration and large-appends.
use super::*;

#[test]
fn lines_raw_small() {
    let a = Rope::from("a\nb\nc");
    assert_eq!(vec!["a\n", "b\n", "c"], a.lines_raw(..).collect::<Vec<_>>());
    assert_eq!(vec!["a\n", "b\n", "c"], a.lines_raw(..).collect::<Vec<_>>());

    let a = Rope::from("a\nb\n");
    assert_eq!(vec!["a\n", "b\n"], a.lines_raw(..).collect::<Vec<_>>());

    let a = Rope::from("\n");
    assert_eq!(vec!["\n"], a.lines_raw(..).collect::<Vec<_>>());

    let a = Rope::from("");
    assert_eq!(0, a.lines_raw(..).count());
}

#[test]
fn lines_small() {
    let a = Rope::from("a\nb\nc");
    assert_eq!(vec!["a", "b", "c"], a.lines(..).collect::<Vec<_>>());
    assert_eq!(String::from(&a).lines().collect::<Vec<_>>(), a.lines(..).collect::<Vec<_>>());

    let a = Rope::from("a\nb\n");
    assert_eq!(vec!["a", "b"], a.lines(..).collect::<Vec<_>>());
    assert_eq!(String::from(&a).lines().collect::<Vec<_>>(), a.lines(..).collect::<Vec<_>>());

    let a = Rope::from("\n");
    assert_eq!(vec![""], a.lines(..).collect::<Vec<_>>());
    assert_eq!(String::from(&a).lines().collect::<Vec<_>>(), a.lines(..).collect::<Vec<_>>());

    let a = Rope::from("");
    assert_eq!(0, a.lines(..).count());
    assert_eq!(String::from(&a).lines().collect::<Vec<_>>(), a.lines(..).collect::<Vec<_>>());

    let a = Rope::from("a\r\nb\r\nc");
    assert_eq!(vec!["a", "b", "c"], a.lines(..).collect::<Vec<_>>());
    assert_eq!(String::from(&a).lines().collect::<Vec<_>>(), a.lines(..).collect::<Vec<_>>());

    let a = Rope::from("a\rb\rc");
    assert_eq!(vec!["a\rb\rc"], a.lines(..).collect::<Vec<_>>());
    assert_eq!(String::from(&a).lines().collect::<Vec<_>>(), a.lines(..).collect::<Vec<_>>());
}

#[test]
fn lines_med() {
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
    //println!("{:?}", r.iter_chunks().collect::<Vec<_>>());

    assert_eq!(vec![a.as_str(), b.as_str()], r.lines_raw(..).collect::<Vec<_>>());
    assert_eq!(vec![&a[..line_len], &b[..line_len]], r.lines(..).collect::<Vec<_>>());
    assert_eq!(String::from(&r).lines().collect::<Vec<_>>(), r.lines(..).collect::<Vec<_>>());

    // additional tests for line indexing
    assert_eq!(a.len(), r.offset_of_line(1));
    assert_eq!(r.len(), r.offset_of_line(2));
    assert_eq!(0, r.line_of_offset(a.len() - 1));
    assert_eq!(1, r.line_of_offset(a.len()));
    assert_eq!(1, r.line_of_offset(r.len() - 1));
    assert_eq!(2, r.line_of_offset(r.len()));
}

#[test]
fn append_large() {
    let mut a = Rope::from("");
    let mut b = String::new();
    for i in 0..5_000 {
        let c = i.to_string() + "\n";
        b.push_str(&c);
        a = a + Rope::from(&c);
    }
    assert_eq!(b, String::from(a));
}
