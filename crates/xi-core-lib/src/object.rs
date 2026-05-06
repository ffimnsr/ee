use std::path::Path;

use tree_sitter::{Node, Parser};
use xi_rope::Rope;

use crate::selection::{SelRegion, Selection};
use crate::tree_sitter_support::ts_language_for_name;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyntaxSelectionAction {
    Expand,
    Shrink,
    SelectPrevSibling,
    SelectNextSibling,
    SelectAllSiblings,
    SelectAllChildren,
    MoveParentNodeStart,
    MoveParentNodeEnd,
}

impl SyntaxSelectionAction {
    pub(crate) fn method_name(self) -> &'static str {
        match self {
            SyntaxSelectionAction::Expand => "expand_selection",
            SyntaxSelectionAction::Shrink => "shrink_selection",
            SyntaxSelectionAction::SelectPrevSibling => "select_prev_sibling",
            SyntaxSelectionAction::SelectNextSibling => "select_next_sibling",
            SyntaxSelectionAction::SelectAllSiblings => "select_all_siblings",
            SyntaxSelectionAction::SelectAllChildren => "select_all_children",
            SyntaxSelectionAction::MoveParentNodeStart => "move_parent_node_start",
            SyntaxSelectionAction::MoveParentNodeEnd => "move_parent_node_end",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyntaxSelectionError {
    SyntaxTreeUnavailable,
    ParentAbsent,
    ChildAbsent,
    SiblingAbsent,
    ChildrenAbsent,
}

impl SyntaxSelectionError {
    pub(crate) fn message(self) -> &'static str {
        match self {
            SyntaxSelectionError::SyntaxTreeUnavailable => "no syntax tree available",
            SyntaxSelectionError::ParentAbsent => "no parent syntax node",
            SyntaxSelectionError::ChildAbsent => "no child syntax node",
            SyntaxSelectionError::SiblingAbsent => "no sibling syntax node",
            SyntaxSelectionError::ChildrenAbsent => "no child syntax nodes",
        }
    }
}

pub(crate) fn apply_syntax_selection(
    text: &Rope,
    current: &Selection,
    history: &mut Vec<Selection>,
    language_name: &str,
    file_path: Option<&Path>,
    action: SyntaxSelectionAction,
) -> Result<Selection, SyntaxSelectionError> {
    if matches!(action, SyntaxSelectionAction::Shrink) {
        if let Some(previous) = history.pop() {
            if selection_contains(current, &previous) {
                return Ok(previous);
            }
            history.clear();
        }
    }

    let Some(language) =
        ts_language_for_name(language_name).or_else(|| syntax_language_for_path(file_path))
    else {
        return Err(SyntaxSelectionError::SyntaxTreeUnavailable);
    };
    let mut parser = Parser::new();
    parser.set_language(&language).map_err(|_| SyntaxSelectionError::SyntaxTreeUnavailable)?;

    let source = text.to_string();
    let Some(tree) = parser.parse(&source, None) else {
        return Err(SyntaxSelectionError::SyntaxTreeUnavailable);
    };
    let root = tree.root_node();
    let text_len = source.len();

    let next = match action {
        SyntaxSelectionAction::Expand => expand_selection(current, root, text_len)?,
        SyntaxSelectionAction::Shrink => shrink_selection(current, root, text_len)?,
        SyntaxSelectionAction::SelectPrevSibling => select_sibling(current, root, text_len, true)?,
        SyntaxSelectionAction::SelectNextSibling => select_sibling(current, root, text_len, false)?,
        SyntaxSelectionAction::SelectAllSiblings => select_all_siblings(current, root, text_len)?,
        SyntaxSelectionAction::SelectAllChildren => select_all_children(current, root, text_len)?,
        SyntaxSelectionAction::MoveParentNodeStart => {
            move_parent_node_boundary(current, root, text_len, false)?
        }
        SyntaxSelectionAction::MoveParentNodeEnd => {
            move_parent_node_boundary(current, root, text_len, true)?
        }
    };

    if matches!(action, SyntaxSelectionAction::Expand) && !selection_eq(&next, current) {
        history.push(current.clone());
    }

    Ok(next)
}

fn syntax_language_for_path(path: Option<&Path>) -> Option<tree_sitter::Language> {
    let ext = path?.extension()?.to_str()?;
    match ext {
        "rs" => ts_language_for_name("Rust"),
        "py" => ts_language_for_name("Python"),
        _ => None,
    }
}

fn expand_selection(
    current: &Selection,
    root: Node<'_>,
    text_len: usize,
) -> Result<Selection, SyntaxSelectionError> {
    let mut selection = Selection::new();
    for &region in current.iter() {
        let (from, to, node) =
            node_for_region(root, region, text_len).ok_or(SyntaxSelectionError::ParentAbsent)?;
        let mut parent = node;
        while parent.start_byte() == from && parent.end_byte() == to {
            parent = parent.parent().ok_or(SyntaxSelectionError::ParentAbsent)?;
        }
        selection.add_region(node_to_region(parent));
    }
    Ok(selection)
}

fn shrink_selection(
    current: &Selection,
    root: Node<'_>,
    text_len: usize,
) -> Result<Selection, SyntaxSelectionError> {
    let mut selection = Selection::new();
    for &region in current.iter() {
        let (_, _, node) =
            node_for_region(root, region, text_len).ok_or(SyntaxSelectionError::ChildAbsent)?;
        let child = first_named_child(node).ok_or(SyntaxSelectionError::ChildAbsent)?;
        selection.add_region(node_to_region(child));
    }
    Ok(selection)
}

fn select_sibling(
    current: &Selection,
    root: Node<'_>,
    text_len: usize,
    previous: bool,
) -> Result<Selection, SyntaxSelectionError> {
    let mut selection = Selection::new();
    for &region in current.iter() {
        let (_, _, node) =
            node_for_region(root, region, text_len).ok_or(SyntaxSelectionError::SiblingAbsent)?;
        let sibling = if previous {
            node.prev_named_sibling().or_else(|| node.prev_sibling())
        } else {
            node.next_named_sibling().or_else(|| node.next_sibling())
        }
        .filter(|sibling| sibling.end_byte() > sibling.start_byte())
        .ok_or(SyntaxSelectionError::SiblingAbsent)?;
        selection.add_region(node_to_region(sibling));
    }
    Ok(selection)
}

fn select_all_siblings(
    current: &Selection,
    root: Node<'_>,
    text_len: usize,
) -> Result<Selection, SyntaxSelectionError> {
    let mut selection = Selection::new();
    let mut found_any = false;

    for &region in current.iter() {
        let (_, _, node) =
            node_for_region(root, region, text_len).ok_or(SyntaxSelectionError::SiblingAbsent)?;
        let parent = node.parent().ok_or(SyntaxSelectionError::SiblingAbsent)?;
        let mut cursor = parent.walk();
        let mut has_sibling = false;
        for child in parent.named_children(&mut cursor) {
            if child.end_byte() <= child.start_byte() {
                continue;
            }
            has_sibling = true;
            selection.add_range_distinct(node_to_region(child));
        }
        if !has_sibling {
            return Err(SyntaxSelectionError::SiblingAbsent);
        }
        found_any = true;
    }

    if !found_any || selection.is_empty() {
        return Err(SyntaxSelectionError::SiblingAbsent);
    }

    Ok(selection)
}

fn select_all_children(
    current: &Selection,
    root: Node<'_>,
    text_len: usize,
) -> Result<Selection, SyntaxSelectionError> {
    let mut selection = Selection::new();
    let mut found_any = false;

    for &region in current.iter() {
        let (_, _, node) =
            node_for_region(root, region, text_len).ok_or(SyntaxSelectionError::ChildrenAbsent)?;
        let mut cursor = node.walk();
        let mut has_child = false;
        for child in node.named_children(&mut cursor) {
            if child.end_byte() <= child.start_byte() {
                continue;
            }
            has_child = true;
            selection.add_range_distinct(node_to_region(child));
        }
        if !has_child {
            return Err(SyntaxSelectionError::ChildrenAbsent);
        }
        found_any = true;
    }

    if !found_any || selection.is_empty() {
        return Err(SyntaxSelectionError::ChildrenAbsent);
    }

    Ok(selection)
}

fn move_parent_node_boundary(
    current: &Selection,
    root: Node<'_>,
    text_len: usize,
    end: bool,
) -> Result<Selection, SyntaxSelectionError> {
    let mut selection = Selection::new();

    for &region in current.iter() {
        let (_, _, node) =
            node_for_region(root, region, text_len).ok_or(SyntaxSelectionError::ParentAbsent)?;
        let parent = node.parent().ok_or(SyntaxSelectionError::ParentAbsent)?;
        let offset = if end { parent.end_byte() } else { parent.start_byte() };
        selection.add_region(SelRegion::caret(offset));
    }

    Ok(selection)
}

fn selection_contains(outer: &Selection, inner: &Selection) -> bool {
    outer.len() == inner.len()
        && outer.iter().zip(inner.iter()).all(|(current, previous)| {
            current.min() <= previous.min() && current.max() >= previous.max()
        })
}

fn selection_eq(left: &Selection, right: &Selection) -> bool {
    left.len() == right.len()
        && left.iter().zip(right.iter()).all(|(lhs, rhs)| {
            lhs.start == rhs.start
                && lhs.end == rhs.end
                && lhs.horiz == rhs.horiz
                && lhs.affinity == rhs.affinity
        })
}

fn node_for_region<'a>(
    root: Node<'a>,
    region: SelRegion,
    text_len: usize,
) -> Option<(usize, usize, Node<'a>)> {
    if text_len == 0 {
        return None;
    }

    let from = region.min().min(text_len);
    let to = region.max().min(text_len);

    let mut search_start = from.min(text_len.saturating_sub(1));
    let mut search_end = if region.is_caret() { from.saturating_add(1) } else { to }.min(text_len);

    if search_end <= search_start {
        if search_start == text_len {
            search_start = text_len.saturating_sub(1);
        }
        search_end = (search_start + 1).min(text_len);
    }

    root.descendant_for_byte_range(search_start, search_end).map(|node| (from, to, node))
}

