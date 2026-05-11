use std::path::{Path, PathBuf};

use tree_sitter::{Node, Parser, Tree};
use xi_rope::Rope;

use crate::selection::{SelRegion, Selection};
use crate::tree_sitter_support::{
    SemanticTargetKind, language_name_for_path, language_supports_semantic_target,
    resolve_ts_language,
};

#[derive(Debug, Clone, Copy)]
struct SyntaxParseWindow<'a> {
    source: &'a str,
    base_offset: usize,
    bounded: bool,
}

#[derive(Debug, Default)]
pub(crate) struct SyntaxParseCache {
    source: String,
    base_offset: usize,
    language_name: String,
    file_path: Option<PathBuf>,
    tree: Option<Tree>,
    parse_count: usize,
}

impl SyntaxParseCache {
    pub(crate) fn contains_window(
        &self,
        language_name: &str,
        file_path: Option<&Path>,
        base_offset: usize,
        end_offset: usize,
    ) -> bool {
        self.tree.is_some()
            && self.language_name == language_name
            && self.file_path.as_deref() == file_path
            && self.base_offset == base_offset
            && self.base_offset.saturating_add(self.source.len()) == end_offset
    }

    pub(crate) fn source(&self) -> &str {
        &self.source
    }

    pub(crate) fn update(
        &mut self,
        source: &str,
        base_offset: usize,
        language_name: &str,
        file_path: Option<&Path>,
    ) -> Result<(), SyntaxSelectionError> {
        let file_path_key = file_path.map(Path::to_path_buf);
        if self.tree.is_some()
            && self.source == source
            && self.base_offset == base_offset
            && self.language_name == language_name
            && self.file_path == file_path_key
        {
            return Ok(());
        }

        let Some(language) = resolve_ts_language(Some(language_name), file_path) else {
            return Err(SyntaxSelectionError::SyntaxTreeUnavailable);
        };
        let mut parser = Parser::new();
        parser.set_language(&language).map_err(|_| SyntaxSelectionError::SyntaxTreeUnavailable)?;
        let tree = parser.parse(source, None).ok_or(SyntaxSelectionError::SyntaxTreeUnavailable)?;

        self.source.clear();
        self.source.push_str(source);
        self.base_offset = base_offset;
        self.language_name.clear();
        self.language_name.push_str(language_name);
        self.file_path = file_path_key;
        self.tree = Some(tree);
        self.parse_count += 1;
        Ok(())
    }

    fn window(&self) -> Result<SyntaxParseWindow<'_>, SyntaxSelectionError> {
        self.tree.as_ref().ok_or(SyntaxSelectionError::SyntaxTreeUnavailable)?;
        Ok(SyntaxParseWindow::bounded(&self.source, self.base_offset))
    }

    fn root_node(&self) -> Result<Node<'_>, SyntaxSelectionError> {
        self.tree.as_ref().map(Tree::root_node).ok_or(SyntaxSelectionError::SyntaxTreeUnavailable)
    }

    #[cfg(test)]
    pub(crate) fn parse_count(&self) -> usize {
        self.parse_count
    }
}

impl<'a> SyntaxParseWindow<'a> {
    fn full(source: &'a str) -> Self {
        Self { source, base_offset: 0, bounded: false }
    }

    fn bounded(source: &'a str, base_offset: usize) -> Self {
        Self { source, base_offset, bounded: true }
    }

    fn len(self) -> usize {
        self.source.len()
    }

    fn contains_offset(self, offset: usize) -> bool {
        offset >= self.base_offset && offset <= self.base_offset.saturating_add(self.len())
    }

    fn contains_region(self, region: SelRegion) -> bool {
        self.contains_offset(region.min()) && self.contains_offset(region.max())
    }

    fn local_offset(self, offset: usize) -> Option<usize> {
        self.contains_offset(offset).then_some(offset.saturating_sub(self.base_offset))
    }

    fn absolute_offset(self, offset: usize) -> usize {
        self.base_offset.saturating_add(offset)
    }
}

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
    OutsideParsedRange,
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
            SyntaxSelectionError::OutsideParsedRange => "outside current parsed range",
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
    let source = text.to_string();
    apply_syntax_navigation_with_window(
        SyntaxParseWindow::full(&source),
        current,
        language_name,
        file_path,
        action,
    )
}

