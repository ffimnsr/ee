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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyntaxNavigationTarget {
    Function,
    Class,
    Parameter,
    Comment,
    Test,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SyntaxNavigationAction {
    pub(crate) target: SyntaxNavigationTarget,
    pub(crate) forward: bool,
}

impl SyntaxNavigationAction {
    pub(crate) const fn new(target: SyntaxNavigationTarget, forward: bool) -> Self {
        Self { target, forward }
    }

    pub(crate) fn method_name(self) -> &'static str {
        match (self.target, self.forward) {
            (SyntaxNavigationTarget::Function, true) => "goto_next_function",
            (SyntaxNavigationTarget::Function, false) => "goto_prev_function",
            (SyntaxNavigationTarget::Class, true) => "goto_next_class",
            (SyntaxNavigationTarget::Class, false) => "goto_prev_class",
            (SyntaxNavigationTarget::Parameter, true) => "goto_next_parameter",
            (SyntaxNavigationTarget::Parameter, false) => "goto_prev_parameter",
            (SyntaxNavigationTarget::Comment, true) => "goto_next_comment",
            (SyntaxNavigationTarget::Comment, false) => "goto_prev_comment",
            (SyntaxNavigationTarget::Test, true) => "goto_next_test",
            (SyntaxNavigationTarget::Test, false) => "goto_prev_test",
        }
    }
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
    NavigationTargetAbsent,
}

impl SyntaxSelectionError {
    pub(crate) fn message(self) -> &'static str {
        match self {
            SyntaxSelectionError::SyntaxTreeUnavailable => "no syntax tree available",
            SyntaxSelectionError::ParentAbsent => "no parent syntax node",
            SyntaxSelectionError::ChildAbsent => "no child syntax node",
            SyntaxSelectionError::SiblingAbsent => "no sibling syntax node",
            SyntaxSelectionError::ChildrenAbsent => "no child syntax nodes",
            SyntaxSelectionError::NavigationTargetAbsent => "no matching syntax node",
        }
    }
}

pub(crate) fn apply_syntax_navigation(
    text: &Rope,
    current: &Selection,
    language_name: &str,
    file_path: Option<&Path>,
    action: SyntaxNavigationAction,
) -> Result<Selection, SyntaxSelectionError> {
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
    let mut selection = Selection::new();

    for &region in current.iter() {
        let cursor = region.end.min(text_len);
        let target = find_navigation_target(root, &source, action, cursor)
            .ok_or(SyntaxSelectionError::NavigationTargetAbsent)?;
        selection.add_region(SelRegion::caret(target.start_byte()));
    }

    Ok(selection)
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

fn find_navigation_target<'a>(
    root: Node<'a>,
    source: &str,
    action: SyntaxNavigationAction,
    cursor: usize,
) -> Option<Node<'a>> {
    let mut matches = Vec::new();
    collect_navigation_nodes(root, source, action.target, &mut matches);
    matches.sort_by_key(|node| (node.start_byte(), node.end_byte()));

    if action.forward {
        matches
            .iter()
            .copied()
            .find(|node| node.start_byte() > cursor)
            .or_else(|| matches.first().copied())
    } else {
        matches
            .iter()
            .rev()
            .copied()
            .find(|node| node.end_byte() <= cursor)
            .or_else(|| matches.last().copied())
    }
}

fn collect_navigation_nodes<'a>(
    node: Node<'a>,
    source: &str,
    target: SyntaxNavigationTarget,
    out: &mut Vec<Node<'a>>,
) {
    if node_matches_target(node, source, target) {
        out.push(node);
    }

    for index in 0..node.child_count() {
        if let Some(child) = node.child(index as u32) {
            collect_navigation_nodes(child, source, target, out);
        }
    }
}

fn node_matches_target(node: Node<'_>, source: &str, target: SyntaxNavigationTarget) -> bool {
    match target {
        SyntaxNavigationTarget::Function => is_function_node(node),
        SyntaxNavigationTarget::Class => is_class_node(node),
        SyntaxNavigationTarget::Parameter => is_parameter_node(node),
        SyntaxNavigationTarget::Comment => node.kind().contains("comment"),
        SyntaxNavigationTarget::Test => is_test_node(node, source),
    }
}

fn is_function_node(node: Node<'_>) -> bool {
    match node.kind() {
        "function_item" | "function_definition" => true,
        "decorated_definition" => first_named_child(node)
            .map(|child| matches!(child.kind(), "function_definition"))
            .unwrap_or(false),
        _ => false,
    }
}

fn is_class_node(node: Node<'_>) -> bool {
    match node.kind() {
        "class_definition" | "struct_item" | "enum_item" | "trait_item" | "impl_item"
        | "union_item" | "type_item" => true,
        "decorated_definition" => first_named_child(node)
            .map(|child| matches!(child.kind(), "class_definition"))
            .unwrap_or(false),
        _ => false,
    }
}

fn is_parameter_node(node: Node<'_>) -> bool {
    let kind = node.kind();
    (kind.contains("parameter") && kind != "parameters")
        || matches!(kind, "typed_parameter" | "self_parameter" | "default_parameter")
}

fn is_test_node(node: Node<'_>, source: &str) -> bool {
    match node.kind() {
        "function_item" => rust_function_has_test_attribute(node, source),
        "function_definition" => python_function_name(node, source)
            .map(|name| name.starts_with("test_"))
            .unwrap_or(false),
        "decorated_definition" => {
            if let Some(child) = first_named_child(node) {
                matches!(child.kind(), "function_definition")
                    && python_function_name(child, source)
                        .map(|name| name.starts_with("test_"))
                        .unwrap_or(false)
            } else {
                false
            }
        }
        _ => false,
    }
}

