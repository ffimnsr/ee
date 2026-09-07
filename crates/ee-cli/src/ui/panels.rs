//! UI: panels rendering.
use super::*;

pub(super) fn render_tab_bar(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let focused_idx = app.tabs.focused_idx();
    let spans: Vec<Span> = app
        .tabs
        .iter()
        .flat_map(|(i, tab)| {
            // Derive a display name from the focused window's buffer title.
            let buf_id = tab.windows.focused_window().buffer_id;
            let label = app
                .backend
                .all_bufs()
                .iter()
                .find(|b| b.id == buf_id)
                .map(|b| b.title())
                .unwrap_or_else(|| format!("[{}]", i + 1));
            let style = if i == focused_idx {
                Style::default().fg(theme::BG_APP).bg(theme::FG_KEY).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::FG_TAB_INACTIVE).bg(theme::BG_CHROME)
            };
            [Span::styled(format!(" {} ", label), style), Span::raw(" ")]
        })
        .collect();

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::BG_APP)),
        area,
    );
}

pub(super) fn render_gutter(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    buf: &BufState,
    vp: Viewport,
    app: &App,
) {
    let height = area.height as usize;
    let line_count = buf.line_count().max(1);
    let cursor_line = buf.cursor_line;
    let top = vp.top_line;
    let sign_col = app.config.sign_column;
    let num_digits = line_count.to_string().len().max(3);
    let cursor_line_bg = theme::BG_CURSOR_LINE;
    let vis_sel_bg = theme::BG_SELECTION;

    // Pre-compute visual selection line range for gutter highlight.
    let visual_line_range: Option<(usize, usize)> = if app.mode.is_visual() {
        let (al, _) = app.visual_anchor.unwrap_or((cursor_line, buf.cursor_col));
        let cl = cursor_line;
        let lo = al.min(cl);
        let hi = al.max(cl);
        Some((lo, hi))
    } else {
        None
    };

    let mut lines: Vec<Line> = Vec::with_capacity(height);
    let mut li = top;
    let tilde_style = Style::default().fg(theme::FG_TILDE).bg(theme::BG_CHROME);
    for _ in 0..height {
        if li >= line_count {
            lines.push(Line::from(Span::styled("~", tilde_style)));
            continue;
        }
        let is_cursor = li == cursor_line;
        let in_visual = visual_line_range.is_some_and(|(lo, hi)| li >= lo && li <= hi);
        let bg = if in_visual {
            vis_sel_bg
        } else if is_cursor && app.config.cursor_line {
            cursor_line_bg
        } else {
            theme::BG_CHROME
        };

        // Sign column: show fold markers when applicable.
        let sign_spans = if sign_col {
            let (marker, fg) = if let Some(severity) = diagnostic_marker_for_line(buf, li) {
                match severity {
                    DiagnosticSeverity::Error => ('E', theme::FG_ERROR),
                    DiagnosticSeverity::Warning => ('W', theme::FG_WARNING),
                    DiagnosticSeverity::Information => ('I', theme::FG_KEY),
                    DiagnosticSeverity::Hint => ('H', theme::FG_MARKER_HINT),
                }
            } else if app.folds.fold_at(buf.id, li).is_some() {
                ('▸', theme::FG_FOLD)
            } else {
                (' ', theme::FG_FOLD)
            };
            let (annotation_marker, annotation_fg) = app
                .git_status(buf.id)
                .and_then(|status| status.sign_for_line(li))
                .map(|sign| (sign.marker(), git_sign_color(sign)))
                .or_else(|| annotation_marker_for_line(buf, li))
                .unwrap_or((' ', theme::FG_GUTTER_DIM));
            vec![
                Span::styled(marker.to_string(), Style::default().fg(fg).bg(bg)),
                Span::styled(
                    annotation_marker.to_string(),
                    Style::default().fg(annotation_fg).bg(bg),
                ),
            ]
        } else {
            Vec::new()
        };

        let num_text = match app.config.number_style {
            NumberStyle::Absolute => format!("{:>width$} ", li + 1, width = num_digits),
            NumberStyle::Relative => {
                let dist = li.abs_diff(cursor_line);
                if dist == 0 {
                    format!("{:>width$} ", li + 1, width = num_digits)
                } else {
                    format!("{:>width$} ", dist, width = num_digits)
                }
            }
            NumberStyle::RelativeAbsolute => {
                let dist = li.abs_diff(cursor_line);
                if is_cursor {
                    format!("{:>width$} ", li + 1, width = num_digits)
                } else {
                    format!("{:>width$} ", dist, width = num_digits)
                }
            }
        };
        let num_fg = if is_cursor { theme::FG_TEXT } else { theme::FG_EMPTY };
        let num_span = Span::styled(num_text, Style::default().fg(num_fg).bg(bg));

        // Fold-aware: advance past fold body.
        if let Some((_, end)) = app.folds.fold_at(buf.id, li) {
            li = end + 1;
        } else {
            li += 1;
        }

        if sign_col {
            let mut row = sign_spans;
            row.push(num_span);
            lines.push(Line::from(row));
        } else {
            lines.push(Line::from(num_span));
        }
    }

    frame.render_widget(Paragraph::new(lines).style(Style::default().bg(theme::BG_CHROME)), area);
}

