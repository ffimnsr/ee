//! User-attachment normalization shared by every provider codec.
//!
//! Attachments arrive as prompt blocks and are normalized once, before the
//! modality guard: text resources gain a stable header, directory resources
//! render a bounded listing, PDFs stay as document payloads for dialects that
//! accept them, duplicate inline images are dropped, and unknown MIME types are
//! replaced by one deterministic notice instead of disappearing silently.
//!
//! The pass never touches the durable transcript; callers normalize the request
//! copy. Size bounding (25 MiB trigger, 15 MiB target) runs afterwards in
//! [`crate::model_metadata::guard_transcript`] so ordering stays deterministic.

use std::collections::BTreeSet;

use crate::model::{ModelContent, ModelMessage};

/// MIME type used by directory attachments.
pub const DIRECTORY_MIME: &str = "application/x-directory";

/// Largest text attachment inlined verbatim.
pub const MAX_INLINE_TEXT_BYTES: usize = 64 * 1024;

/// Largest directory listing rendered, in bytes.
pub const MAX_DIRECTORY_LISTING_BYTES: usize = 4 * 1024;

/// Largest directory listing rendered, in lines.
pub const MAX_DIRECTORY_LISTING_LINES: usize = 200;

/// Normalizes attachments in one request transcript.
///
/// Returns `None` when nothing changed so the caller can keep the transcript
/// reference.
#[must_use]
pub fn normalize_attachments(transcript: &[ModelMessage]) -> Option<Vec<ModelMessage>> {
    let mut rewritten = transcript.to_vec();
    let mut changed = false;
    let mut seen_images: BTreeSet<(String, Option<String>, String)> = BTreeSet::new();
    let mut noticed_mimes: BTreeSet<String> = BTreeSet::new();

    for message in &mut rewritten {
        let mut content = Vec::with_capacity(message.content.len());
        for block in std::mem::take(&mut message.content) {
            match block {
                ModelContent::Image { data, mime_type, uri } => {
                    let key = (mime_type.clone(), uri.clone(), data.clone());
                    if seen_images.insert(key) {
                        content.push(ModelContent::Image { data, mime_type, uri });
                    } else {
                        changed = true;
                    }
                }
                ModelContent::EmbeddedResource { uri, mime_type, text, blob } => {
                    let mime = mime_type.as_deref().unwrap_or("text/plain");
                    if mime == DIRECTORY_MIME {
                        content.push(ModelContent::Text(directory_listing(&uri, text.as_deref())));
                        changed = true;
                        continue;
                    }
                    if mime.starts_with("text/") || (text.is_some() && blob.is_none()) {
                        let body = text.as_deref().unwrap_or_default();
                        content.push(ModelContent::Text(inline_text(&uri, mime, body)));
                        changed = true;
                        continue;
                    }
                    if mime == "application/pdf" {
                        if blob.is_some() {
                            content.push(ModelContent::EmbeddedResource {
                                uri,
                                mime_type,
                                text: None,
                                blob,
                            });
                        } else {
                            content.push(ModelContent::Text(format!(
                                "[pdf omitted: {uri} carried no payload]"
                            )));
                            changed = true;
                        }
                        continue;
                    }
                    if is_supported_media(mime) {
                        content.push(ModelContent::EmbeddedResource { uri, mime_type, text, blob });
                        continue;
                    }
                    changed = true;
                    if noticed_mimes.insert(mime.to_string()) {
                        content.push(ModelContent::Text(format!(
                            "[attachment omitted: {mime} is not a supported attachment type]"
                        )));
                    }
                }
                other => content.push(other),
            }
        }
        message.content = content;
    }

    changed.then_some(rewritten)
}

/// Whether a blob MIME type has a documented media path.
fn is_supported_media(mime: &str) -> bool {
    mime.starts_with("image/")
        || mime.starts_with("audio/")
        || mime.starts_with("video/")
        || mime == "application/pdf"
}

/// Renders a text attachment with a stable header and a bounded body.
fn inline_text(uri: &str, mime: &str, text: &str) -> String {
    let body = truncate_utf8(text, MAX_INLINE_TEXT_BYTES);
    let mut rendered = format!("[file: {uri} ({mime})]\n{body}");
    if body.len() < text.len() {
        rendered.push_str("\n[... attachment truncated ...]");
    }
    rendered
}