fn rust_function_has_test_attribute(node: Node<'_>, source: &str) -> bool {
    let mut sibling = node.prev_named_sibling();
    while let Some(candidate) = sibling {
        if candidate.kind() != "attribute_item" {
            break;
        }
        if node_text(candidate, source).contains("test") {
            return true;
        }
        sibling = candidate.prev_named_sibling();
    }
    false
}

fn python_function_name<'a>(node: Node<'a>, source: &'a str) -> Option<&'a str> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == "identifier")
        .map(|child| node_text(child, source))
}

fn node_text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    source.get(node.start_byte()..node.end_byte()).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::{
        SyntaxNavigationAction, SyntaxNavigationTarget, SyntaxSelectionAction,
        SyntaxSelectionError, apply_syntax_navigation, apply_syntax_selection, node_to_region,
    };
    use crate::selection::{SelRegion, Selection};
    use crate::tree_sitter_support::ts_language_for_name;
    use tree_sitter::Parser;
    use xi_rope::Rope;

    fn select_range(start: usize, end: usize) -> Selection {
        Selection::new_simple(SelRegion::new(start, end))
    }

    fn caret(offset: usize) -> Selection {
        Selection::new_simple(SelRegion::caret(offset))
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

    #[test]
    fn goto_next_and_prev_function_follow_function_items() {
        let source = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        let text = Rope::from(source);

        let next = apply_syntax_navigation(
            &text,
            &caret(source.find("alpha").unwrap()),
            "Rust",
            None,
            SyntaxNavigationAction::new(SyntaxNavigationTarget::Function, true),
        )
        .unwrap();
        assert!(super::selection_eq(&next, &caret(source.find("fn beta").unwrap())));

        let prev = apply_syntax_navigation(
            &text,
            &caret(source.find("gamma").unwrap()),
            "Rust",
            None,
            SyntaxNavigationAction::new(SyntaxNavigationTarget::Function, false),
        )
        .unwrap();
        assert!(super::selection_eq(&prev, &caret(source.find("fn beta").unwrap())));
    }

    #[test]
    fn goto_next_and_prev_class_follow_type_items() {
        let source = "struct Alpha;\nfn helper() {}\nenum Beta { One }\ntrait Gamma {}\n";
        let text = Rope::from(source);

        let next = apply_syntax_navigation(
            &text,
            &caret(source.find("Alpha").unwrap()),
            "Rust",
            None,
            SyntaxNavigationAction::new(SyntaxNavigationTarget::Class, true),
        )
        .unwrap();
        assert!(super::selection_eq(&next, &caret(source.find("enum Beta").unwrap())));

        let prev = apply_syntax_navigation(
            &text,
            &caret(source.find("trait Gamma").unwrap()),
            "Rust",
            None,
            SyntaxNavigationAction::new(SyntaxNavigationTarget::Class, false),
        )
        .unwrap();
        assert!(super::selection_eq(&prev, &caret(source.find("enum Beta").unwrap())));
    }

    #[test]
    fn goto_next_and_prev_parameter_follow_parameter_nodes() {
        let source = "fn alpha(first: usize, second: usize, third: usize) {}\n";
        let text = Rope::from(source);

        let next = apply_syntax_navigation(
            &text,
            &caret(source.find("first").unwrap()),
            "Rust",
            None,
            SyntaxNavigationAction::new(SyntaxNavigationTarget::Parameter, true),
        )
        .unwrap();
        assert!(super::selection_eq(&next, &caret(source.find("second").unwrap())));

        let prev = apply_syntax_navigation(
            &text,
            &caret(source.find("third").unwrap()),
            "Rust",
            None,
            SyntaxNavigationAction::new(SyntaxNavigationTarget::Parameter, false),
        )
        .unwrap();
        assert!(super::selection_eq(&prev, &caret(source.find("second").unwrap())));
    }

    #[test]
    fn goto_next_and_prev_comment_follow_comment_nodes() {
        let source = "// alpha\nfn demo() {}\n// beta\n// gamma\n";
        let text = Rope::from(source);

        let next = apply_syntax_navigation(
            &text,
            &caret(0),
            "Rust",
            None,
            SyntaxNavigationAction::new(SyntaxNavigationTarget::Comment, true),
        )
        .unwrap();
        assert!(super::selection_eq(&next, &caret(source.find("// beta").unwrap())));

        let prev = apply_syntax_navigation(
            &text,
            &caret(source.find("// gamma").unwrap()),
            "Rust",
            None,
            SyntaxNavigationAction::new(SyntaxNavigationTarget::Comment, false),
        )
        .unwrap();
        assert!(super::selection_eq(&prev, &caret(source.find("// beta").unwrap())));
    }

    #[test]
    fn goto_next_and_prev_test_follow_test_functions() {
        let source = "#[test]\nfn alpha() {}\nfn helper() {}\n#[tokio::test]\nasync fn beta() {}\n";
        let text = Rope::from(source);

        let next = apply_syntax_navigation(
            &text,
            &caret(source.find("alpha").unwrap()),
            "Rust",
            None,
            SyntaxNavigationAction::new(SyntaxNavigationTarget::Test, true),
        )
        .unwrap();
        assert!(super::selection_eq(&next, &caret(source.find("async fn beta").unwrap())));

        let prev = apply_syntax_navigation(
            &text,
            &caret(source.find("beta").unwrap()),
            "Rust",
            None,
            SyntaxNavigationAction::new(SyntaxNavigationTarget::Test, false),
        )
        .unwrap();
        assert!(super::selection_eq(&prev, &caret(source.find("fn alpha").unwrap())));
    }
}