pub(super) fn render_buffer(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    buf: &BufState,
    vp: Viewport,
    app: &App,
) {
    let content_area = buffer_content_area(area);
    let height = area.height as usize;
    let top = vp.top_line;
    let left = vp.left_col;
    let viewport_width = content_area.width as usize;
    let cursor_line = buf.cursor_line;
    let cursor_line_bg = theme::BG_CURSOR_LINE;
    let buf_bg = theme::BG_APP;

    frame.render_widget(Block::default().style(Style::default().bg(buf_bg)), area);

    // Pre-compute visual selection bounds for this buffer.
    // Returns (anchor_line, anchor_col, cursor_line, cursor_col) in natural order.
    let visual_sel = if app.mode.is_visual() {
        let (al, ac) = app.visual_anchor.unwrap_or((cursor_line, buf.cursor_col));
        let (cl, cc) = (cursor_line, buf.cursor_col);
        Some((al, ac, cl, cc))
    } else {
        None
    };

    if buf.line_count() == 0 && top == 0 {
        let label = if buf.pending_line_request
            || buf
                .path
                .as_ref()
                .and_then(|path| std::fs::metadata(path).ok())
                .is_some_and(|metadata| metadata.len() > 0)
        {
            "Loading..."
        } else {
            "empty buffer"
        };
        let text = vec![Line::from(Span::styled(
            label,
            Style::default().fg(theme::FG_EMPTY).add_modifier(Modifier::ITALIC),
        ))];
        frame.render_widget(
            Paragraph::new(text)
                .block(Block::default().borders(Borders::NONE))
                .style(Style::default().fg(theme::FG_BUFFER).bg(buf_bg)),
            content_area,
        );
        return;
    }

    // Collect visible logical lines (fold-aware).  Use line_count() so VLF
    // buffers iterate over `line_cache` size rather than the empty `lines` vec.
    let mut visible: Vec<usize> = Vec::with_capacity(height);
    let mut li = top;
    while visible.len() < height && li < buf.line_count() {
        visible.push(li);
        if let Some((_, end)) = app.folds.fold_at(buf.id, li) {
            li = end + 1;
        } else {
            li += 1;
        }
    }
    // Style for lines not yet loaded in VLF mode.
    let loading_style = Style::default().fg(theme::FG_LOADING).add_modifier(Modifier::ITALIC);

    let text: Vec<Line> = visible
        .iter()
        .map(|&log_idx| {
            let is_cursor = log_idx == cursor_line;
            let bg = if is_cursor && app.config.cursor_line { cursor_line_bg } else { buf_bg };

            // In VLF mode, a `None` return means the page is not yet loaded.
            // Render a non-blocking "Loading…" row instead of an empty placeholder.
            let line_opt = buf.get_line(log_idx);
            if buf.is_vlf && line_opt.is_none() {
                let loading_text = "  Loading…";
                let mut l = Line::from(Span::styled(loading_text, loading_style));
                if is_cursor && app.config.cursor_line {
                    l = l.style(Style::default().bg(bg));
                }
                return l;
            }

            let line = line_opt.unwrap_or("");
            let is_fold_header = app.folds.fold_at(buf.id, log_idx).is_some();
            let byte_start = display_col_to_byte(line, left);
            let byte_end =
                display_col_to_byte(line, left.saturating_add(viewport_width).saturating_add(1));

            let mut spans: Vec<Span<'static>> = if is_fold_header {
                // Show fold marker line (abbreviated first line + fold indicator).
                let preview: String = line.chars().take(40).collect();
                vec![Span::styled(
                    format!("{preview}  ··· (folded)",),
                    Style::default().fg(theme::FG_FOLD).bg(bg).add_modifier(Modifier::ITALIC),
                )]
            } else {
                let backend_syntax = match buf.line_slot(log_idx) {
                    Some(LineSlot::Known(cached_line)) if !cached_line.syntax_spans.is_empty() => {
                        Some(cached_line.syntax_spans.as_slice())
                    }
                    _ => None,
                };

                if let Some(syntax_spans) = backend_syntax {
                    crate::highlight::Highlighter::scope_spans_in_range(
                        line,
                        syntax_spans,
                        byte_start,
                        byte_end,
                    )
                    .into_iter()
                    .map(|s| {
                        let style = if is_cursor && app.config.cursor_line {
                            s.style.bg(bg)
                        } else {
                            s.style
                        };
                        Span::styled(s.content.into_owned(), style)
                    })
                    .collect()
                } else {
                    vec![Span::styled(
                        line[byte_start..byte_end].to_owned(),
                        Style::default().bg(bg),
                    )]
                }
            };

            // Apply visible-whitespace substitution when enabled.
            if app.config.show_visible_whitespace && !is_fold_header {
                spans = apply_visible_whitespace(spans);
            }

            if !is_fold_header {
                spans = apply_core_annotations(spans, line, log_idx, &buf.annotations, left);
                if buf.is_vlf {
                    spans = apply_vlf_search_ranges(spans, log_idx, &buf.vlf_search_ranges, left);
                }
            }

            spans = expand_tabs_in_spans(spans, app.config.tab_width);

            // Apply search match highlighting over the rendered spans.
            if let Some(ref pat) = app.search_pattern
                && !is_fold_header
                && !buf.is_vlf
            {
                spans = apply_search_highlights(spans, line, pat, byte_start, byte_end, bg);
            }

            spans = sanitize_control_chars(spans);

            // Apply color column highlight.
            if let Some(cc) = app.config.color_column
                && cc >= left
            {
                let screen_col = cc - left;
                spans = apply_color_column(spans, screen_col);
            }

            // Apply visual-mode selection highlight (drawn last so it wins).
            if let Some((al, ac, cl, cc)) = visual_sel {
                let (sel_top_line, sel_top_col) =
                    if al < cl || (al == cl && ac <= cc) { (al, ac) } else { (cl, cc) };
                let (sel_bot_line, sel_bot_col) =
                    if al < cl || (al == cl && ac <= cc) { (cl, cc) } else { (al, ac) };

                let in_sel = log_idx >= sel_top_line && log_idx <= sel_bot_line;
                if in_sel {
                    let (col_start, col_end) = match app.mode {
                        crate::app::Mode::VisualLine => (None, None),
                        crate::app::Mode::VisualBlock => {
                            let c1 = sel_top_col.min(sel_bot_col);
                            let c2 = sel_top_col.max(sel_bot_col);
                            (Some(c1), Some(c2))
                        }
                        _ => {
                            // Characterwise: first line starts at anchor col,
                            // last line ends at cursor col, middle lines full.
                            let cs =
                                if log_idx == sel_top_line { Some(sel_top_col) } else { Some(0) };
                            let ce = if log_idx == sel_bot_line { Some(sel_bot_col) } else { None };
                            (cs, ce)
                        }
                    };
                    spans = apply_visual_highlight(spans, col_start, col_end, left);
                }
            }

            if let Some(swift_motion) = app.swift_motion.as_ref() {
                let line_targets = swift_motion
                    .targets
                    .iter()
                    .filter(|target| target.line == log_idx)
                    .cloned()
                    .collect::<Vec<_>>();
                if !line_targets.is_empty() {
                    spans = apply_swift_motion_targets(spans, &line_targets, left);
                }
            }

            spans = pad_spans_to_width(spans, viewport_width, Style::default().bg(bg));

            let mut l = Line::from(spans);
            if is_cursor && app.config.cursor_line {
                l = l.style(Style::default().bg(bg));
            }
            l
        })
        .collect();

    let mut widget = Paragraph::new(text)
        .block(Block::default().borders(Borders::NONE))
        .style(Style::default().fg(theme::FG_BUFFER).bg(buf_bg));

    if app.config.wrap_lines {
        widget = widget.wrap(Wrap { trim: false });
    }

    frame.render_widget(widget, content_area);
}