/// Renders a bounded directory listing.
fn directory_listing(uri: &str, listing: Option<&str>) -> String {
    let Some(listing) = listing.filter(|listing| !listing.is_empty()) else {
        return format!("[directory: {uri}]\n(listing unavailable)");
    };
    let mut lines: Vec<&str> = listing.lines().take(MAX_DIRECTORY_LISTING_LINES).collect();
    let total = listing.lines().count();
    let mut rendered = format!("[directory: {uri}]\n");
    let mut used = 0usize;
    let mut shown = 0usize;
    for line in lines.drain(..) {
        let cost = line.len().saturating_add(1);
        if used.saturating_add(cost) > MAX_DIRECTORY_LISTING_BYTES {
            break;
        }
        rendered.push_str(line);
        rendered.push('\n');
        used = used.saturating_add(cost);
        shown += 1;
    }
    if shown < total {
        rendered.push_str(&format!("[... {} entries not shown ...]\n", total - shown));
    }
    rendered
}

/// Truncates at a UTF-8 boundary.
fn truncate_utf8(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelMessage, ModelRole};

    fn resource(uri: &str, mime: &str, text: Option<&str>, blob: Option<&str>) -> ModelContent {
        ModelContent::EmbeddedResource {
            uri: uri.into(),
            mime_type: Some(mime.into()),
            text: text.map(str::to_string),
            blob: blob.map(str::to_string),
        }
    }

    #[test]
    fn duplicate_inline_images_are_sent_once() {
        let image = ModelContent::Image {
            data: "AAAA".into(),
            mime_type: "image/png".into(),
            uri: Some("file:///a.png".into()),
        };
        let messages = vec![
            ModelMessage::new(ModelRole::User).with_content(vec![image.clone()]),
            ModelMessage::new(ModelRole::User).with_content(vec![image]),
        ];
        let normalized = normalize_attachments(&messages).expect("rewritten");
        let images = normalized
            .iter()
            .flat_map(|message| message.content.iter())
            .filter(|block| matches!(block, ModelContent::Image { .. }))
            .count();
        assert_eq!(images, 1);
    }

    #[test]
    fn text_attachment_gets_a_stable_header() {
        let messages = vec![ModelMessage::new(ModelRole::User).with_content(vec![resource(
            "file:///notes.md",
            "text/markdown",
            Some("# hi"),
            None,
        )])];
        let normalized = normalize_attachments(&messages).expect("rewritten");
        let ModelContent::Text(text) = &normalized[0].content[0] else {
            panic!("expected text");
        };
        assert!(text.starts_with("[file: file:///notes.md (text/markdown)]\n# hi"));
    }

    #[test]
    fn directory_attachment_renders_a_bounded_listing() {
        let listing =
            (0..500).map(|index| format!("file-{index}.txt")).collect::<Vec<_>>().join("\n");
        let messages = vec![ModelMessage::new(ModelRole::User).with_content(vec![resource(
            "file:///src",
            DIRECTORY_MIME,
            Some(&listing),
            None,
        )])];
        let normalized = normalize_attachments(&messages).expect("rewritten");
        let ModelContent::Text(text) = &normalized[0].content[0] else {
            panic!("expected text");
        };
        assert!(text.starts_with("[directory: file:///src]"));
        assert!(text.contains("entries not shown"));
        assert!(text.len() <= MAX_DIRECTORY_LISTING_BYTES + 200);
    }

    #[test]
    fn unknown_mime_is_dropped_with_one_notice_per_type() {
        let messages = vec![
            ModelMessage::new(ModelRole::User).with_content(vec![resource(
                "file:///a.zip",
                "application/zip",
                None,
                Some("AA=="),
            )]),
            ModelMessage::new(ModelRole::User).with_content(vec![resource(
                "file:///b.zip",
                "application/zip",
                None,
                Some("AA=="),
            )]),
        ];
        let normalized = normalize_attachments(&messages).expect("rewritten");
        let notices: Vec<&str> = normalized
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ModelContent::Text(text) if text.contains("attachment omitted") => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("application/zip"));
    }

    #[test]
    fn pdf_payload_stays_for_dialect_encoders() {
        let messages = vec![ModelMessage::new(ModelRole::User).with_content(vec![resource(
            "file:///scan.pdf",
            "application/pdf",
            None,
            Some("JVBERi0="),
        )])];
        assert!(normalize_attachments(&messages).is_none(), "nothing to rewrite");
    }
}
