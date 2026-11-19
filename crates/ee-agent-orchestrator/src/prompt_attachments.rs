//! Prompt attachments, mention expansion, and deterministic image
//! normalization.
//!
//! Explicit attachments arrive in `_meta.attachments` as workspace paths or
//! base64 `data:` URLs. The same request may reference `@path` files and
//! `@skill:name` skills in prompt text; those are appended as bounded
//! workspace-plane instructions so repository content keeps its untrusted
//! trust label.
//!
//! Paths must resolve inside a workspace root. Anything else fails closed with
//! the external-directory approval message (I21) instead of reading silently.
//! Images are decoded, downscaled to the configured maximum dimensions,
//! re-encoded, and checked against the configured base64 byte cap; formats
//! that cannot be decoded pass through unchanged.

use std::collections::BTreeSet;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use ee_agent_acp_server::PromptContext;
use ee_agent_protocol::{
    BlobResourceContents, ContentBlock, EmbeddedResource, EmbeddedResourceResource, ImageContent,
    TextResourceContents,
};
use image::{GenericImageView, ImageFormat};
use serde::Deserialize;

use crate::model::{ModelMessage, ModelRole};
use crate::prompt::WorkspaceMessage;
use crate::trust::TrustLevel;

/// Default maximum image width or height after normalization.
pub const DEFAULT_MAX_IMAGE_DIMENSION: u32 = 2048;
/// Default maximum base64 payload for one normalized image.
pub const DEFAULT_MAX_IMAGE_BASE64_BYTES: usize = 4 * 1024 * 1024;
/// Default maximum decoded bytes for one attachment.
pub const DEFAULT_MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
/// Default maximum decoded bytes for all attachments in one prompt.
pub const DEFAULT_MAX_TOTAL_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;
/// Default maximum bytes inlined from one `@path` or `@skill:` mention.
pub const DEFAULT_MAX_MENTION_BYTES: usize = 64 * 1024;
/// Default maximum attachments accepted in one prompt.
pub const DEFAULT_MAX_ATTACHMENTS: usize = 16;

/// Attachment and mention limits for one prompt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromptAttachmentConfig {
    /// Whether explicit attachments and mention expansion are enabled.
    pub enabled: bool,
    /// Maximum attachments accepted per prompt.
    pub max_attachments: usize,
    /// Maximum decoded bytes per attachment.
    pub max_attachment_bytes: usize,
    /// Maximum decoded bytes across all attachments in one prompt.
    pub max_total_attachment_bytes: usize,
    /// Maximum bytes inlined from one mention.
    pub max_mention_bytes: usize,
    /// Maximum image width or height after normalization.
    pub max_image_dimension: u32,
    /// Maximum base64 payload for one normalized image.
    pub max_image_base64_bytes: usize,
}

impl Default for PromptAttachmentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_attachments: DEFAULT_MAX_ATTACHMENTS,
            max_attachment_bytes: DEFAULT_MAX_ATTACHMENT_BYTES,
            max_total_attachment_bytes: DEFAULT_MAX_TOTAL_ATTACHMENT_BYTES,
            max_mention_bytes: DEFAULT_MAX_MENTION_BYTES,
            max_image_dimension: DEFAULT_MAX_IMAGE_DIMENSION,
            max_image_base64_bytes: DEFAULT_MAX_IMAGE_BASE64_BYTES,
        }
    }
}

/// Result of preparing one prompt.
#[derive(Debug, Default)]
pub struct PreparedPrompt {
    /// Trusted/untrusted instruction messages for the workspace plane.
    pub instructions: Vec<WorkspaceMessage>,
    /// Number of explicit attachments materialized.
    pub attachment_count: usize,
}

