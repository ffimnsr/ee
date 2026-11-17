//! Portable sampling knobs shared by every dialect.
//!
//! The reference agent keeps one protocol-neutral [`GenerationOptionsFields`]
//! record and lets each protocol lower the subset it documents
//! (`packages/ai/src/schema/options.ts:166-196`,
//! `packages/core/src/session/model-request.ts:317-320`). EE does the same: the
//! normalized request carries this struct and the wire codecs decide the field
//! name, so a provider never re-implements sampling.
//!
//! Every field is optional on purpose. An unset knob emits nothing, so a strict
//! provider that rejects unknown or zero-valued sampling fields keeps working
//! with no configuration.
//!
//! [`GenerationOptionsFields`]: https://github.com/anomalyco/opencode

use serde::{Deserialize, Serialize};

use crate::model_metadata::SamplingSupport;

/// Largest accepted number of stop sequences.
///
/// OpenAI-compatible endpoints accept at most four; sending more is a local
/// configuration error rather than a billable provider rejection.
pub const MAX_STOP_SEQUENCES: usize = 4;

/// Longest accepted stop sequence, in bytes.
pub const MAX_STOP_SEQUENCE_BYTES: usize = 256;

/// Sampling and generation knobs for one model call.
///
/// The struct is merged rather than replaced: session defaults come from the
/// provider configuration, and a per-request value wins knob by knob.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GenerationOptions {
    /// Output-token bound (`maxTokens` upstream).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling probability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Top-k sampling cut-off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u64>,
    /// Frequency penalty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    /// Presence penalty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// Deterministic sampling seed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    /// Stop sequences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
}

impl GenerationOptions {
    /// Options with every knob unset.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            seed: None,
            stop: None,
        }
    }

    /// Whether no knob is set, so no body field should be emitted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.max_output_tokens.is_none()
            && self.temperature.is_none()
            && self.top_p.is_none()
            && self.top_k.is_none()
            && self.frequency_penalty.is_none()
            && self.presence_penalty.is_none()
            && self.seed.is_none()
            && self.stop.as_ref().is_none_or(Vec::is_empty)
    }

    /// Returns `self` overlaid on `base`, where `self` wins every knob it sets.
    #[must_use]
    pub fn merged_over(&self, base: &Self) -> Self {
        Self {
            max_output_tokens: self.max_output_tokens.or(base.max_output_tokens),
            temperature: self.temperature.or(base.temperature),
            top_p: self.top_p.or(base.top_p),
            top_k: self.top_k.or(base.top_k),
            frequency_penalty: self.frequency_penalty.or(base.frequency_penalty),
            presence_penalty: self.presence_penalty.or(base.presence_penalty),
            seed: self.seed.or(base.seed),
            stop: self.stop.clone().filter(|stop| !stop.is_empty()).or_else(|| base.stop.clone()),
        }
    }

    /// Validates the documented ranges and the stop-sequence bounds.
    ///
    /// Called by the provider configuration (startup) and by the request
    /// builders (per call), so a bad value never becomes a billable request.
    ///
    /// # Errors
    ///
    /// Returns a message naming the offending knob.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(value) = self.temperature
            && !(0.0..=2.0).contains(&value)
        {
            return Err(format!("temperature {value} is outside the documented 0..=2 range"));
        }
        if let Some(value) = self.top_p
            && !(0.0..=1.0).contains(&value)
        {
            return Err(format!("top_p {value} is outside the documented 0..=1 range"));
        }
        if self.top_k == Some(0) {
            return Err(String::from("top_k must be at least 1"));
        }
        for (name, value) in [
            ("frequency_penalty", self.frequency_penalty),
            ("presence_penalty", self.presence_penalty),
        ] {
            if let Some(value) = value
                && !(-2.0..=2.0).contains(&value)
            {
                return Err(format!("{name} {value} is outside the documented -2..=2 range"));
            }
        }
        if let Some(stop) = &self.stop {
            if stop.len() > MAX_STOP_SEQUENCES {
                return Err(format!(
                    "stop accepts at most {MAX_STOP_SEQUENCES} sequences, got {}",
                    stop.len()
                ));
            }
            for sequence in stop {
                if sequence.is_empty() {
                    return Err(String::from("stop sequences must not be empty"));
                }
                if sequence.len() > MAX_STOP_SEQUENCE_BYTES {
                    return Err(format!("stop sequence exceeds {MAX_STOP_SEQUENCE_BYTES} bytes"));
                }
            }
        }
        Ok(())
    }

    /// Returns the first configured knob the model does not advertise.
    ///
    /// Unset knobs are never reported, so an empty configuration is always
    /// accepted regardless of what the catalog declares.
    #[must_use]
    pub fn unsupported(&self, sampling: SamplingSupport) -> Option<&'static str> {
        if self.temperature.is_some() && !sampling.temperature {
            return Some("temperature");
        }
        if self.top_p.is_some() && !sampling.top_p {
            return Some("top_p");
        }
        if self.top_k.is_some() && !sampling.top_k {
            return Some("top_k");
        }
        None
    }
}

#[cfg(test)]
mod tests;