pub(super) fn render_status(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let mode_label = if app.quickfix_focused {
        "QFX"
    } else if app.location_list_focused {
        "LOC"
    } else {
        app.mode.label()
    };
    let mode = Span::styled(
        format!(" {} ", mode_label),
        Style::default().fg(theme::BG_APP).bg(theme::FG_KEY),
    );
    let file = Span::styled(
        format!(" {}", app.backend.title()),
        Style::default().fg(theme::FG_STATUS_FILE).bg(theme::BG_STATUS),
    );
    let modified = if app.backend.pristine {
        Span::raw("")
    } else {
        Span::styled(" [+]", Style::default().fg(theme::FG_WARNING).bg(theme::BG_STATUS))
    };
    let vlf_gap = if app.backend.active().is_vlf {
        Span::styled(" ", Style::default().bg(theme::BG_STATUS))
    } else {
        Span::raw("")
    };
    let vlf = if app.backend.active().is_vlf {
        Span::styled(" VLF ", Style::default().fg(theme::BG_APP).bg(theme::BG_FIND))
    } else {
        Span::raw("")
    };
    let position_text =
        format!("  Ln {}, Col {} ", app.backend.cursor_line + 1, app.backend.cursor_col + 1);
    let position = Span::styled(
        position_text.as_str(),
        Style::default().fg(theme::FG_TAB_INACTIVE).bg(theme::BG_STATUS),
    );

    let mut spans = match app.config.statusline_format {
        StatuslineFormat::Minimal => {
            let mut spans = vec![mode, file, modified, vlf_gap, vlf];
            if let Some(git_span) = git_status_span(app) {
                spans.push(git_span);
            }
            spans
        }
        StatuslineFormat::Default => {
            let buf_count = app.backend.buf_count();
            let buf_indicator = if buf_count > 1 {
                Span::styled(
                    format!("  [{}/{}]", app.backend.current_idx() + 1, buf_count),
                    Style::default().fg(theme::FG_MUTED).bg(theme::BG_STATUS),
                )
            } else {
                Span::raw("")
            };
            // Show wrap/list/number indicators on the right.
            let flags = {
                let mut f = String::new();
                if app.config.wrap_lines {
                    f.push_str(" wrap");
                }
                if app.config.show_visible_whitespace {
                    f.push_str(" list");
                }
                f
            };
            let flag_span = if flags.is_empty() {
                Span::raw("")
            } else {
                Span::styled(
                    format!(" │{}", flags),
                    Style::default().fg(theme::FG_STATUS_FLAG).bg(theme::BG_STATUS),
                )
            };
            let mut spans = vec![mode, file, modified, vlf_gap, vlf];
            if let Some(git_span) = git_status_span(app) {
                spans.push(git_span);
            }
            spans.push(buf_indicator);
            spans.push(flag_span);
            spans
        }
    };
    let left_width =
        spans.iter().map(|span| UnicodeWidthStr::width(span.content.as_ref())).sum::<usize>();
    let position_width = UnicodeWidthStr::width(position_text.as_str());
    let status_width = area.width as usize;
    if left_width + position_width < status_width {
        spans.push(Span::styled(
            " ".repeat(status_width - left_width - position_width),
            Style::default().bg(theme::BG_STATUS),
        ));
    }
    spans.push(position);

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::BG_STATUS)),
        area,
    );
}

pub(super) fn git_sign_color(sign: crate::git::GitSign) -> Color {
    match sign {
        crate::git::GitSign::Added => theme::FG_SUCCESS,
        crate::git::GitSign::Modified => theme::FG_WARNING,
        crate::git::GitSign::Deleted => theme::FG_ERROR,
    }
}

pub(super) fn git_status_span(app: &App) -> Option<Span<'static>> {
    if app.backend.active().is_vlf {
        return Some(Span::styled(
            "  git:off(vlf)",
            Style::default().fg(theme::FG_WARNING).bg(theme::BG_STATUS),
        ));
    }

    let status = app.current_git_status()?;
    let dirty = if status.dirty { '*' } else { ' ' };
    Some(Span::styled(
        format!("  git:{}{}", status.branch, dirty),
        Style::default().fg(theme::FG_SUCCESS).bg(theme::BG_STATUS),
    ))
}
