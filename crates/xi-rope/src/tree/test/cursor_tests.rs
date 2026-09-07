//! Tree tests: cursor navigation and invariants.
use super::*;

#[test]
fn cursor_next_triangle() {
    let n = 2_000;
    let text = Rope::from(build_triangle(n));

    let mut cursor = Cursor::new(&text, 0);
    let mut prev_offset = cursor.pos();
    for i in 1..(n + 1) as usize {
        let offset = cursor.next::<LinesMetric>().expect("arrived at the end too soon");
        assert_eq!(offset - prev_offset, i);
        prev_offset = offset;
    }
    assert_eq!(cursor.next::<LinesMetric>(), None);
}

#[test]
fn node_is_empty() {
    let text = Rope::from(String::new());
    assert!(text.is_empty());
}

#[test]
fn cursor_next_empty() {
    let text = Rope::from(String::new());
    let mut cursor = Cursor::new(&text, 0);
    assert_eq!(cursor.next::<LinesMetric>(), None);
    assert_eq!(cursor.pos(), 0);
}

#[test]
fn cursor_iter() {
    let text: Rope = build_triangle(50).into();
    let mut cursor = Cursor::new(&text, 0);
    let mut manual = Vec::new();
    while let Some(nxt) = cursor.next::<LinesMetric>() {
        manual.push(nxt);
    }

    cursor.set(0);
    let auto = cursor.iter::<LinesMetric>().collect::<Vec<_>>();
    assert_eq!(manual, auto);
}

#[test]
fn cursor_next_misc() {
    cursor_next_for("toto");
    cursor_next_for("toto\n");
    cursor_next_for("toto\ntata");
    cursor_next_for("歴史\n科学的");
    cursor_next_for("\n歴史\n科学的\n");
    cursor_next_for(&build_triangle(100));
}

fn cursor_next_for(s: &str) {
    let r = Rope::from(s.to_owned());
    for i in 0..r.len() {
        let mut c = Cursor::new(&r, i);
        let it = c.next::<LinesMetric>();
        let pos = c.pos();
        assert!(s.as_bytes()[i..pos - 1].iter().all(|c| *c != b'\n'), "missed linebreak");
        if pos < s.len() {
            assert!(it.is_some(), "must be Some(_)");
            assert!(s.as_bytes()[pos - 1] == b'\n', "not a linebreak");
        } else if s.as_bytes()[s.len() - 1] == b'\n' {
            assert!(it.is_some(), "must be Some(_)");
        } else {
            assert!(it.is_none());
            assert!(c.get_leaf().is_none());
        }
    }
}

#[test]
fn cursor_prev_misc() {
    cursor_prev_for("toto");
    cursor_prev_for("a\na\n");
    cursor_prev_for("toto\n");
    cursor_prev_for("toto\ntata");
    cursor_prev_for("歴史\n科学的");
    cursor_prev_for("\n歴史\n科学的\n");
    cursor_prev_for(&build_triangle(100));
}

fn cursor_prev_for(s: &str) {
    let r = Rope::from(s.to_owned());
    for i in 0..r.len() {
        let mut c = Cursor::new(&r, i);
        let it = c.prev::<LinesMetric>();
        let pos = c.pos();

        //Should countain at most one linebreak
        assert!(
            s.as_bytes()[pos..i].iter().filter(|c| **c == b'\n').count() <= 1,
            "missed linebreak"
        );

        if i == 0 && s.as_bytes()[i] == b'\n' {
            assert_eq!(pos, 0);
        }

        if pos > 0 {
            assert!(it.is_some(), "must be Some(_)");
            assert!(s.as_bytes()[pos - 1] == b'\n', "not a linebreak");
        }
    }
}

