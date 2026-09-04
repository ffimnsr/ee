//! Elicitation prompts: field schema, values, secretive-request detection.

use std::collections::{BTreeMap, BTreeSet};

use ee_agent_host::ClientRequestResult;
use ee_agent_protocol::{
    ElicitationAcceptAction, ElicitationAction, ElicitationContentValue, ElicitationPropertySchema,
    ElicitationSchema,
};
use url::Url;

// ── Elicitation prompt ───────────────────────────────────────────────────────

/// One form field rendered as a TUI widget.
#[derive(Debug, Clone)]
pub(crate) struct ElicitationFieldUi {
    pub(crate) name: String,
    pub(crate) title: String,
    #[allow(dead_code)]
    pub(crate) kind: ElicitationFieldKind,
    pub(crate) value: ElicitationFieldValue,
    /// Set when the schema shape is unsupported; the field is read-only and
    /// the prompt reports it visibly.
    pub(crate) unsupported: Option<String>,
    pub(crate) required: bool,
}

impl ElicitationFieldUi {
    /// Rendered label for the field.
    pub(crate) fn label(&self) -> String {
        if self.title.is_empty() { self.name.clone() } else { self.title.clone() }
    }

    /// Rendered current value for the composer/status area.
    pub(crate) fn display_value(&self) -> String {
        match &self.value {
            ElicitationFieldValue::Text(text) => {
                if text.is_empty() {
                    String::from("(empty)")
                } else {
                    text.clone()
                }
            }
            ElicitationFieldValue::Boolean(value) => {
                if *value {
                    String::from("yes")
                } else {
                    String::from("no")
                }
            }
            ElicitationFieldValue::Enum(selected, options) => {
                options.get(*selected).cloned().unwrap_or_else(|| String::from("(invalid)"))
            }
            ElicitationFieldValue::Number(text) => {
                if text.is_empty() {
                    String::from("(empty)")
                } else {
                    text.clone()
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElicitationFieldKind {
    Text,
    Boolean,
    Enum,
    Number,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ElicitationFieldValue {
    Text(String),
    Boolean(bool),
    Enum(usize, Vec<String>),
    Number(String),
}

/// A pending `elicitation/create` request awaiting user input.
#[derive(Debug)]
pub(crate) struct ElicitationPrompt {
    pub(crate) session_id: Option<String>,
    pub(crate) agent_id: String,
    pub(crate) agent_label: String,
    pub(crate) message: String,
    pub(crate) url: Option<String>,
    pub(crate) url_host: Option<String>,
    /// URL-mode `elicitationId` used by `elicitation/complete`.
    pub(crate) completion_id: Option<String>,
    pub(crate) fields: Vec<ElicitationFieldUi>,
    pub(crate) selected_field: usize,
    /// 0 = accept, 1 = decline, 2 = cancel.
    pub(crate) selected_choice: usize,
    /// Response channel; answering resolves the agent's pending request.
    pub(crate) reply: tokio::sync::oneshot::Sender<ClientRequestResult>,
    /// Visible rejection reason for unsupported schema shapes or unsafe requests.
    pub(crate) unsupported_reason: Option<String>,
}

impl ElicitationPrompt {
    /// Maximum number of form fields rendered per elicitation (Phase 7
    /// resource limit); larger schemas fail visibly.
    const MAX_ELICITATION_FIELDS: usize = 24;
    /// Maximum JSON nesting depth accepted for a form schema. ACP v1 schemas
    /// are normally shallow; this caps future/extension payloads before UI use.
    const MAX_ELICITATION_SCHEMA_DEPTH: usize = 12;

    /// Builds a prompt from an ACP form-mode request.
    pub(super) fn from_form(
        session_id: Option<String>,
        agent_id: String,
        agent_label: String,
        message: String,
        schema: &ElicitationSchema,
        reply: tokio::sync::oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        let required: BTreeSet<String> =
            schema.required.clone().unwrap_or_default().into_iter().collect();
        let mut fields = Vec::new();
        let mut unsupported = Vec::new();
        if let Ok(value) = serde_json::to_value(schema)
            && json_depth(&value) > Self::MAX_ELICITATION_SCHEMA_DEPTH
        {
            unsupported
                .push(format!("schema depth exceeds {}", Self::MAX_ELICITATION_SCHEMA_DEPTH));
        }
        let mut over_cap = false;
        for (name, property) in &schema.properties {
            if fields.len() >= Self::MAX_ELICITATION_FIELDS {
                over_cap = true;
                break;
            }
            let field = ElicitationFieldUi::from_property(name, property, &required);
            if let Some(reason) = &field.unsupported {
                unsupported.push(format!("unsupported field {}: {reason}", field.label()));
            }
            fields.push(field);
        }
        if over_cap {
            unsupported.push(format!("too many fields (> {})", Self::MAX_ELICITATION_FIELDS));
        }
        if let Some(reason) = detect_secretive_elicitation_request(&message, &fields) {
            unsupported.push(reason);
        }
        Self {
            session_id,
            agent_id,
            agent_label,
            message,
            url: None,
            url_host: None,
            completion_id: None,
            fields,
            selected_field: 0,
            selected_choice: 0,
            reply,
            unsupported_reason: if unsupported.is_empty() {
                None
            } else {
                Some(unsupported.join(", "))
            },
        }
    }

    /// Builds a prompt from an ACP URL-mode request.
    pub(super) fn from_url(
        session_id: Option<String>,
        agent_id: String,
        agent_label: String,
        message: String,
        completion_id: String,
        url: String,
        reply: tokio::sync::oneshot::Sender<ClientRequestResult>,
    ) -> Self {
        let url_host =
            Url::parse(&url).ok().and_then(|parsed| parsed.host_str().map(str::to_string));
        Self {
            session_id,
            agent_id,
            agent_label,
            message,
            url: Some(url),
            url_host,
            completion_id: Some(completion_id),
            fields: Vec::new(),
            selected_field: 0,
            selected_choice: 0,
            reply,
            unsupported_reason: None,
        }
    }

    /// Builds the response content map for the current field values.
    pub(super) fn content_map(&self) -> Result<BTreeMap<String, ElicitationContentValue>, String> {
        if let Some(reason) = &self.unsupported_reason {
            return Err(reason.clone());
        }
        let mut content = BTreeMap::new();
        for field in &self.fields {
            if field.unsupported.is_some() {
                return Err(format!("unsupported field: {}", field.label()));
            }
            if field.required && field.value.is_empty() {
                return Err(format!("required field missing: {}", field.label()));
            }
            let value = field
                .value
                .to_content()
                .ok_or_else(|| format!("invalid value for {}", field.label()))?;
            content.insert(field.name.clone(), value);
        }
        Ok(content)
    }

    pub(super) fn submit_action(&self, accept: bool) -> ElicitationAction {
        if !accept {
            return ElicitationAction::Cancel;
        }
        match self.selected_choice {
            1 => ElicitationAction::Decline,
            2 => ElicitationAction::Cancel,
            _ => ElicitationAction::Accept(ElicitationAcceptAction::new()),
        }
    }
}

pub(super) fn detect_secretive_elicitation_request(
    message: &str,
    fields: &[ElicitationFieldUi],
) -> Option<String> {
    let mut blocked = Vec::new();
    for field in fields {
        let label = field.label();
        if looks_sensitive_elicitation_text(&field.name) || looks_sensitive_elicitation_text(&label)
        {
            blocked.push(label);
        }
    }
    if looks_sensitive_elicitation_text(message) {
        blocked.push(String::from("request message"));
    }
    blocked.sort();
    blocked.dedup();
    if blocked.is_empty() {
        None
    } else {
        Some(format!("secret-like elicitation requests are blocked: {}", blocked.join(", ")))
    }
}

pub(super) fn looks_sensitive_elicitation_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ee_agent_host::redact::is_secret_key(&lower)
        || [
            "password",
            "passcode",
            "secret",
            "token",
            "credential",
            "api key",
            "apikey",
            "private key",
            "access key",
            "auth code",
            "authorization code",
            "otp",
            "one-time code",
        ]
        .into_iter()
        .any(|needle| lower.contains(needle))
}

pub(super) fn json_depth(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Array(values) => {
            1 + values.iter().map(json_depth).max().unwrap_or_default()
        }
        serde_json::Value::Object(values) => {
            1 + values.values().map(json_depth).max().unwrap_or_default()
        }
        _ => 1,
    }
}

impl ElicitationFieldValue {
    pub(super) fn is_empty(&self) -> bool {
        match self {
            Self::Text(text) | Self::Number(text) => text.trim().is_empty(),
            Self::Boolean(_) | Self::Enum(_, _) => false,
        }
    }

    fn to_content(&self) -> Option<ElicitationContentValue> {
        match self {
            Self::Text(text) => Some(ElicitationContentValue::String(text.clone())),
            Self::Number(text) => {
                let text = text.trim();
                if text.parse::<i64>().is_ok() {
                    text.parse::<i64>().ok().map(ElicitationContentValue::Integer)
                } else {
                    text.parse::<f64>().ok().map(ElicitationContentValue::Number)
                }
            }
            Self::Boolean(value) => Some(ElicitationContentValue::Boolean(*value)),
            Self::Enum(selected, options) => {
                options.get(*selected).cloned().map(ElicitationContentValue::String)
            }
        }
    }
}

impl ElicitationFieldUi {
    /// Converts one ACP form property into a UI field; unsupported shapes
    /// are preserved with a visible reason.
    fn from_property(
        name: &str,
        property: &ElicitationPropertySchema,
        required: &BTreeSet<String>,
    ) -> Self {
        let required = required.contains(name);
        let unsupported = |reason: String| Self {
            name: name.to_string(),
            title: String::new(),
            kind: ElicitationFieldKind::Text,
            value: ElicitationFieldValue::Text(String::new()),
            unsupported: Some(reason),
            required,
        };
        match property {
            ElicitationPropertySchema::String(schema) => {
                if let Some(options) = schema.enum_values.clone()
                    && !options.is_empty()
                {
                    let default = options
                        .iter()
                        .position(|option| schema.default.as_deref() == Some(option.as_str()));
                    Self {
                        name: name.to_string(),
                        title: schema.title.clone().unwrap_or_default(),
                        kind: ElicitationFieldKind::Enum,
                        value: ElicitationFieldValue::Enum(default.unwrap_or(0), options),
                        unsupported: None,
                        required,
                    }
                } else {
                    Self {
                        name: name.to_string(),
                        title: schema.title.clone().unwrap_or_default(),
                        kind: ElicitationFieldKind::Text,
                        value: ElicitationFieldValue::Text(
                            schema.default.clone().unwrap_or_default(),
                        ),
                        unsupported: None,
                        required,
                    }
                }
            }
            ElicitationPropertySchema::Boolean(schema) => Self {
                name: name.to_string(),
                title: schema.title.clone().unwrap_or_default(),
                kind: ElicitationFieldKind::Boolean,
                value: ElicitationFieldValue::Boolean(schema.default.unwrap_or(false)),
                unsupported: None,
                required,
            },
            ElicitationPropertySchema::Number(schema) => Self {
                name: name.to_string(),
                title: schema.title.clone().unwrap_or_default(),
                kind: ElicitationFieldKind::Number,
                value: ElicitationFieldValue::Number(
                    schema.default.map(|value| value.to_string()).unwrap_or_default(),
                ),
                unsupported: None,
                required,
            },
            ElicitationPropertySchema::Integer(schema) => Self {
                name: name.to_string(),
                title: schema.title.clone().unwrap_or_default(),
                kind: ElicitationFieldKind::Number,
                value: ElicitationFieldValue::Number(
                    schema.default.map(|value| value.to_string()).unwrap_or_default(),
                ),
                unsupported: None,
                required,
            },
            ElicitationPropertySchema::Array(_) => {
                unsupported(String::from("multi-select arrays are not supported"))
            }
            // Non-exhaustive upstream; unknown property schemas fail visibly.
            _ => unsupported(String::from("unknown property schema")),
        }
    }
}

impl ElicitationPrompt {
    /// Steps the selected field's value by `delta` (enum/boolean cycling).
    pub(super) fn step_elicitation_field(&mut self, delta: isize) {
        let Some(field) = self.fields.get_mut(self.selected_field) else {
            return;
        };
        match &mut field.value {
            ElicitationFieldValue::Boolean(value) => *value = !*value,
            ElicitationFieldValue::Enum(selected, options) => {
                let count = options.len().max(1) as isize;
                *selected = (*selected as isize + delta).rem_euclid(count) as usize;
            }
            ElicitationFieldValue::Text(_) | ElicitationFieldValue::Number(_) => {
                if self.fields.len() > 1 {
                    // Left/Right on a text field moves to the adjacent field.
                    let count = self.fields.len() as isize;
                    self.selected_field =
                        (self.selected_field as isize + delta).rem_euclid(count) as usize;
                }
            }
        }
    }
}