fn first_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.end_byte() > child.start_byte())
        .or_else(|| first_child(node))
}

fn first_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).find(|child| child.end_byte() > child.start_byte())
}

fn node_to_region(node: Node<'_>) -> SelRegion {
    SelRegion::new(node.start_byte(), node.end_byte())
}

#[cfg(test)]
mod tests {
    use super::{
        SyntaxSelectionAction, SyntaxSelectionError, apply_syntax_selection, node_to_region,
    };
    use crate::selection::{SelRegion, Selection};
    use crate::tree_sitter_support::ts_language_for_name;
    use tree_sitter::Parser;
    use xi_rope::Rope;

    fn select_range(start: usize, end: usize) -> Selection {
        Selection::new_simple(SelRegion::new(start, end))
    }

    fn rust_region(source: &str, needle: &str) -> SelRegion {
        let mut parser = Parser::new();
        let language = ts_language_for_name("Rust").unwrap();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let start = source.find(needle).unwrap();
        let end = start + needle.len();
        let node = tree.root_node().descendant_for_byte_range(start, end).unwrap();
        node_to_region(node)
    }

    #[test]
    fn expand_and_shrink_use_history() {
        let source = "fn main() { foo(bar); }";
        let text = Rope::from(source);
        let mut history = Vec::new();
        let current = select_range(source.find("bar").unwrap(), source.find("bar").unwrap() + 3);

        let expanded = apply_syntax_selection(
            &text,
            &current,
            &mut history,
            "Rust",
            None,
            SyntaxSelectionAction::Expand,
        )
        .unwrap();

        assert_eq!(history.len(), 1);
        assert!(super::selection_eq(&history[0], &current));
        assert!(!super::selection_eq(&expanded, &current));

        let shrunk = apply_syntax_selection(
            &text,
            &expanded,
            &mut history,
            "Rust",
            None,
            SyntaxSelectionAction::Shrink,
        )
        .unwrap();

        assert!(super::selection_eq(&shrunk, &current));
    }

