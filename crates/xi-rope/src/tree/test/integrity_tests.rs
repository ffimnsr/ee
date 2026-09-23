//! Tree tests: integrity/invariant assertions detect corruption.
use super::*;

#[test]
fn integrity_passes_on_valid_nodes() {
    let mut tb = TreeBuilder::<RopeInfo>::new();
    for i in 0..500u32 {
        tb.push_leaf(format!("line {i}\n"));
    }
    let tree = tb.build();
    tree.assert_integrity();
    tree.assert_invariants();
}

#[test]
fn integrity_detects_wrong_length() {
    let mut n = Node::<RopeInfo>::from_leaf("abc".to_string());
    Arc::make_mut(&mut n.0).len = 99;
    assert!(std::panic::catch_unwind(|| n.assert_integrity()).is_err());
}

#[test]
fn integrity_detects_wrong_leaf_info() {
    let mut n = Node::<RopeInfo>::from_leaf("abc\n".to_string());
    Arc::make_mut(&mut n.0).info.chars = 99;
    assert!(std::panic::catch_unwind(|| n.assert_integrity()).is_err());
}

#[test]
fn integrity_detects_wrong_child_height() {
    let left = Node::<RopeInfo>::from_leaf("a".repeat(600));
    let right = Node::<RopeInfo>::from_leaf("b".repeat(600));
    let mut n = Node::from_nodes(vec![left, right]);
    if let NodeVal::Internal(children) = &mut Arc::make_mut(&mut n.0).val {
        Arc::make_mut(&mut children[1].0).height = 9;
    } else {
        unreachable!();
    }
    assert!(std::panic::catch_unwind(|| n.assert_integrity()).is_err());
}

#[test]
fn integrity_detects_interior_info_mismatch() {
    let left = Node::<RopeInfo>::from_leaf("a".repeat(600));
    let right = Node::<RopeInfo>::from_leaf("b".repeat(600));
    let mut n = Node::from_nodes(vec![left, right]);
    Arc::make_mut(&mut n.0).info.chars += 1;
    assert!(std::panic::catch_unwind(|| n.assert_integrity()).is_err());
}
