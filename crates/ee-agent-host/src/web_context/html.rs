//! Web-context module: html.
use super::normalize::normalize_text;
use super::*;

/// Accepts only explicit text-like response MIME types.
pub(super) fn is_html_mime(content_type: &str) -> bool {
    matches!(
        content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase().as_str(),
        "text/html" | "application/xhtml+xml"
    )
}

pub(super) fn is_json_mime(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|content_type| {
        let mime = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
        mime == "application/json" || (mime.starts_with("application/") && mime.ends_with("+json"))
    })
}

pub fn is_text_mime(content_type: Option<&str>) -> bool {
    let Some(content_type) = content_type else {
        return false;
    };
    let mime = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    matches!(
        mime.as_str(),
        "text/plain"
            | "text/html"
            | "text/markdown"
            | "text/x-markdown"
            | "text/json"
            | "text/yaml"
            | "text/x-yaml"
            | "application/json"
            | "application/yaml"
            | "application/x-yaml"
            | "application/xml"
            | "text/xml"
    ) || (mime.starts_with("application/") && mime.ends_with("+json"))
        || (mime.starts_with("application/") && mime.ends_with("+xml"))
}

/// Extracts title and readable text from bounded HTML/XHTML with a small,
/// non-executing tokenizer. Remote text remains untrusted data.
///
/// This deliberately does not attempt browser-compatible rendering. It drops
/// subtrees with executable, interactive, inert, or hidden semantics instead
/// of preserving their text for an agent to interpret as document content.
pub(super) fn extract_html_text(html: &str) -> (Option<String>, String) {
    let mut body = String::new();
    let mut title = String::new();
    let mut suppressed_depth = 0usize;
    let mut suppressed_tags: Vec<String> = Vec::new();
    let mut title_depth = 0usize;
    let mut cursor = 0usize;

    while cursor < html.len() {
        let Some(relative_start) = html[cursor..].find('<') else {
            append_html_text(
                &html[cursor..],
                suppressed_depth == 0,
                title_depth > 0,
                &mut title,
                &mut body,
            );
            break;
        };
        let start = cursor + relative_start;
        append_html_text(
            &html[cursor..start],
            suppressed_depth == 0,
            title_depth > 0,
            &mut title,
            &mut body,
        );
        let Some((end, token)) = next_html_tag(html, start) else {
            // A malformed trailing '<' is document text, never markup.
            append_html_text(
                &html[start..],
                suppressed_depth == 0,
                title_depth > 0,
                &mut title,
                &mut body,
            );
            break;
        };
        cursor = end;
        let Some(tag) = parse_html_tag(token) else {
            continue;
        };
        if tag.is_end {
            if tag.name.eq_ignore_ascii_case("title") && title_depth > 0 {
                title_depth -= 1;
            }
            if let Some(index) =
                suppressed_tags.iter().rposition(|open_tag| open_tag.eq_ignore_ascii_case(tag.name))
            {
                let removed = suppressed_tags.len() - index;
                suppressed_tags.truncate(index);
                suppressed_depth = suppressed_depth.saturating_sub(removed);
            }
            continue;
        }
        if tag.name.eq_ignore_ascii_case("title") && suppressed_depth == 0 && !tag.suppresses_text {
            title_depth += 1;
        }
        if tag.suppresses_text && !tag.self_closing {
            suppressed_depth += 1;
            suppressed_tags.push(tag.name.to_owned());
        }
        if is_html_block_tag(tag.name) {
            body.push(' ');
        }
    }

    let title = normalize_text(&decode_html_entities(&title), MAX_TITLE_BYTES);
    let body = normalize_text(&decode_html_entities(&body), MAX_TEXT_BYTES);
    (!title.is_empty()).then_some(title).map_or((None, body.clone()), |title| (Some(title), body))
}

#[derive(Debug)]
pub(super) struct HtmlTag<'a> {
    pub(super) name: &'a str,
    pub(super) is_end: bool,
    pub(super) self_closing: bool,
    pub(super) suppresses_text: bool,
}

pub(super) fn next_html_tag(html: &str, start: usize) -> Option<(usize, &str)> {
    let bytes = html.as_bytes();
    let mut index = start + 1;
    let mut quote = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(delimiter) = quote {
            if byte == delimiter {
                quote = None;
            }
        } else if matches!(byte, b'\'' | b'\"') {
            quote = Some(byte);
        } else if byte == b'>' {
            return Some((index + 1, &html[start + 1..index]));
        }
        index += 1;
    }
    None
}

pub(super) fn parse_html_tag(token: &str) -> Option<HtmlTag<'_>> {
    let token = token.trim();
    if token.starts_with('!') || token.starts_with('?') {
        return None;
    }
    let (is_end, token) =
        token.strip_prefix('/').map_or((false, token), |token| (true, token.trim_start()));
    let name_end = token
        .find(|character: char| {
            !character.is_ascii_alphanumeric() && character != '-' && character != ':'
        })
        .unwrap_or(token.len());
    let name = token[..name_end].to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    let attributes = &token[name_end..];
    let self_closing = attributes.trim_end().ends_with('/');
    let suppresses_text = is_suppressed_html_tag(&name) || html_attributes_hide_content(attributes);
    // Leak-free normalization: name is converted only to compare then borrowed
    // from the original token after validating ASCII tag syntax.
    let name = &token[..name_end];
    Some(HtmlTag { name, is_end, self_closing, suppresses_text })
}

pub(super) fn is_suppressed_html_tag(name: &str) -> bool {
    [
        "script", "style", "form", "template", "noscript", "textarea", "select", "option",
        "button", "details", "dialog", "input", "output", "iframe", "object", "embed", "canvas",
        "svg",
    ]
    .into_iter()
    .any(|blocked| name.eq_ignore_ascii_case(blocked))
}

pub(super) fn is_html_block_tag(name: &str) -> bool {
    [
        "p", "div", "section", "article", "main", "header", "footer", "li", "br", "tr", "h1", "h2",
        "h3", "h4", "h5", "h6",
    ]
    .into_iter()
    .any(|block| name.eq_ignore_ascii_case(block))
}

pub(super) fn html_attributes_hide_content(attributes: &str) -> bool {
    let compact: String = attributes
        .chars()
        .filter(|character| !character.is_ascii_whitespace() && !matches!(character, '\'' | '\"'))
        .flat_map(char::to_lowercase)
        .collect();
    compact == "hidden"
        || compact == "inert"
        || compact.contains("hidden=")
        || compact.contains("inert=")
        || compact.contains("aria-hidden=true")
        || compact.contains("style=display:none")
        || compact.contains("style=visibility:hidden")
}

pub(super) fn append_html_text(
    value: &str,
    visible: bool,
    title: bool,
    title_text: &mut String,
    body: &mut String,
) {
    if !visible {
        return;
    }
    if title {
        title_text.push_str(value);
    } else {
        body.push_str(value);
    }
}

pub(super) fn decode_html_entities(value: &str) -> String {
    value
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}