/// Materializes explicit attachments, expands mentions, and normalizes images
/// in `ctx.prompt`.
pub fn prepare_prompt(
    ctx: &mut PromptContext,
    cwd: &Path,
    workspace_roots: &[PathBuf],
    config_root: Option<&Path>,
    config: &PromptAttachmentConfig,
) -> Result<PreparedPrompt, String> {
    let mut prepared = PreparedPrompt::default();
    if !config.enabled {
        return Ok(prepared);
    }
    // Slash commands stay pure: they are parsed before attachments would make
    // sense and must not trigger workspace reads.
    if prompt_is_command(&ctx.prompt) {
        return Ok(prepared);
    }
    let roots = canonical_roots(workspace_roots, cwd);
    let mut total_bytes = 0usize;
    let mut attachments = parse_attachments(ctx.metadata.as_ref())?;
    if attachments.len() > config.max_attachments {
        return Err(format!(
            "prompt carries {} attachments; at most {} are allowed",
            attachments.len(),
            config.max_attachments
        ));
    }
    for attachment in attachments.drain(..) {
        let (path, mime, bytes) = match attachment {
            RawAttachment::Path(path) | RawAttachment::PathSpec { path } => {
                let canonical = resolve_inside_roots(&path, cwd, &roots)?;
                let bytes = fs::read(&canonical).map_err(|error| {
                    format!("cannot read attachment {}: {error}", canonical.display())
                })?;
                let mime = mime_for_path(&canonical);
                (Some(canonical), mime.to_string(), bytes)
            }
            RawAttachment::DataUrl { data_url, media_type } => {
                let (mime, bytes) = parse_data_url(&data_url)?;
                (None, media_type.unwrap_or(mime), bytes)
            }
        };
        total_bytes = total_bytes.saturating_add(bytes.len());
        if bytes.len() > config.max_attachment_bytes {
            return Err(format!(
                "attachment exceeds the {}-byte per-item limit",
                config.max_attachment_bytes
            ));
        }
        if total_bytes > config.max_total_attachment_bytes {
            return Err(format!(
                "attachments exceed the {}-byte per-prompt limit",
                config.max_total_attachment_bytes
            ));
        }
        let block = attachment_block(path.as_deref(), &mime, bytes)?;
        ctx.prompt.push(block);
        prepared.attachment_count += 1;
    }
    prepared.instructions.extend(expand_mentions(
        ctx,
        cwd,
        &roots,
        config_root,
        config.max_mention_bytes,
        &mut total_bytes,
    )?);
    for block in &mut ctx.prompt {
        if let ContentBlock::Image(image) = block {
            normalize_image(image, config)?;
        }
    }
    Ok(prepared)
}

fn prompt_is_command(prompt: &[ContentBlock]) -> bool {
    prompt.iter().any(|block| {
        matches!(block, ContentBlock::Text(text) if text.text.trim_start().starts_with('/'))
    })
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawAttachment {
    Path(String),
    PathSpec {
        path: String,
    },
    DataUrl {
        #[serde(rename = "dataUrl")]
        data_url: String,
        #[serde(default, rename = "mediaType")]
        media_type: Option<String>,
    },
}

fn parse_attachments(
    metadata: Option<&ee_agent_protocol::Meta>,
) -> Result<Vec<RawAttachment>, String> {
    let Some(value) = metadata.and_then(|metadata| metadata.get("attachments")) else {
        return Ok(Vec::new());
    };
    let entries =
        value.as_array().ok_or_else(|| "prompt _meta.attachments must be an array".to_string())?;
    let mut attachments = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        if let Some(object) = entry.as_object() {
            let has_path = object.contains_key("path");
            let has_data_url = object.contains_key("dataUrl");
            if has_path == has_data_url {
                return Err(format!(
                    "attachment {index} must name exactly one of `path` or `dataUrl`"
                ));
            }
            for key in object.keys() {
                if !matches!(key.as_str(), "path" | "dataUrl" | "mediaType") {
                    return Err(format!("attachment {index} has unknown key {key:?}"));
                }
            }
        }
        let parsed: RawAttachment = serde_json::from_value(entry.clone()).map_err(|error| {
            format!(
                "attachment {index} must be a path string, {{path}}, or {{dataUrl}} object: {error}"
            )
        })?;
        attachments.push(parsed);
    }
    Ok(attachments)
}

fn canonical_roots(workspace_roots: &[PathBuf], cwd: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for root in std::iter::once(cwd).chain(workspace_roots.iter().map(PathBuf::as_path)) {
        let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        if !roots.contains(&canonical) {
            roots.push(canonical);
        }
    }
    roots
}

fn resolve_inside_roots(path: &str, cwd: &Path, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let candidate =
        if Path::new(path).is_absolute() { PathBuf::from(path) } else { cwd.join(path) };
    let canonical = fs::canonicalize(&candidate)
        .map_err(|error| format!("cannot resolve attachment {path:?}: {error}"))?;
    if !roots.iter().any(|root| canonical.starts_with(root)) {
        return Err(format!(
            "attachment {path:?} is outside the workspace roots; external directory reads require approval (I21)"
        ));
    }
    Ok(canonical)
}