    #[test]
    fn select_next_and_prev_sibling_follow_named_nodes() {
        let source = "fn main() { [foo, bar, baz]; }";
        let text = Rope::from(source);
        let mut history = Vec::new();
        let current = select_range(source.find("bar").unwrap(), source.find("bar").unwrap() + 3);

        let next = apply_syntax_selection(
            &text,
            &current,
            &mut history,
            "Rust",
            None,
            SyntaxSelectionAction::SelectNextSibling,
        )
        .unwrap();
        assert!(super::selection_eq(
            &next,
            &select_range(source.find("baz").unwrap(), source.find("baz").unwrap() + 3)
        ));

        let prev = apply_syntax_selection(
            &text,
            &current,
            &mut history,
            "Rust",
            None,
            SyntaxSelectionAction::SelectPrevSibling,
        )
        .unwrap();
        assert!(super::selection_eq(
            &prev,
            &select_range(source.find("foo").unwrap(), source.find("foo").unwrap() + 3)
        ));
    }

    #[test]
    fn select_all_siblings_collects_named_peers() {
        let source = "fn main() { [foo, bar, baz]; }";
        let text = Rope::from(source);
        let mut history = Vec::new();
        let current = select_range(source.find("bar").unwrap(), source.find("bar").unwrap() + 3);

        let siblings = apply_syntax_selection(
            &text,
            &current,
            &mut history,
            "Rust",
            None,
            SyntaxSelectionAction::SelectAllSiblings,
        )
        .unwrap();

        let expected = vec![
            select_range(source.find("foo").unwrap(), source.find("foo").unwrap() + 3),
            select_range(source.find("bar").unwrap(), source.find("bar").unwrap() + 3),
            select_range(source.find("baz").unwrap(), source.find("baz").unwrap() + 3),
        ];
        assert_eq!(siblings.len(), expected.len());
        for selection in expected {
            assert!(siblings.iter().any(|region| {
                region.min() == selection[0].min() && region.max() == selection[0].max()
            }));
        }
    }

