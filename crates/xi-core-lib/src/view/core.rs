//! `impl View` methods: core.
use super::*;

impl View {
    pub fn new(view_id: ViewId, buffer_id: BufferId) -> View {
        View {
            view_id,
            buffer_id,
            pending_render: false,
            selection: SelRegion::caret(0).into(),
            primary_selection_idx: 0,
            object_selection_history: Vec::new(),
            semantic_parse_cache: object::SyntaxParseCache::default(),
            scroll_to: Some(0),
            size: Size::default(),
            drag_state: None,
            first_line: 0,
            height: 10,
            lines: Lines::default(),
            lc_shadow: LineCacheShadow::default(),
            find: Vec::new(),
            find_id_counter: Counter::default(),
            find_changed: FindStatusChange::None,
            find_progress: FindProgress::Ready,
            highlight_find: false,
            vlf_find: None,
            replace: None,
            replace_changed: false,
            annotations: AnnotationStore::new(),
            diagnostics: HashMap::new(),
            pending_hover_requests: HashMap::new(),
        }
    }

    pub(crate) fn get_buffer_id(&self) -> BufferId {
        self.buffer_id
    }

    pub(crate) fn get_view_id(&self) -> ViewId {
        self.view_id
    }

    pub(crate) fn get_lines(&self) -> &Lines {
        &self.lines
    }

    pub(crate) fn get_replace(&self) -> Option<Replace> {
        self.replace.clone()
    }

    pub(crate) fn set_has_pending_render(&mut self, pending: bool) {
        self.pending_render = pending
    }

    pub(crate) fn has_pending_render(&self) -> bool {
        self.pending_render
    }

    pub(crate) fn replace_pending_hover_request(
        &mut self,
        plugin_id: PluginId,
        request_id: RequestId,
    ) -> Option<RequestId> {
        self.pending_hover_requests.insert(plugin_id, request_id)
    }

    pub(crate) fn take_pending_hover_request(&mut self, plugin_id: PluginId) -> Option<RequestId> {
        self.pending_hover_requests.remove(&plugin_id)
    }

    pub(crate) fn update_wrap_settings(&mut self, text: &Rope, wrap_cols: usize, word_wrap: bool) {
        let wrap_width = match (word_wrap, wrap_cols) {
            (true, _) => WrapWidth::Width(self.size.width),
            (false, 0) => WrapWidth::None,
            (false, cols) => WrapWidth::Bytes(cols),
        };
        self.lines.set_wrap_width(text, wrap_width);
    }

    pub(crate) fn needs_more_wrap(&self) -> bool {
        !self.lines.is_converged()
    }

    pub(crate) fn start_vlf_find(
        &mut self,
        store: &crate::vlf::store::VlfStore,
        chars: String,
        case_sensitive: bool,
        regex: bool,
        whole_words: bool,
    ) {
        self.vlf_find = VlfSearchState::new(store, chars, case_sensitive, regex, whole_words);
    }

    pub(crate) fn clear_vlf_find(&mut self) {
        self.vlf_find = None;
    }

    pub(crate) fn vlf_find_in_progress(&self) -> bool {
        self.vlf_find.as_ref().is_some_and(|search| !search.is_complete())
    }

    pub(crate) fn scan_vlf_find(
        &mut self,
        store: &crate::vlf::store::VlfStore,
    ) -> std::io::Result<Option<VlfSearchStatus>> {
        let Some(search) = self.vlf_find.as_mut() else {
            return Ok(None);
        };

        search.scan_batch(store)?;
        Ok(Some(search.status()))
    }

    pub(crate) fn advance_vlf_match(&mut self, reverse: bool, wrap: bool) -> Option<VlfMatchRange> {
        self.vlf_find.as_mut()?.next_match(reverse, wrap)
    }

    pub(crate) fn needs_wrap_in_visible_region(&self, text: &Rope) -> bool {
        if self.lines.is_converged() {
            false
        } else {
            let visible_region = self.interval_of_visible_region(text);
            self.lines.interval_needs_wrap(visible_region)
        }
    }

    pub(crate) fn find_in_progress(&self) -> bool {
        matches!(self.find_progress, FindProgress::InProgress(_) | FindProgress::Started)
    }
}