fn parse_data_url(url: &str) -> Result<(String, Vec<u8>), String> {
    let rest = url
        .strip_prefix("data:")
        .ok_or_else(|| "attachment data URL must start with `data:`".to_string())?;
    let (metadata, payload) = rest
        .split_once(',')
        .ok_or_else(|| "attachment data URL is missing the `,` payload separator".to_string())?;
    if !metadata.split(';').any(|part| part.trim() == "base64") {
        return Err("attachment data URL must be base64-encoded".to_string());
    }
    let mime = metadata
        .split(';')
        .next()
        .filter(|mime| !mime.trim().is_empty())
        .unwrap_or("application/octet-stream")
        .trim()
        .to_string();
    let bytes = BASE64
        .decode(payload.trim())
        .map_err(|error| format!("attachment data URL is not valid base64: {error}"))?;
    Ok((mime, bytes))
}

fn attachment_block(
    path: Option<&Path>,
    mime: &str,
    bytes: Vec<u8>,
) -> Result<ContentBlock, String> {
    if mime.starts_with("image/") {
        return Ok(ContentBlock::Image(
            ImageContent::new(BASE64.encode(&bytes), mime.to_string())
                .uri(path.map(|path| path.display().to_string())),
        ));
    }
    let uri = path
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| format!("attachment://{mime}"));
    if mime.starts_with("text/") || String::from_utf8(bytes.clone()).is_ok() {
        let text = String::from_utf8(bytes)
            .map_err(|_| "text attachment is not valid UTF-8".to_string())?;
        return Ok(ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::TextResourceContents(
                TextResourceContents::new(text, uri).mime_type(mime.to_string()),
            ),
        )));
    }
    Ok(ContentBlock::Resource(EmbeddedResource::new(
        EmbeddedResourceResource::BlobResourceContents(
            BlobResourceContents::new(BASE64.encode(&bytes), uri).mime_type(mime.to_string()),
        ),
    )))
}

fn expand_mentions(
    ctx: &PromptContext,
    cwd: &Path,
    roots: &[PathBuf],
    config_root: Option<&Path>,
    max_mention_bytes: usize,
    total_bytes: &mut usize,
) -> Result<Vec<WorkspaceMessage>, String> {
    let mut messages = Vec::new();
    let mut seen = BTreeSet::new();
    for block in &ctx.prompt {
        let ContentBlock::Text(text) = block else { continue };
        for token in text.text.split_whitespace() {
            let Some(mention) = token.strip_prefix('@') else { continue };
            let mention = mention.trim_end_matches([',', '.', ';', ':', ')', ']', '}']);
            if mention.is_empty() {
                continue;
            }
            if let Some(name) = mention.strip_prefix("skill:") {
                if !seen.insert(format!("skill:{name}")) {
                    continue;
                }
                messages.push(skill_message(name, cwd, roots, config_root, max_mention_bytes)?);
                continue;
            }
            if !mention.contains('/') && !mention.contains('.') {
                // Bare `@name` handles are ordinary text, not file paths.
                continue;
            }
            if !seen.insert(format!("path:{mention}")) {
                continue;
            }
            let canonical = resolve_inside_roots(mention, cwd, roots)?;
            let metadata = fs::metadata(&canonical)
                .map_err(|error| format!("cannot stat mention {mention:?}: {error}"))?;
            if !metadata.is_file() {
                continue;
            }
            if metadata.len() > max_mention_bytes as u64 {
                return Err(format!(
                    "mention {mention:?} exceeds the {max_mention_bytes}-byte mention limit"
                ));
            }
            let bytes = fs::read(&canonical)
                .map_err(|error| format!("cannot read mention {mention:?}: {error}"))?;
            *total_bytes = total_bytes.saturating_add(bytes.len());
            let mime = mime_for_path(&canonical);
            if mime.starts_with("image/") {
                continue;
            }
            let content = String::from_utf8(bytes)
                .map_err(|_| format!("mention {mention:?} is not valid UTF-8"))?;
            messages.push(workspace_message(
                &canonical,
                format!("[file: {}]\n{content}", canonical.display()),
                TrustLevel::WorkspaceInstructionUntrusted,
            ));
        }
    }
    Ok(messages)
}

fn skill_message(
    name: &str,
    cwd: &Path,
    roots: &[PathBuf],
    config_root: Option<&Path>,
    max_bytes: usize,
) -> Result<WorkspaceMessage, String> {
    let skill = crate::prompt::skills::load_skill(Some(name), None, cwd, roots, config_root)
        .map_err(|error| format!("unknown skill {name:?}: {error}"))?;
    let content = if skill.content.len() > max_bytes {
        truncate_bytes(&skill.content, max_bytes)
    } else {
        skill.content
    };
    let trust = match skill.scope {
        crate::prompt::skills::SkillScope::Workspace => TrustLevel::WorkspaceInstructionUntrusted,
        crate::prompt::skills::SkillScope::User => TrustLevel::UserInstructionTrusted,
    };
    Ok(workspace_message(&skill.path, format!("[skill: {}]\n{content}", skill.name), trust))
}