#[cfg(test)]
pub(crate) fn apply_syntax_navigation_in_window(
    source: &str,
    base_offset: usize,
    current: &Selection,
    language_name: &str,
    file_path: Option<&Path>,
    action: SyntaxNavigationAction,
) -> Result<Selection, SyntaxSelectionError> {
    apply_syntax_navigation_with_window(
        SyntaxParseWindow::bounded(source, base_offset),
        current,
        language_name,
        file_path,
        action,
    )
}

pub(crate) fn apply_syntax_navigation_in_cache(
    cache: &SyntaxParseCache,
    current: &Selection,
    action: SyntaxNavigationAction,
) -> Result<Selection, SyntaxSelectionError> {
    if !supports_semantic_target(&cache.language_name, cache.file_path.as_deref(), action.target) {
        return Err(SyntaxSelectionError::NavigationTargetAbsent);
    }
    let window = cache.window()?;
    let root = cache.root_node()?;
    apply_syntax_navigation_from_tree(window, root, current, action)
}

fn apply_syntax_navigation_with_window(
    window: SyntaxParseWindow<'_>,
    current: &Selection,
    language_name: &str,
    file_path: Option<&Path>,
    action: SyntaxNavigationAction,
) -> Result<Selection, SyntaxSelectionError> {
    let Some(language) = resolve_ts_language(Some(language_name), file_path) else {
        return Err(SyntaxSelectionError::SyntaxTreeUnavailable);
    };
    if !supports_semantic_target(language_name, file_path, action.target) {
        return Err(SyntaxSelectionError::NavigationTargetAbsent);
    }

    let mut parser = Parser::new();
    parser.set_language(&language).map_err(|_| SyntaxSelectionError::SyntaxTreeUnavailable)?;

    let Some(tree) = parser.parse(window.source, None) else {
        return Err(SyntaxSelectionError::SyntaxTreeUnavailable);
    };
    apply_syntax_navigation_from_tree(window, tree.root_node(), current, action)
}