    #[test]
    fn select_all_children_collects_named_children() {
        let source = "fn main() { [foo, bar, baz]; }";
        let text = Rope::from(source);
        let mut history = Vec::new();
        let array = rust_region(source, "[foo, bar, baz]");
        let current = select_range(array.min(), array.max());

        let children = apply_syntax_selection(
            &text,
            &current,
            &mut history,
            "Rust",
            None,
            SyntaxSelectionAction::SelectAllChildren,
        )
        .unwrap();

        assert_eq!(children.len(), 3);
        for needle in ["foo", "bar", "baz"] {
            let start = source.find(needle).unwrap();
            let end = start + needle.len();
            assert!(children.iter().any(|region| region.min() == start && region.max() == end));
        }
    }

    #[test]
    fn unsupported_language_reports_missing_tree() {
        let text = Rope::from("plain text");
        let mut history = Vec::new();
        let selection = select_range(0, 5);

        let err = apply_syntax_selection(
            &text,
            &selection,
            &mut history,
            "Text",
            None,
            SyntaxSelectionAction::Expand,
        )
        .unwrap_err();

        assert_eq!(err, SyntaxSelectionError::SyntaxTreeUnavailable);
    }

    #[test]
    fn move_parent_node_boundaries_return_parent_carets() {
        let source = "fn main() { foo(bar); }";
        let text = Rope::from(source);
        let mut history = Vec::new();
        let current = select_range(source.find("bar").unwrap(), source.find("bar").unwrap() + 3);

        let start = apply_syntax_selection(
            &text,
            &current,
            &mut history,
            "Rust",
            None,
            SyntaxSelectionAction::MoveParentNodeStart,
        )
        .unwrap();
        assert_eq!(start.len(), 1);
        let open_paren = source.rfind('(').unwrap();
        assert_eq!(start[0].min(), open_paren);
        assert_eq!(start[0].max(), open_paren);

        let end = apply_syntax_selection(
            &text,
            &current,
            &mut history,
            "Rust",
            None,
            SyntaxSelectionAction::MoveParentNodeEnd,
        )
        .unwrap();
        let close_paren = source.rfind(')').unwrap() + 1;
        assert_eq!(end.len(), 1);
        assert_eq!(end[0].min(), close_paren);
        assert_eq!(end[0].max(), close_paren);
    }
}