fn workspace_message(path: &Path, text: String, trust: TrustLevel) -> WorkspaceMessage {
    let message = ModelMessage::text(ModelRole::User, text)
        .with_trust(trust)
        .with_metadata([("ee_prompt".to_string(), "mention".to_string())]);
    WorkspaceMessage { path: path.display().to_string(), message }
}

fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    let mut end = 0usize;
    for (index, character) in text.char_indices() {
        if index + character.len_utf8() > max_bytes {
            break;
        }
        end = index + character.len_utf8();
    }
    let mut truncated = text[..end].to_string();
    truncated.push_str("\n[... mention truncated ...]");
    truncated
}

fn mime_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        Some("pdf") => "application/pdf",
        Some("json") => "application/json",
        Some("md" | "markdown") => "text/markdown",
        Some("html" | "htm") => "text/html",
        Some("css") => "text/css",
        Some("js" | "mjs" | "cjs") => "text/javascript",
        Some(
            "ts" | "tsx" | "jsx" | "rs" | "py" | "go" | "java" | "c" | "h" | "cpp" | "hpp" | "toml"
            | "yaml" | "yml" | "sh" | "bash" | "zsh" | "txt" | "log" | "sql",
        ) => "text/plain",
        _ => "application/octet-stream",
    }
}

fn normalize_image(
    image: &mut ImageContent,
    config: &PromptAttachmentConfig,
) -> Result<(), String> {
    let Ok(bytes) = BASE64.decode(&image.data) else {
        // Passthrough: unreadable payloads are left for the provider codec.
        return Ok(());
    };
    if bytes.len() > config.max_attachment_bytes {
        return Err(format!(
            "image exceeds the {}-byte per-item limit",
            config.max_attachment_bytes
        ));
    }
    let Ok(decoded) = image::load_from_memory(&bytes) else {
        // Passthrough: undecodable formats are not silently dropped.
        return Ok(());
    };
    let (width, height) = decoded.dimensions();
    if width <= config.max_image_dimension
        && height <= config.max_image_dimension
        && image.data.len() <= config.max_image_base64_bytes
    {
        return Ok(());
    }
    let mut current = decoded;
    let mut limit = config.max_image_dimension.max(1);
    for _ in 0..4 {
        let scaled = current.resize(limit, limit, image::imageops::FilterType::Lanczos3);
        let mut encoded = Vec::new();
        scaled
            .write_to(&mut Cursor::new(&mut encoded), ImageFormat::Png)
            .map_err(|error| format!("failed to re-encode image: {error}"))?;
        let payload = BASE64.encode(&encoded);
        if payload.len() <= config.max_image_base64_bytes {
            image.data = payload;
            image.mime_type = "image/png".to_string();
            return Ok(());
        }
        current = scaled;
        limit = (limit / 2).max(256);
    }
    Err(format!(
        "image could not be normalized below the {}-byte base64 limit",
        config.max_image_base64_bytes
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_agent_protocol::{SessionId, TextContent};

    fn context(text: &str) -> PromptContext {
        PromptContext::new(SessionId::new("s-1"), vec![ContentBlock::Text(TextContent::new(text))])
    }

    fn png(width: u32, height: u32) -> String {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .expect("encode png");
        BASE64.encode(bytes)
    }

    #[test]
    fn attachment_outside_workspace_roots_fails_closed() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let file = outside.path().join("secret.txt");
        fs::write(&file, "secret").expect("write");
        let mut ctx = context("read this");
        ctx.metadata =
            Some(serde_json::json!({ "attachments": [file] }).as_object().expect("map").clone());
        let error = prepare_prompt(
            &mut ctx,
            workspace.path(),
            &[workspace.path().to_path_buf()],
            None,
            &PromptAttachmentConfig::default(),
        )
        .expect_err("outside path rejected");
        assert!(error.contains("I21"), "unexpected error: {error}");
    }

    #[test]
    fn text_attachment_round_trips_inside_the_workspace() {
        let workspace = tempfile::tempdir().expect("workspace");
        let file = workspace.path().join("notes.md");
        fs::write(&file, "# Notes\nhello").expect("write");
        let mut ctx = context("summarize");
        ctx.metadata = Some(
            serde_json::json!({ "attachments": ["notes.md"] }).as_object().expect("map").clone(),
        );
        let prepared = prepare_prompt(
            &mut ctx,
            workspace.path(),
            &[workspace.path().to_path_buf()],
            None,
            &PromptAttachmentConfig::default(),
        )
        .expect("attachment prepared");
        assert_eq!(prepared.attachment_count, 1);
        assert!(matches!(ctx.prompt.last(), Some(ContentBlock::Resource(_))));
    }

    #[test]
    fn large_image_is_downscaled_and_reencoded() {
        let workspace = tempfile::tempdir().expect("workspace");
        let mut ctx = context("look");
        ctx.prompt.push(ContentBlock::Image(ImageContent::new(png(400, 300), "image/png")));
        let config = PromptAttachmentConfig {
            max_image_dimension: 128,
            max_image_base64_bytes: 512 * 1024,
            ..PromptAttachmentConfig::default()
        };
        prepare_prompt(
            &mut ctx,
            workspace.path(),
            &[workspace.path().to_path_buf()],
            None,
            &config,
        )
        .expect("image normalized");
        let ContentBlock::Image(image) = ctx.prompt.last().expect("image block") else {
            panic!("expected image block");
        };
        let bytes = BASE64.decode(&image.data).expect("valid base64");
        let decoded = image::load_from_memory(&bytes).expect("valid image");
        assert!(decoded.width() <= 128 && decoded.height() <= 128);
        assert_eq!(image.mime_type, "image/png");
    }

    #[test]
    fn skill_mention_is_injected_as_untrusted_workspace_content() {
        let workspace = tempfile::tempdir().expect("workspace");
        let skill = workspace.path().join(".agents").join("skills").join("atlas").join("SKILL.md");
        fs::create_dir_all(skill.parent().expect("parent")).expect("mkdir");
        fs::write(&skill, "---\nname: atlas\ndescription: Atlas\n---\nAtlas body\n")
            .expect("write");
        let mut ctx = context("use @skill:atlas");
        let prepared = prepare_prompt(
            &mut ctx,
            workspace.path(),
            &[workspace.path().to_path_buf()],
            None,
            &PromptAttachmentConfig::default(),
        )
        .expect("skill expanded");
        assert_eq!(prepared.instructions.len(), 1);
        assert_eq!(
            prepared.instructions[0].message.trust,
            TrustLevel::WorkspaceInstructionUntrusted
        );
        assert!(prepared.instructions[0].message.text_content().contains("Atlas body"));
    }

    #[test]
    fn path_mention_is_injected_as_untrusted_workspace_content() {
        let workspace = tempfile::tempdir().expect("workspace");
        let file = workspace.path().join("skills").join("atlas").join("SKILL.md");
        fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        fs::write(&file, "Atlas path body").expect("write");
        let mut ctx = context("read @skills/atlas/SKILL.md");
        let prepared = prepare_prompt(
            &mut ctx,
            workspace.path(),
            &[workspace.path().to_path_buf()],
            None,
            &PromptAttachmentConfig::default(),
        )
        .expect("path mention expanded");
        let message = prepared
            .instructions
            .iter()
            .find(|message| message.message.text_content().contains("Atlas path body"))
            .expect("path content injected");
        assert_eq!(message.message.trust, TrustLevel::WorkspaceInstructionUntrusted);
    }

    #[test]
    fn data_url_attachment_is_materialized() {
        let workspace = tempfile::tempdir().expect("workspace");
        let mut ctx = context("see attached");
        ctx.metadata = Some(
            serde_json::json!({
                "attachments": [{ "dataUrl": format!("data:text/plain;base64,{}", BASE64.encode("hello")) }]
            })
            .as_object()
            .expect("map")
            .clone(),
        );
        let prepared = prepare_prompt(
            &mut ctx,
            workspace.path(),
            &[workspace.path().to_path_buf()],
            None,
            &PromptAttachmentConfig::default(),
        )
        .expect("data URL prepared");
        assert_eq!(prepared.attachment_count, 1);
    }

    #[test]
    fn total_attachment_bytes_are_capped() {
        let workspace = tempfile::tempdir().expect("workspace");
        let file = workspace.path().join("large.txt");
        fs::write(&file, "x".repeat(32)).expect("write");
        let mut ctx = context("read");
        ctx.metadata = Some(
            serde_json::json!({ "attachments": ["large.txt"] }).as_object().expect("map").clone(),
        );
        let config = PromptAttachmentConfig {
            max_total_attachment_bytes: 8,
            ..PromptAttachmentConfig::default()
        };
        let error = prepare_prompt(
            &mut ctx,
            workspace.path(),
            &[workspace.path().to_path_buf()],
            None,
            &config,
        )
        .expect_err("total cap enforced");
        assert!(error.contains("per-prompt limit"));
    }
}