fn apply_syntax_navigation_from_tree(
    window: SyntaxParseWindow<'_>,
    root: Node<'_>,
    current: &Selection,
    action: SyntaxNavigationAction,
) -> Result<Selection, SyntaxSelectionError> {
    let mut selection = Selection::new();

    for &region in current.iter() {
        let cursor =
            window.local_offset(region.end).ok_or(SyntaxSelectionError::OutsideParsedRange)?;
        let target = find_navigation_target(root, window.source, action, cursor, window.bounded)
            .ok_or(if window.bounded {
                SyntaxSelectionError::OutsideParsedRange
            } else {
                SyntaxSelectionError::NavigationTargetAbsent
            })?;
        selection.add_region(SelRegion::caret(window.absolute_offset(target.start_byte())));
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
    let source = text.to_string();
    apply_syntax_selection_with_window(
        SyntaxParseWindow::full(&source),
        current,
        history,
        language_name,
        file_path,
        action,
    )
}

#[cfg(test)]
pub(crate) fn apply_syntax_selection_in_window(
    source: &str,
    base_offset: usize,
    current: &Selection,
    history: &mut Vec<Selection>,
    language_name: &str,
    file_path: Option<&Path>,
    action: SyntaxSelectionAction,
) -> Result<Selection, SyntaxSelectionError> {
    apply_syntax_selection_with_window(
        SyntaxParseWindow::bounded(source, base_offset),
        current,
        history,
        language_name,
        file_path,
        action,
    )
}

pub(crate) fn apply_syntax_selection_in_cache(
    cache: &SyntaxParseCache,
    current: &Selection,
    history: &mut Vec<Selection>,
    action: SyntaxSelectionAction,
) -> Result<Selection, SyntaxSelectionError> {
    apply_syntax_selection_from_tree(cache.window()?, cache.root_node()?, current, history, action)
}

fn apply_syntax_selection_with_window(
    window: SyntaxParseWindow<'_>,
    current: &Selection,
    history: &mut Vec<Selection>,
    language_name: &str,
    file_path: Option<&Path>,
    action: SyntaxSelectionAction,
) -> Result<Selection, SyntaxSelectionError> {
    let Some(language) = resolve_ts_language(Some(language_name), file_path) else {
        return Err(SyntaxSelectionError::SyntaxTreeUnavailable);
    };
    let mut parser = Parser::new();
    parser.set_language(&language).map_err(|_| SyntaxSelectionError::SyntaxTreeUnavailable)?;

    let Some(tree) = parser.parse(window.source, None) else {
        return Err(SyntaxSelectionError::SyntaxTreeUnavailable);
    };
    apply_syntax_selection_from_tree(window, tree.root_node(), current, history, action)
}

fn apply_syntax_selection_from_tree(
    window: SyntaxParseWindow<'_>,
    root: Node<'_>,
    current: &Selection,
    history: &mut Vec<Selection>,
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

    let next = match action {
        SyntaxSelectionAction::Expand => expand_selection(current, root, window)?,
        SyntaxSelectionAction::Shrink => shrink_selection(current, root, window)?,
        SyntaxSelectionAction::SelectPrevSibling => select_sibling(current, root, window, true)?,
        SyntaxSelectionAction::SelectNextSibling => select_sibling(current, root, window, false)?,
        SyntaxSelectionAction::SelectAllSiblings => select_all_siblings(current, root, window)?,
        SyntaxSelectionAction::SelectAllChildren => select_all_children(current, root, window)?,
        SyntaxSelectionAction::MoveParentNodeStart => {
            move_parent_node_boundary(current, root, window, false)?
        }
        SyntaxSelectionAction::MoveParentNodeEnd => {
            move_parent_node_boundary(current, root, window, true)?
        }
    };

    if matches!(action, SyntaxSelectionAction::Expand) && !selection_eq(&next, current) {
        history.push(current.clone());
    }

    Ok(next)
}

fn expand_selection(
    current: &Selection,
    root: Node<'_>,
    window: SyntaxParseWindow<'_>,
) -> Result<Selection, SyntaxSelectionError> {
    let mut selection = Selection::new();
    for &region in current.iter() {
        let (from, to, node) = node_for_region(root, region, window).ok_or(if window.bounded {
            SyntaxSelectionError::OutsideParsedRange
        } else {
            SyntaxSelectionError::ParentAbsent
        })?;
        let mut parent = node;
        while parent.start_byte() == from && parent.end_byte() == to {
            parent = parent.parent().ok_or(SyntaxSelectionError::ParentAbsent)?;
        }
        if window.bounded && parent.parent().is_none() {
            return Err(SyntaxSelectionError::OutsideParsedRange);
        }
        selection.add_region(node_to_region(parent, window.base_offset));
    }
    Ok(selection)
}

fn shrink_selection(
    current: &Selection,
    root: Node<'_>,
    window: SyntaxParseWindow<'_>,
) -> Result<Selection, SyntaxSelectionError> {
    let mut selection = Selection::new();
    for &region in current.iter() {
        let (_, _, node) = node_for_region(root, region, window).ok_or(if window.bounded {
            SyntaxSelectionError::OutsideParsedRange
        } else {
            SyntaxSelectionError::ChildAbsent
        })?;
        let child = first_named_child(node).ok_or(SyntaxSelectionError::ChildAbsent)?;
        selection.add_region(node_to_region(child, window.base_offset));
    }
    Ok(selection)
}

fn select_sibling(
    current: &Selection,
    root: Node<'_>,
    window: SyntaxParseWindow<'_>,
    previous: bool,
) -> Result<Selection, SyntaxSelectionError> {
    let mut selection = Selection::new();
    for &region in current.iter() {
        let (_, _, node) = node_for_region(root, region, window).ok_or(if window.bounded {
            SyntaxSelectionError::OutsideParsedRange
        } else {
            SyntaxSelectionError::SiblingAbsent
        })?;
        let sibling = if previous {
            node.prev_named_sibling().or_else(|| node.prev_sibling())
        } else {
            node.next_named_sibling().or_else(|| node.next_sibling())
        }
        .filter(|sibling| sibling.end_byte() > sibling.start_byte())
        .ok_or(if window.bounded {
            SyntaxSelectionError::OutsideParsedRange
        } else {
            SyntaxSelectionError::SiblingAbsent
        })?;
        selection.add_region(node_to_region(sibling, window.base_offset));
    }
    Ok(selection)
}

fn select_all_siblings(
    current: &Selection,
    root: Node<'_>,
    window: SyntaxParseWindow<'_>,
) -> Result<Selection, SyntaxSelectionError> {
    let mut selection = Selection::new();
    let mut found_any = false;

    for &region in current.iter() {
        let (_, _, node) = node_for_region(root, region, window).ok_or(if window.bounded {
            SyntaxSelectionError::OutsideParsedRange
        } else {
            SyntaxSelectionError::SiblingAbsent
        })?;
        let parent = node.parent().ok_or(SyntaxSelectionError::SiblingAbsent)?;
        if window.bounded && parent.parent().is_none() {
            return Err(SyntaxSelectionError::OutsideParsedRange);
        }
        let mut cursor = parent.walk();
        let mut has_sibling = false;
        for child in parent.named_children(&mut cursor) {
            if child.end_byte() <= child.start_byte() {
                continue;
            }
            has_sibling = true;
            selection.add_range_distinct(node_to_region(child, window.base_offset));
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
    window: SyntaxParseWindow<'_>,
) -> Result<Selection, SyntaxSelectionError> {
    let mut selection = Selection::new();
    let mut found_any = false;

    for &region in current.iter() {
        let (_, _, node) = node_for_region(root, region, window).ok_or(if window.bounded {
            SyntaxSelectionError::OutsideParsedRange
        } else {
            SyntaxSelectionError::ChildrenAbsent
        })?;
        let mut cursor = node.walk();
        let mut has_child = false;
        for child in node.named_children(&mut cursor) {
            if child.end_byte() <= child.start_byte() {
                continue;
            }
            has_child = true;
            selection.add_range_distinct(node_to_region(child, window.base_offset));
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
    window: SyntaxParseWindow<'_>,
    end: bool,
) -> Result<Selection, SyntaxSelectionError> {
    let mut selection = Selection::new();

    for &region in current.iter() {
        let (_, _, node) = node_for_region(root, region, window).ok_or(if window.bounded {
            SyntaxSelectionError::OutsideParsedRange
        } else {
            SyntaxSelectionError::ParentAbsent
        })?;
        let parent = node.parent().ok_or(SyntaxSelectionError::ParentAbsent)?;
        if window.bounded && parent.parent().is_none() {
            return Err(SyntaxSelectionError::OutsideParsedRange);
        }
        let offset = if end { parent.end_byte() } else { parent.start_byte() };
        selection.add_region(SelRegion::caret(window.absolute_offset(offset)));
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
    window: SyntaxParseWindow<'_>,
) -> Option<(usize, usize, Node<'a>)> {
    let text_len = window.len();
    if text_len == 0 {
        return None;
    }

    if !window.contains_region(region) {
        return None;
    }

    let from = window.local_offset(region.min())?.min(text_len);
    let to = window.local_offset(region.max())?.min(text_len);

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

fn node_to_region(node: Node<'_>, base_offset: usize) -> SelRegion {
    SelRegion::new(
        base_offset.saturating_add(node.start_byte()),
        base_offset.saturating_add(node.end_byte()),
    )
}

fn find_navigation_target<'a>(
    root: Node<'a>,
    source: &str,
    action: SyntaxNavigationAction,
    cursor: usize,
    bounded: bool,
) -> Option<Node<'a>> {
    let mut matches = Vec::new();
    collect_navigation_nodes(root, source, action.target, &mut matches);
    matches.sort_by_key(|node| (node.start_byte(), node.end_byte()));

    if action.forward {
        matches
            .iter()
            .copied()
            .find(|node| node.start_byte() > cursor)
            .or_else(|| (!bounded).then(|| matches.first().copied()).flatten())
    } else {
        matches
            .iter()
            .rev()
            .copied()
            .find(|node| node.end_byte() <= cursor)
            .or_else(|| (!bounded).then(|| matches.last().copied()).flatten())
    }
}

fn semantic_target_kind(target: SyntaxNavigationTarget) -> SemanticTargetKind {
    match target {
        SyntaxNavigationTarget::Function => SemanticTargetKind::Function,
        SyntaxNavigationTarget::Class => SemanticTargetKind::Class,
        SyntaxNavigationTarget::Parameter => SemanticTargetKind::Parameter,
        SyntaxNavigationTarget::Comment => SemanticTargetKind::Comment,
        SyntaxNavigationTarget::Test => SemanticTargetKind::Test,
    }
}

fn supports_semantic_target(
    language_name: &str,
    file_path: Option<&Path>,
    target: SyntaxNavigationTarget,
) -> bool {
    let target = semantic_target_kind(target);
    language_supports_semantic_target(language_name, target)
        || file_path
            .and_then(language_name_for_path)
            .is_some_and(|resolved| language_supports_semantic_target(resolved, target))
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
        "function_item"
        | "function_definition"
        | "function_declaration"
        | "generator_function_declaration"
        | "method_definition" => true,
        "decorated_definition" => first_named_child(node)
            .map(|child| matches!(child.kind(), "function_definition"))
            .unwrap_or(false),
        _ => false,
    }
}

fn is_class_node(node: Node<'_>) -> bool {
    match node.kind() {
        "class_definition" | "class_declaration" | "struct_item" | "enum_item" | "trait_item"
        | "impl_item" | "union_item" | "type_item" => true,
        "decorated_definition" => first_named_child(node)
            .map(|child| matches!(child.kind(), "class_definition"))
            .unwrap_or(false),
        _ => false,
    }
}

fn is_parameter_node(node: Node<'_>) -> bool {
    let kind = node.kind();
    (kind.contains("parameter") && !kind.ends_with("parameters"))
        || matches!(kind, "typed_parameter" | "self_parameter" | "default_parameter")
        || (kind == "identifier"
            && node
                .parent()
                .is_some_and(|parent| matches!(parent.kind(), "parameters" | "formal_parameters")))
}

fn is_test_node(node: Node<'_>, source: &str) -> bool {
    match node.kind() {
        "function_item" => rust_function_has_test_attribute(node, source),
        "function_definition" => python_function_name(node, source)
            .map(|name| name.starts_with("test_"))
            .unwrap_or(false),
        "call_expression" => javascript_test_call_name(node, source)
            .map(|name| matches!(name, "test" | "it"))
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

fn javascript_test_call_name<'a>(node: Node<'a>, source: &'a str) -> Option<&'a str> {
    let function = node.child_by_field_name("function").or_else(|| first_named_child(node))?;
    match function.kind() {
        "identifier" => Some(node_text(function, source)),
        "member_expression" => {
            function.child_by_field_name("property").map(|property| node_text(property, source))
        }
        _ => None,
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
        SyntaxSelectionError, apply_syntax_navigation, apply_syntax_navigation_in_window,
        apply_syntax_selection, apply_syntax_selection_in_window, node_to_region,
    };
    use crate::selection::{SelRegion, Selection};
    use crate::tree_sitter_support::ts_language_for_name;
    use std::path::Path;
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
        node_to_region(node, 0)
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
    fn registry_path_resolution_extends_beyond_rust_and_python() {
        let source = "function alpha() { return value; }\n";
        let text = Rope::from(source);
        let mut history = Vec::new();
        let current =
            select_range(source.find("value").unwrap(), source.find("value").unwrap() + 5);

        let expanded = apply_syntax_selection(
            &text,
            &current,
            &mut history,
            "Plain Text",
            Some(Path::new("app.js")),
            SyntaxSelectionAction::Expand,
        )
        .unwrap();

        assert!(!super::selection_eq(&expanded, &current));
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

    #[test]
    fn goto_next_function_uses_registry_for_javascript() {
        let source = "function alpha() {}\nfunction beta() {}\n";
        let text = Rope::from(source);

        let next = apply_syntax_navigation(
            &text,
            &caret(source.find("alpha").unwrap()),
            "Plain Text",
            Some(Path::new("app.js")),
            SyntaxNavigationAction::new(SyntaxNavigationTarget::Function, true),
        )
        .unwrap();

        assert!(super::selection_eq(&next, &caret(source.find("function beta").unwrap())));
    }

    #[test]
    fn javascript_semantic_navigation_covers_supported_targets() {
        let source = "// alpha\nclass Alpha {}\nfunction beta(first, second) {}\ntest('beta', () => {});\n// omega\n";
        let text = Rope::from(source);
        let path = Some(Path::new("app.js"));

        let next_function = apply_syntax_navigation(
            &text,
            &caret(0),
            "Plain Text",
            path,
            SyntaxNavigationAction::new(SyntaxNavigationTarget::Function, true),
        )
        .unwrap();
        assert!(super::selection_eq(&next_function, &caret(source.find("function beta").unwrap())));

        let prev_class = apply_syntax_navigation(
            &text,
            &caret(source.find("function beta").unwrap()),
            "Plain Text",
            path,
            SyntaxNavigationAction::new(SyntaxNavigationTarget::Class, false),
        )
        .unwrap();
        assert!(super::selection_eq(&prev_class, &caret(source.find("class Alpha").unwrap())));

        let next_parameter = apply_syntax_navigation(
            &text,
            &caret(source.find("first").unwrap()),
            "Plain Text",
            path,
            SyntaxNavigationAction::new(SyntaxNavigationTarget::Parameter, true),
        )
        .unwrap();
        assert!(super::selection_eq(&next_parameter, &caret(source.find("second").unwrap())));

        let prev_comment = apply_syntax_navigation(
            &text,
            &caret(source.find("omega").unwrap()),
            "Plain Text",
            path,
            SyntaxNavigationAction::new(SyntaxNavigationTarget::Comment, false),
        )
        .unwrap();
        assert!(super::selection_eq(&prev_comment, &caret(source.find("// alpha").unwrap())));

        let next_test = apply_syntax_navigation(
            &text,
            &caret(source.find("function beta").unwrap()),
            "Plain Text",
            path,
            SyntaxNavigationAction::new(SyntaxNavigationTarget::Test, true),
        )
        .unwrap();
        assert!(super::selection_eq(&next_test, &caret(source.find("test(").unwrap())));
    }

    #[test]
    fn bounded_navigation_reports_outside_parsed_range_without_wrapping() {
        let source = "fn alpha() {}\nfn beta() {}\n";
        let current = caret(0);

        let err = apply_syntax_navigation_in_window(
            &source[..source.find("fn beta").unwrap()],
            0,
            &current,
            "Rust",
            None,
            SyntaxNavigationAction::new(SyntaxNavigationTarget::Function, true),
        )
        .unwrap_err();

        assert_eq!(err, SyntaxSelectionError::OutsideParsedRange);
    }

    #[test]
    fn bounded_selection_uses_absolute_offsets() {
        let source = "fn main() { [foo, bar, baz]; }\n";
        let base = source.find('[').unwrap();
        let end = source.find(']').unwrap() + 1;
        let current = select_range(base, end);
        let mut history = Vec::new();

        let children = apply_syntax_selection_in_window(
            &source[base..end],
            base,
            &current,
            &mut history,
            "Rust",
            None,
            SyntaxSelectionAction::SelectAllChildren,
        )
        .unwrap();

        assert!(children.iter().any(|region| region.min() == source.find("foo").unwrap()));
        assert!(children.iter().any(|region| region.min() == source.find("bar").unwrap()));
        assert!(children.iter().any(|region| region.min() == source.find("baz").unwrap()));
    }

    #[test]
    fn bounded_expand_selection_grows_region_for_visible_node() {
        let source = "fn main() { foo(bar); }\n";
        let start = source.find("bar").unwrap();
        let end = start + 3;
        let current = select_range(start, end);
        let mut history = Vec::new();

        let expanded = apply_syntax_selection_in_window(
            source,
            0,
            &current,
            &mut history,
            "Rust",
            None,
            SyntaxSelectionAction::Expand,
        )
        .unwrap();

        assert_eq!(expanded.len(), 1);
        assert!(expanded[0].min() <= start);
        assert!(expanded[0].max() >= end);
        assert!(expanded[0].min() < start || expanded[0].max() > end);
    }
}