#[test]
fn at_or_next() {
    let text: Rope = "this\nis\nalil\nstring".into();
    let mut cursor = Cursor::new(&text, 0);
    assert_eq!(cursor.at_or_next::<LinesMetric>(), Some(5));
    assert_eq!(cursor.at_or_next::<LinesMetric>(), Some(5));
    cursor.set(1);
    assert_eq!(cursor.at_or_next::<LinesMetric>(), Some(5));
    assert_eq!(cursor.at_or_prev::<LinesMetric>(), Some(5));
    cursor.set(6);
    assert_eq!(cursor.at_or_prev::<LinesMetric>(), Some(5));
    cursor.set(6);
    assert_eq!(cursor.at_or_next::<LinesMetric>(), Some(8));
    assert_eq!(cursor.at_or_next::<LinesMetric>(), Some(8));
}

#[test]
fn next_zero_measure_large() {
    let mut text = Rope::from("a");
    for _ in 0..24 {
        text = Node::concat(text.clone(), text);
        let mut cursor = Cursor::new(&text, 0);
        assert_eq!(cursor.next::<LinesMetric>(), None);
        // Test that cursor is properly invalidated and at end of text.
        assert_eq!(cursor.get_leaf(), None);
        assert_eq!(cursor.pos(), text.len());

        cursor.set(text.len());
        assert_eq!(cursor.prev::<LinesMetric>(), None);
        // Test that cursor is properly invalidated and at beginning of text.
        assert_eq!(cursor.get_leaf(), None);
        assert_eq!(cursor.pos(), 0);
    }
}

#[test]
fn prev_line_large() {
    let s: String = format!("{}{}", "\n", build_triangle(1000));
    let rope = Rope::from(s);
    let mut expected_pos = rope.len();
    let mut cursor = Cursor::new(&rope, rope.len());

    for i in (1..1001).rev() {
        expected_pos -= i;
        assert_eq!(expected_pos, cursor.prev::<LinesMetric>().unwrap());
    }

    assert_eq!(None, cursor.prev::<LinesMetric>());
}

#[test]
fn prev_line_small() {
    let empty_rope = Rope::from("\n");
    let mut cursor = Cursor::new(&empty_rope, empty_rope.len());
    assert_eq!(None, cursor.prev::<LinesMetric>());

    let rope = Rope::from("\n\n\n\n\n\n\n\n\n\n");
    cursor = Cursor::new(&rope, rope.len());
    let mut expected_pos = rope.len();
    for _ in (1..10).rev() {
        expected_pos -= 1;
        assert_eq!(expected_pos, cursor.prev::<LinesMetric>().unwrap());
    }

    assert_eq!(None, cursor.prev::<LinesMetric>());
}

#[test]
fn is_boundary_at_leaf_start_uses_previous_leaf() {
    let left = format!("{}\n", "a".repeat(511));
    let right = "b".repeat(511);
    let boundary = left.len();
    let rope = Node::concat(Rope::from(left), Rope::from(right));
    let mut cursor = Cursor::new(&rope, boundary);

    assert!(cursor.is_boundary::<LinesMetric>());
    assert_eq!(cursor.pos(), boundary);
    assert_eq!(cursor.get_leaf().map(|(_, offset)| offset), Some(0));
}

#[test]
fn set_can_step_into_adjacent_leaf() {
    let left = "a".repeat(511);
    let right = "b".repeat(511);
    let boundary = left.len();
    let rope = Node::concat(Rope::from(left), Rope::from(right));
    let mut cursor = Cursor::new(&rope, boundary - 1);

    cursor.set(boundary);

    assert_eq!(cursor.pos(), boundary);
    assert_eq!(cursor.get_leaf().map(|(_, offset)| offset), Some(0));
    assert_eq!(cursor.get_leaf().map(|(leaf, _)| leaf.len()), Some(511));
}

#[test]
fn balance_invariant() {
    let mut tb = TreeBuilder::<RopeInfo>::new();
    let leaves: Vec<String> = (0..1000).map(|i| i.to_string()).collect();
    tb.push_leaves(leaves);
    let tree = tb.build();
    println!("height {}", tree.height());
}
